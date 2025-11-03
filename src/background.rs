use crate::{AxiomMetric, CreateEventsPayload, ExtraFields, Url};
use std::{
    fmt::{self, Debug},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::{instrument::WithSubscriber, subscriber::NoSubscriber};

use crate::AxiomMsg;

#[derive(Debug)]
pub struct BadRedirect {
    status: u16,
    to: Url,
}

impl fmt::Display for BadRedirect {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Following such a redirect drops the request body, and will likely
        // give an HTTP 200 response even though nobody ever looked at the POST
        // body.
        //
        // This can e.g. happen for login redirects when you post to a
        // login-protected URL.
        write!(f, "invalid HTTP {} redirect to {}", self.status, self.to)
    }
}

impl std::error::Error for BadRedirect {}

fn report(mut err: &(dyn std::error::Error + 'static)) -> String {
    use std::fmt::Write as _;
    let mut s = format!("{}", err);
    while let Some(src) = err.source() {
        let _ = write!(s, ": {}", src);
        err = src;
    }
    s
}

pub trait Backend {
    type Err: std::fmt::Display;
    type Fut: Future<Output = Result<(), Self::Err>>;

    fn submit_events<'a>(&mut self, payload: CreateEventsPayload<'a>) -> Self::Fut;
}

pub struct ClientBackend {
    pub axiom_endpoint_url: Url,
    pub compression_context: zstd::zstd_safe::CCtx<'static>,
    pub http_client: reqwest::Client,
}

impl ClientBackend {
    pub fn new(axiom_endpoint_url: Url, http_headers: reqwest::header::HeaderMap) -> Self {
        Self {
            axiom_endpoint_url,
            compression_context: zstd::zstd_safe::CCtx::try_create().expect("zstd context allocation to succeed"),
            http_client: reqwest::Client::builder()
                .user_agent(concat!(
                    env!("CARGO_PKG_NAME"),
                    "/",
                    env!("CARGO_PKG_VERSION")
                ))
                .default_headers(http_headers)
                .redirect(reqwest::redirect::Policy::custom(|a| {
                    let status = a.status().as_u16();
                    // the two redirect types that discard the body
                    if status == 302 || status == 303 {
                        let to = a.url().clone();
                        return a.error(BadRedirect { status, to });
                    }
                    // delegate to default impl
                    reqwest::redirect::Policy::default().redirect(a)
                }))
                .build()
                .expect("building TLS backend initialization and loading system configuration to succeed") }
    }
}

impl Backend for ClientBackend {
    type Err = Box<dyn std::error::Error>;
    type Fut = Pin<Box<dyn Future<Output = Result<(), Self::Err>> + Send + 'static>>;

    fn submit_events<'a>(&mut self, payload: CreateEventsPayload<'a>) -> Self::Fut {
        let mut body = Vec::with_capacity(128);
        let mut encoder = zstd::Encoder::with_context(&mut body, &mut self.compression_context);
        let () = serde_json::to_writer(&mut encoder, &payload)
                    .expect("none of the tracing field types can fail to serialize and zstd compression should succeed");
        encoder.finish().expect("zstd compression should succeed");

        let request_builder = self.http_client.post(self.axiom_endpoint_url.clone());
        Box::pin(
            async move {
                let resp = request_builder
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header(reqwest::header::CONTENT_ENCODING, "zstd")
                    .body(body)
                    .send()
                    .await
                    .map_err(|e| report(&e))?;
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.map_err(|e| -> Self::Err {
                        format!("HTTP error {}, decoding body failed: {}", status, e).into()
                    })?;
                    return Err(format!("HTTP error {}: {:#?}", status, body).into());
                }
                Ok(())
            }
            .with_subscriber(NoSubscriber::default()),
        )
    }
}

struct BackgroundTaskInner<B> {
    receiver: mpsc::Receiver<Option<AxiomMsg>>,
    extra_fields: ExtraFields,
    backend: B,
    backoff_count: u32,
    quitting: bool,
    // we can't recv into inflight_reqs because the buffer must hold `Option`s
    recv_buf: Vec<Option<AxiomMsg>>,
    inflight_reqs: Vec<AxiomMsg>,
}

struct InflightState<F> {
    send_task: F,
}

struct BackingOffState {
    task: Pin<Box<tokio::time::Sleep>>,
}

impl<B: Backend + Unpin> BackgroundTaskInner<B>
where
    <B as Backend>::Fut: Unpin,
{
    /// whether the send queue should be dropped, and the backoff duration
    fn backoff_time(&self) -> (bool, Duration) {
        let backoff_time = if self.backoff_count >= 1 {
            Duration::from_millis(
                500u64
                    .checked_shl(self.backoff_count - 1)
                    .unwrap_or(u64::MAX),
            )
        } else {
            Duration::from_millis(0)
        };
        (
            backoff_time >= Duration::from_secs(30),
            std::cmp::min(backoff_time, Duration::from_secs(600)),
        )
    }

    fn poll_idle(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> (Poll<()>, Option<State<<B as Backend>::Fut>>) {
        // invariant: no `assert!` in production
        debug_assert!(
            self.inflight_reqs.is_empty(),
            "poll_idle called with {} inflight reqs",
            self.inflight_reqs.len()
        );
        if !self.inflight_reqs.is_empty() {
            let new_state = self.transition_inflight();
            cx.waker().wake_by_ref();
            return (Poll::Pending, Some(new_state));
        }

        let _default_guard = tracing::subscriber::set_default(NoSubscriber::default());

        // nothing is inflight, can safely quit
        if self.quitting {
            return (Poll::Ready(()), None);
        }

        // can't mutably borrow this and another field at the same time, so move out
        let mut recvd = std::mem::take(&mut self.recv_buf);
        match Pin::new(&mut self.receiver).poll_recv_many(cx, &mut recvd, 1024) {
            Poll::Pending => {
                self.recv_buf = recvd;
                return (Poll::Pending, None);
            }
            Poll::Ready(0) => {
                // channel closed (since recv limit is nonzero)
                self.quitting = true;
                if recvd.is_empty() {
                    self.recv_buf = recvd;
                    return (Poll::Ready(()), None);
                }
            }
            Poll::Ready(1..) => {}
        }

        {
            let msgs_iter = recvd.drain(..);
            for msg in msgs_iter {
                match msg {
                    Some(event) => self.inflight_reqs.push(event),
                    None => {
                        // explicit close
                        self.quitting = true;
                        break; // drop following events
                    }
                }
            }
        }
        self.recv_buf = recvd;

        if !self.inflight_reqs.is_empty() {
            let new_state = self.transition_inflight();
            cx.waker().wake_by_ref();
            return (Poll::Pending, Some(new_state));
        }

        // no requests to send
        if self.quitting {
            (Poll::Ready(()), None)
        } else {
            (Poll::Pending, None)
        }
    }

    fn transition_inflight(mut self: Pin<&mut Self>) -> State<<B as Backend>::Fut> {
        let Self {
            extra_fields,
            backend,
            inflight_reqs,
            ..
        } = &mut *self;
        let fut = backend.submit_events(CreateEventsPayload {
            events: inflight_reqs,
            extra_fields,
        });
        State::Inflight(InflightState { send_task: fut })
    }

    fn poll_inflight(
        mut self: Pin<&mut Self>,
        state: &mut InflightState<B::Fut>,
        cx: &mut Context<'_>,
    ) -> (Poll<()>, Option<State<<B as Backend>::Fut>>) {
        let mut default_guard = tracing::subscriber::set_default(NoSubscriber::default());

        let res = match Pin::new(&mut state.send_task).poll(cx) {
            Poll::Pending => return (Poll::Pending, None),
            Poll::Ready(res) => res,
        };
        match res {
            Ok(()) => {
                self.backoff_count = 0; // reset backoff count when success 
                let new_state = self.transition_idle();
                cx.waker().wake_by_ref();
                (Poll::Pending, Some(new_state))
            }
            Err(e) => {
                let (drop_outstanding, backoff_time) = self.backoff_time();
                drop(default_guard);
                tracing::error!(
                    error_count = self.backoff_count + 1,
                    ?backoff_time,
                    error = %e,
                    "couldn't send logs to axiom",
                );
                default_guard = tracing::subscriber::set_default(NoSubscriber::default());
                if drop_outstanding {
                    let num_dropped: usize = self.inflight_reqs.len();
                    self.inflight_reqs.clear();
                    drop(default_guard);
                    tracing::error!(
                        num_dropped,
                        "dropped outstanding messages due to sending errors",
                    );
                    #[allow(unused_assignments)]
                    {
                        default_guard = tracing::subscriber::set_default(NoSubscriber::default());
                    }
                }
                let new_state = self.transition_backoff(backoff_time);
                cx.waker().wake_by_ref();
                (Poll::Pending, Some(new_state))
            }
        }
    }

    fn transition_idle(mut self: Pin<&mut Self>) -> State<<B as Backend>::Fut> {
        self.inflight_reqs.clear();
        State::Idle
    }

    fn transition_backoff(
        mut self: Pin<&mut Self>,
        backoff_time: Duration,
    ) -> State<<B as Backend>::Fut> {
        self.backoff_count += 1;
        State::BackingOff(BackingOffState {
            task: Box::pin(tokio::time::sleep(backoff_time)),
        })
    }

    fn poll_backoff(
        self: Pin<&mut Self>,
        state: &mut BackingOffState,
        cx: &mut Context<'_>,
    ) -> (Poll<()>, Option<State<<B as Backend>::Fut>>) {
        let _default_guard = tracing::subscriber::set_default(NoSubscriber::default());

        match Pin::new(&mut state.task).poll(cx) {
            Poll::Pending => (Poll::Pending, None),
            Poll::Ready(()) => {
                let new_state = self.transition_inflight();
                cx.waker().wake_by_ref();
                (Poll::Pending, Some(new_state))
            }
        }
    }
}

enum State<F> {
    Idle,
    Inflight(InflightState<F>),
    BackingOff(BackingOffState),
}

impl<F> State<F> {
    fn initial() -> Self {
        Self::Idle
    }
}

pub struct BackgroundTaskFut<B: Backend>(BackgroundTaskInner<B>, State<B::Fut>);

impl BackgroundTaskFut<ClientBackend> {
    pub fn new(
        axiom_endpoint_url: Url,
        http_headers: reqwest::header::HeaderMap,
        extra_fields: ExtraFields,
        receiver: mpsc::Receiver<Option<AxiomMsg>>,
    ) -> Self {
        Self::new_with_backend(
            extra_fields,
            ClientBackend::new(axiom_endpoint_url, http_headers),
            receiver,
        )
    }
}

impl<B: Backend> BackgroundTaskFut<B> {
    pub fn new_with_backend(
        extra_fields: ExtraFields,
        backend: B,
        receiver: mpsc::Receiver<Option<AxiomMsg>>,
    ) -> Self {
        Self(
            BackgroundTaskInner {
                receiver,
                extra_fields,
                backend,
                backoff_count: 0,
                quitting: false,
                recv_buf: Vec::new(),
                inflight_reqs: Vec::new(),
            },
            State::initial(),
        )
    }
}

impl<B: Backend + Unpin> Future for BackgroundTaskFut<B>
where
    B: Unpin,
    B::Fut: Unpin,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Self(inner, curr_state) = &mut *self;
        let (poll, sv) = match curr_state {
            State::Idle => Pin::new(inner).poll_idle(cx),
            State::Inflight(state) => Pin::new(inner).poll_inflight(state, cx),
            State::BackingOff(state) => Pin::new(inner).poll_backoff(state, cx),
        };
        if let Some(new_state) = sv {
            *curr_state = new_state;
        };
        poll
    }
}

pub type BackgroundTask = BackgroundTaskFut<ClientBackend>;

/// Handle to cleanly shut down the `BackgroundTask`.
///
/// It'll still try to send all available data and then quit.
#[derive(Clone)]
pub struct BackgroundTaskController {
    sender: mpsc::Sender<Option<AxiomMsg>>,
}

impl BackgroundTaskController {
    pub fn new(sender: mpsc::Sender<Option<AxiomMsg>>) -> Self {
        Self { sender }
    }

    /// Shut down the associated `BackgroundTask`.
    pub async fn shutdown(&self) {
        // Ignore the error. If no one is listening, it already shut down.
        let _ = self.sender.send(None).await;
    }

    /// Shut down the associated `BackgroundTask`. Panics if called within an asynchronous
    /// execution context.
    pub fn shutdown_blocking(&self) {
        let _ = self.sender.blocking_send(None);
    }

    pub async fn export_metrics(
        &self,
        metric: AxiomMetric,
    ) -> Result<(), mpsc::error::SendError<Option<AxiomMsg>>> {
        self.sender.send(Some(AxiomMsg::Metric(metric))).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttributeField, AxiomEvent, AxiomMsg, EventField, OtelField, ResourceField};
    use crate::{CreateEventsPayload, SpanId, builder::DEFAULT_CHANNEL_SIZE};
    use axum::{
        Json, Router,
        extract::{Path, Request, State},
        http::header::HeaderMap,
        middleware::{self, Next},
        response::{IntoResponse, Response},
        routing::post,
    };
    use reqwest::StatusCode;
    use serde::Deserialize;
    use serde_json::json;
    use std::{
        borrow::Cow,
        collections::{HashMap, hash_map::Entry},
        io::Cursor,
        num::NonZeroU64,
        sync::{Arc, Mutex, RwLock},
        task::{RawWaker, RawWakerVTable, Waker},
    };
    use time::OffsetDateTime;
    use tracing::Level;
    use tracing_mock::expect;
    use tracing_subscriber::{Layer, filter, layer::SubscriberExt};

    struct RecorderBackend {
        events: Vec<AxiomMsg>,
        induce_failure: bool,
    }

    impl RecorderBackend {
        fn new() -> Self {
            Self {
                events: Vec::new(),
                induce_failure: false,
            }
        }

        fn new_induce_failure() -> Self {
            Self {
                events: Vec::new(),
                induce_failure: true,
            }
        }
    }

    impl Backend for RecorderBackend {
        type Err = Box<dyn std::error::Error>;
        type Fut = Pin<Box<dyn Future<Output = Result<(), Self::Err>>>>;

        fn submit_events<'a>(&mut self, payload: CreateEventsPayload<'a>) -> Self::Fut {
            let ret = if self.induce_failure {
                Err(Box::<dyn std::error::Error>::from("test error"))
            } else {
                self.events.append(&mut payload.events.clone());
                Ok(())
            };
            Box::pin(async move { ret })
        }
    }

    fn new_event(span_id: Option<u64>) -> AxiomEvent {
        AxiomEvent {
            otel: OtelField {
                time: OffsetDateTime::now_utc(),
                span_id: span_id.map(|i| SpanId::from(NonZeroU64::new(i).unwrap())),
                trace_id: None,
                parent_span_id: None,
                duration_ms: None,
                kind: "client",
                name: Cow::Borrowed("name"),
            },
            attributes: AttributeField {
                annotation_type: None,
                busy_ns: None,
                idle_ns: None,
                target: Cow::Borrowed("target"),
            },
            resources: ResourceField { service_name: None },
            events: EventField {
                event_name: Cow::Borrowed("event name"),
                level: "INFO",
                event_field: Default::default(),
            },
        }
    }

    fn dummy_raw_waker() -> RawWaker {
        fn clone(_: *const ()) -> RawWaker {
            dummy_raw_waker()
        }
        fn wake(_: *const ()) {}
        fn wake_by_ref(_: *const ()) {}
        fn drop(_: *const ()) {}

        let vtable = &RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        RawWaker::new(std::ptr::null(), vtable)
    }

    fn dummy_waker() -> Waker {
        unsafe { Waker::from_raw(dummy_raw_waker()) }
    }

    #[test]
    fn task_poll_submit() {
        use super::State;
        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);
        let (sender, receiver) = mpsc::channel(DEFAULT_CHANNEL_SIZE);
        let mut task = BackgroundTaskFut::new_with_backend(
            ExtraFields::new(),
            RecorderBackend::new(),
            receiver,
        );
        debug_assert!(matches!(task.1, State::Idle));

        let evt = new_event(Some(1234));
        sender
            .blocking_send(Some(AxiomMsg::Event(evt.clone())))
            .unwrap();
        sender
            .blocking_send(Some(AxiomMsg::Event(evt.clone())))
            .unwrap();

        // Idle => Inflight
        debug_assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Pending);
        debug_assert!(matches!(task.1, State::Inflight(_)));
        debug_assert_eq!(task.0.inflight_reqs.len(), 2);

        // add to queue while task is not processing new events
        sender
            .blocking_send(Some(AxiomMsg::Event(AxiomEvent {
                otel: OtelField {
                    span_id: Some(SpanId::from(NonZeroU64::new(1).unwrap())),
                    ..evt.otel.clone()
                },
                ..evt.clone()
            })))
            .unwrap();

        // send_task completes with Ok => idle
        debug_assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Pending);
        debug_assert!(matches!(task.1, State::Idle));
        debug_assert_eq!(task.0.backend.events.len(), 2);
        debug_assert_eq!(task.0.backend.events[0], AxiomMsg::Event(evt.clone()));
        debug_assert_eq!(task.0.backend.events[1], AxiomMsg::Event(evt));

        debug_assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Pending);
        debug_assert!(matches!(task.1, State::Inflight(_)));
        debug_assert_eq!(task.0.backend.events.len(), 3);
        debug_assert_eq!(
            task.0.backend.events[2].as_event().unwrap().otel.span_id,
            Some(SpanId::from(NonZeroU64::new(1).unwrap()))
        );

        debug_assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Pending);
        debug_assert!(matches!(task.1, State::Idle));
        debug_assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Pending);
        debug_assert!(matches!(task.1, State::Idle));

        sender.blocking_send(None).unwrap();
        debug_assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Ready(()));
        debug_assert!(matches!(task.1, State::Idle));
    }

    #[test]
    fn task_drop_channel_quits() {
        use super::State;
        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);
        let (sender, receiver) = mpsc::channel(DEFAULT_CHANNEL_SIZE);
        let mut task = BackgroundTaskFut::new_with_backend(
            ExtraFields::new(),
            RecorderBackend::new(),
            receiver,
        );
        debug_assert!(matches!(task.1, State::Idle));

        drop(sender);
        debug_assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Ready(()));
    }

    // need a runtime to even be able to construct tokio::time::sleep() futures, and
    // making that part modular is more complexity than its worth
    #[tokio::test]
    async fn task_poll_retry() {
        use super::State;
        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);
        let (sender, receiver) = mpsc::channel(DEFAULT_CHANNEL_SIZE);
        let mut task = BackgroundTaskFut::new_with_backend(
            ExtraFields::new(),
            RecorderBackend::new_induce_failure(),
            receiver,
        );
        debug_assert!(matches!(task.1, State::Idle));

        let evt = new_event(Some(1234));
        sender
            .send(Some(AxiomMsg::Event(evt.clone())))
            .await
            .unwrap();
        sender
            .send(Some(AxiomMsg::Event(evt.clone())))
            .await
            .unwrap();

        debug_assert_eq!(
            task.0.backend.events.len(),
            0,
            "no events processed until poll"
        );
        assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Pending);
        assert!(matches!(task.1, State::Inflight(_)));

        assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Pending);
        match &mut task.1 {
            State::BackingOff(BackingOffState { task }) => task.await,
            _ => panic!("expected task to be in BackingOff state"),
        }
        debug_assert_eq!(
            task.0.backend.events.len(),
            0,
            "events fail to submit due to induce_failure"
        );

        task.0.backend.induce_failure = false;
        debug_assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Pending);
        debug_assert!(matches!(task.1, State::Inflight(_)));

        debug_assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Pending);
        debug_assert!(matches!(task.1, State::Idle));
        debug_assert_eq!(
            task.0.backend.events.len(),
            2,
            "events submit successfully after induce_failure disabled"
        );

        drop(sender);
        debug_assert_eq!(Pin::new(&mut task).poll(&mut cx), Poll::Ready(()));
    }

    const MOCK_API_KEY: &str = "xxx-testing-api-key-xxx";
    const TESTING_HEADER_NAME: &str = "x-tested-header";
    const TESTING_HEADER_VALUE: &str = "tested-header-value";
    const TEST_EXTRA_FIELD_NAME: &str = "test.extra_field";
    const TEST_EXTRA_FIELD_VALUE: &str = "extra_field_val";
    const TESTING_DATASET: &str = "testdataset";

    type ApiEvent = HashMap<String, serde_json::Value>;
    type Dataset = Vec<ApiEvent>;

    #[derive(Clone, Debug, Deserialize, serde::Serialize)]
    struct WrappedEvent {
        #[serde(default, rename = "_time")]
        time: String,
        #[serde(default)]
        events: ApiEvent,
        #[serde(skip_serializing_if = "Option::is_none")]
        attributes: Option<ApiEvent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        resources: Option<ApiEvent>,
        #[serde(flatten)]
        top_level: ApiEvent,
    }

    #[derive(Clone, Debug, Deserialize, serde::Serialize)]
    struct WrappedMetric {
        #[serde(default, rename = "_time")]
        time: String,
        metrics: ApiEvent,
        description: Option<ApiEvent>,
    }

    #[derive(Clone, Debug, Deserialize, serde::Serialize)]
    #[serde(untagged)]
    enum WrappedMsg {
        Event(WrappedEvent),
        Metric(WrappedMetric),
    }

    impl WrappedMsg {
        fn as_event(&self) -> Option<&WrappedEvent> {
            if let WrappedMsg::Event(event) = self {
                Some(event)
            } else {
                None
            }
        }
    }

    type DatasetPayload = Vec<WrappedMsg>;

    #[derive(Debug, Clone)]
    struct AppState {
        datasets: Arc<RwLock<HashMap<String, Dataset>>>,
        req_count: Arc<Mutex<u8>>,
    }

    impl AppState {
        fn new() -> Self {
            Self {
                datasets: Arc::new(RwLock::new(HashMap::new())),
                req_count: Arc::new(Mutex::new(0)),
            }
        }
    }

    async fn middleware_auth_with_mock_key(request: Request, next: Next) -> Response {
        let api_key = match request.headers().get(reqwest::header::AUTHORIZATION) {
            None => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "missing API key header"})),
                )
                    .into_response();
            }
            Some(api_key) => api_key,
        };
        let mock_api_header_value = format!("Bearer {}", &MOCK_API_KEY);
        if api_key != mock_api_header_value.as_str() {
            return (StatusCode::FORBIDDEN, "invalid API key").into_response();
        }
        next.run(request).await
    }

    async fn middleware_zstd_decompress(request: Request, next: Next) -> Response {
        debug_assert_eq!(
            request
                .headers()
                .get(reqwest::header::CONTENT_ENCODING)
                .and_then(|v| str::from_utf8(v.as_bytes()).ok()),
            Some("zstd")
        );

        let (mut parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, 10 * 1024 * 1024).await.unwrap();

        let new_body = match zstd::decode_all(Cursor::new(bytes.to_vec())) {
            Err(e) => {
                tracing::error!(%e, "zstd decoding failed");
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "failed to decompress request body"})),
                )
                    .into_response();
            }
            Ok(b) => b,
        };
        parts.headers.remove(reqwest::header::CONTENT_ENCODING);
        let req = Request::from_parts(parts, new_body.into());

        next.run(req).await
    }

    async fn post_create_events(
        State(state): State<AppState>,
        headers: HeaderMap,
        Path(dataset): Path<String>,
        Json(payload): Json<DatasetPayload>,
    ) -> Response {
        debug_assert_eq!(
            headers
                .get(TESTING_HEADER_NAME)
                .and_then(|v| v.to_str().ok()),
            Some(TESTING_HEADER_VALUE),
            "missing or incorrect extra HTTP header"
        );

        if headers.get("x-induce-error").is_some() {
            let mut req_count = state.req_count.lock().unwrap();
            *req_count += 1;
            if *req_count == 1 {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "induced error"})),
                )
                    .into_response();
            }
        }

        payload.iter().for_each(|evt| {
            let nested_fields: [(&str, Option<&ApiEvent>); 4] = [
                ("events", Some(&evt.as_event().unwrap().events)),
                ("attributes", evt.as_event().unwrap().attributes.as_ref()),
                ("resources", evt.as_event().unwrap().resources.as_ref()),
                ("top_level", Some(&evt.as_event().unwrap().top_level)),
            ];

            for (name, fields) in nested_fields {
                if let Some(field) = fields {
                    for (k, v) in field {
                        // default Deserialize trait will collect this into top level
                        if k.contains("extra_fields") {
                            continue;
                        }

                        debug_assert!(
                            !matches!(
                                v,
                                serde_json::Value::Array(_) | serde_json::Value::Object(_)
                            ),
                            "{} must be depth 1: {} = {}",
                            name,
                            k,
                            v
                        );
                    }
                }
            }
        });

        match state.datasets.write().unwrap().entry(dataset.to_string()) {
            Entry::Vacant(e) => {
                e.insert(
                    payload
                        .into_iter()
                        .map(|ev| {
                            let evt = ev.as_event().expect("not an event").clone();
                            let mut merged = evt.top_level;
                            merged.extend(evt.resources.unwrap());
                            merged.extend(evt.attributes.unwrap());
                            merged.extend(evt.events);
                            merged
                        })
                        .collect::<Vec<_>>(),
                );
            }
            Entry::Occupied(mut e) => {
                e.get_mut().extend(
                    payload
                        .into_iter()
                        .map(|e| e.as_event().unwrap().clone().events),
                );
            }
        };

        Json(json!({"status": 200})).into_response()
    }

    fn build_mock_server() -> (AppState, Router<()>) {
        let state = AppState::new();
        (
            state.clone(),
            Router::new()
                .route(
                    "/v1/datasets/{dataset_name}/ingest",
                    post(post_create_events),
                )
                .layer(middleware::from_fn(middleware_zstd_decompress))
                .layer(middleware::from_fn(middleware_auth_with_mock_key))
                .with_state(state),
        )
    }

    async fn run_mock_server() -> (String, AppState, impl Future<Output = ()>) {
        let (state, app) = build_mock_server();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_host = format!("http://{}", listener.local_addr().unwrap());
        let serve_task = async { axum::serve(listener, app).await.unwrap() };
        (api_host, state, serve_task)
    }

    #[tokio::test]
    async fn end_to_end_submit() {
        let _default = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(tracing_subscriber::fmt::layer()),
        );
        let (api_host, state, serve_task) = run_mock_server().await;
        let (layer, bg_task, bg_task_controller) = crate::builder(MOCK_API_KEY)
            .extra_field(
                TEST_EXTRA_FIELD_NAME.into(),
                Cow::Borrowed(TEST_EXTRA_FIELD_VALUE).into(),
            )
            .service_name("test-service-name".into())
            .http_header(TESTING_HEADER_NAME, TESTING_HEADER_VALUE)
            .unwrap()
            .build(&api_host, TESTING_DATASET)
            .unwrap();
        let _server_join_handle = tokio::spawn(serve_task.with_current_subscriber());
        let bg_join_handle = tokio::spawn(bg_task.with_current_subscriber());

        let subscriber = tracing_subscriber::registry()
            .with(layer)
            .with(tracing_subscriber::fmt::layer());
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::span!(target: "span-test-target", tracing::Level::WARN, "span name", span.field = 40);
            let _enter = span.enter();
            tracing::event!(name: "test-name", target: "test-target", tracing::Level::INFO, event.field = 41, "end-to-end submit event message");
        });
        // flush all of the events
        bg_task_controller.shutdown().await;
        bg_join_handle
            .await
            .unwrap_or_else(|_| eprintln!("background task panicked"));

        // get an exclusive copy so that we don't have to go through the lock all the time
        let datasets = state.datasets.read().unwrap().clone();

        debug_assert_eq!(datasets.len(), 1, "expected single dataset");
        let test_dataset = datasets
            .get(TESTING_DATASET)
            .expect("single dataset to be testing dataset");
        debug_assert_eq!(
            test_dataset.len(),
            2,
            "expected exactly one event and one span close"
        );
        let log_event = &test_dataset[0];
        debug_assert_eq!(log_event.get("event.field"), Some(&json!(41)));
        debug_assert_eq!(log_event.get("span.field"), Some(&json!(40)));
        debug_assert_eq!(log_event.get("level"), Some(&json!("info")));
        debug_assert_eq!(log_event.get("target"), Some(&json!("test-target")));
        debug_assert_eq!(log_event.get("event_name"), Some(&json!("test-name")));
        let trace_id = log_event.get("trace_id").unwrap();

        let span_event = &test_dataset[1];
        debug_assert_eq!(span_event.get("event.field"), None);
        debug_assert_eq!(span_event.get("span.field"), Some(&json!(40)));
        debug_assert_eq!(span_event.get("level"), Some(&json!("warn")));
        debug_assert_eq!(span_event.get("target"), Some(&json!("span-test-target")));
        debug_assert_eq!(span_event.get("event_name"), Some(&json!("span name")));
        debug_assert!(span_event.get("span_id").is_some());
        debug_assert_eq!(
            log_event.get("parent_span_id"),
            Some(span_event.get("span_id").unwrap())
        );
        debug_assert_eq!(span_event.get("trace_id"), Some(trace_id));
    }

    #[tokio::test]
    async fn end_to_end_retry() {
        let (mock_layer, handle) = tracing_mock::layer::mock()
            .event(
                expect::event()
                    .at_level(Level::ERROR)
                    .with_fields(expect::msg("couldn't send logs to axiom"))
                    .with_fields(expect::field("error")), // the underlying reqwest error
            )
            .run_with_handle();

        let crate_name = module_path!().split("::").next().unwrap();
        let filter = filter::Targets::new()
            .with_target(format!("{}::background", crate_name), Level::INFO)
            .with_default(filter::LevelFilter::OFF);
        let _default = tracing::subscriber::set_default(
            tracing_subscriber::registry()
                .with(tracing_subscriber::fmt::layer())
                /* comment out next line to reduce stdout spam during test */
                .with(mock_layer.with_filter(filter)),
        );

        let (api_host, state, serve_task) = run_mock_server().await;
        let (honey_layer, bg_task, bg_task_controller) = crate::builder(MOCK_API_KEY)
            .extra_field(
                TEST_EXTRA_FIELD_NAME.into(),
                Cow::Borrowed(TEST_EXTRA_FIELD_VALUE).into(),
            )
            .service_name("test-service-name".into())
            .http_header(TESTING_HEADER_NAME, TESTING_HEADER_VALUE)
            .unwrap()
            .http_header("x-induce-error", "1")
            .unwrap()
            .build(&api_host, TESTING_DATASET)
            .unwrap();

        let _server_join_handle = tokio::spawn(serve_task.with_current_subscriber());
        let honey_bg_join_handle = tokio::spawn(bg_task.with_current_subscriber());

        let subscriber = tracing_subscriber::registry()
            .with(honey_layer)
            .with(tracing_subscriber::fmt::layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::event!(Level::INFO, "end-to-end retry event message");
        });

        // since (for now) we don't continue retrying after shutdown signal, give some
        // time for client to retry
        for _ in 0..4 {
            if *state.req_count.lock().unwrap() > 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // flush all of the events
        bg_task_controller.shutdown().await;
        honey_bg_join_handle.await.unwrap();
        handle.assert_finished();

        debug_assert_eq!(
            *state.req_count.lock().unwrap(),
            2,
            "expected induce error test to take exactly 2 request to suceed",
        );
        // get an exclusive copy so that we don't have to go through the lock all the time
        let datasets = state.datasets.read().unwrap().clone();
        debug_assert_eq!(datasets.len(), 1, "expected dataset to be recorded");
        debug_assert_eq!(
            datasets.values().next().unwrap().len(),
            1,
            "expected single event in dataset"
        );
    }
}
