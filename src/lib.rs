//! # tracing-axiom
//!
//! [Axiom.co](https://axiom.co) backend for the tracing crate.
//!
//! ## Usage
//!
//! Assumptions:
//! - `tokio` async runtime.
//! - `data` field configured as a map field in your Axiom dataset.
//! - `base_url` set to your org's Axiom edge deployment base domain:
//!   <https://axiom.co/docs/reference/regions>
//! - `api_key` set per Axiom ingest auth docs:
//!   <https://axiom.co/docs/restapi/ingest>
//!
//! ```rs
//! let axiom: tracing_axiom::Axiom =
//!     tracing_axiom::init(tracing_axiom::Config {
//!         evt_que_len: 4 << 10,
//!         met_que_len: 4 << 10,
//!         service_name: "example-service",
//!         base_url: "https://us-east-1.aws.edge.axiom.co".parse().unwrap(),
//!         api_key: &api_key,
//!         datasets: tracing_axiom::DatasetIds::All {
//!             event_dataset_id: "example-dataset",
//!             metric_dataset_id: "example-dataset",
//!         },
//!         collect_target: 4 << 10,
//!         collect_timeout: std::time::Duration::from_millis(500),
//!         sender_pool_size: 1,
//!     });
//!
//! // NOTE: can clone `axiom.evt_tx` and send custom events to it as long as they
//! //       implement `serde::Serialize`.
//!
//! let subscriber = tracing_subscriber::registry()
//!     .with(tracing_subscriber::fmt::layer())
//!     .with(tracing_axiom::layer(axiom.evt_tx.clone().downgrade()));
//! tracing::subscriber::set_global_default(subscriber).unwrap();
//!
//! // Don't forget to deinit! Drop will panic!
//! axiom.deinit().await;
//! ```
//!
//! See `examples/simple.rs` for a working example.
//!

use std::{borrow::Cow, collections::BTreeMap};

use prost::Message as _;
pub use reqwest::Url;

pub mod layer;
pub mod metrics;
mod proto;

pub(crate) const INTERNAL_TARGET: &str = "tracing_axiom::internal";
const AXIOM_DATASET_HEADER: &str = "X-Axiom-Dataset";

#[derive(Clone, Copy, Debug)]
pub enum DatasetIds<'a> {
    /// Send events to `POST {base_url}/v1/ingest/{dataset_id}`.
    Events { dataset_id: &'a str },
    /// Send metrics to `POST {base_url}/v1/metrics` with `X-Axiom-Dataset`.
    Metrics { dataset_id: &'a str },
    /// Send events and metrics, allowing distinct Axiom datasets for each.
    All { event_dataset_id: &'a str, metric_dataset_id: &'a str },
}

impl<'a> DatasetIds<'a> {
    fn event_dataset_id(self) -> Option<&'a str> {
        match self {
            Self::Events { dataset_id } => Some(dataset_id),
            Self::Metrics { .. } => None,
            Self::All { event_dataset_id, .. } => Some(event_dataset_id),
        }
    }

    fn metric_dataset_id(self) -> Option<&'a str> {
        match self {
            Self::Events { .. } => None,
            Self::Metrics { dataset_id } => Some(dataset_id),
            Self::All { metric_dataset_id, .. } => Some(metric_dataset_id),
        }
    }
}

pub struct Config<'a> {
    /// Axiom ingest auth token.
    ///
    /// See <https://axiom.co/docs/restapi/ingest>.
    pub api_key: &'a str,
    /// Base URL for your Axiom edge deployment ingest API.
    ///
    /// See <https://axiom.co/docs/reference/regions>.
    pub base_url: reqwest::Url,
    /// Axiom datasets to use for event and/or metric ingest.
    pub datasets: DatasetIds<'a>,
    /// Event queue length. Will start dropping events once this is full
    pub evt_que_len: usize,
    pub met_que_len: usize,
    pub service_name: &'static str,

    /// Try to collect this many events before sending to Axiom.
    ///
    /// Should be > 0 and <= 10_000.
    /// See <https://axiom.co/docs/reference/limits#limits-on-ingested-data>.
    pub collect_target: usize,
    /// If we didn't collect up to target after this duratiom, timeout and send
    /// what we have.
    pub collect_timeout: std::time::Duration,
    /// Max number of concurrent sender jobs.
    pub sender_pool_size: usize,
}

pub struct Axiom<X: Send = Never> {
    // NOTE: ORER MATTERS. this sender needs to be dropped before _evt_handle
    pub evt_tx: tokio::sync::mpsc::Sender<Event<X>>,
    pub met_tx: tokio::sync::mpsc::Sender<metrics::Metric>,
    evt_handle: Option<tokio::task::JoinHandle<()>>,
    met_handle: Option<tokio::task::JoinHandle<()>>,
}

pub fn init<X>(cfg: Config) -> Axiom<X>
where
    X: serde::Serialize + std::marker::Send + 'static,
{
    // NOTE: mirror axiom-go's default client timeouts.
    // Total timeout:
    // https://github.com/axiomhq/axiom-go/blob/05ab863353532f691e2e46d72008d39897de3b6c/axiom/client.go#L61
    // Transport timeouts:
    // https://github.com/axiomhq/axiom-go/blob/05ab863353532f691e2e46d72008d39897de3b6c/axiom/client.go#L67
    const TIMEOUT_REQ: std::time::Duration =
        std::time::Duration::from_secs(300);
    const TIMEOUT_CONNECT: std::time::Duration =
        std::time::Duration::from_secs(30);
    const TIMEOUT_READ: std::time::Duration =
        std::time::Duration::from_secs(120);

    assert!(cfg.evt_que_len > 0, "evt_que_len must be > 0");
    assert!(cfg.met_que_len > 0, "met_que_len must be > 0");
    assert!(cfg.collect_target > 0, "collect_target must be > 0");
    // See field doc-comment
    assert!(cfg.collect_target <= 10_000, "collect_target must be <= 10_000");
    assert!(cfg.sender_pool_size > 0, "sender_pool_size must be > 0");

    let event_dataset_id = cfg.datasets.event_dataset_id();
    let metric_dataset_id = cfg.datasets.metric_dataset_id();
    let (evt_tx, evt_rx) = tokio::sync::mpsc::channel(cfg.evt_que_len);

    // NOTE: We let tokio handle errors for these queues, you shouldnt be able to send
    // into queue without valid dataset tagged therefore queue will not exist and we will
    // get send err from tokio.
    let mut evt_rx = event_dataset_id.map(|_| evt_rx);
    let (met_tx, met_rx) = tokio::sync::mpsc::channel(cfg.met_que_len);
    let mut met_rx = metric_dataset_id.map(|_| met_rx);

    // NOTE: too much effort to bubble error here. this is run once on app init
    //       so this is fine. spurious crashlooping is impossible as the
    //       parsing is deterministic and config shouldn't be dynamic
    let bearer = reqwest::header::HeaderValue::try_from(
        format!("Bearer {}", cfg.api_key), //.
    )
    .unwrap();
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT_REQ)
        .connect_timeout(TIMEOUT_CONNECT)
        .read_timeout(TIMEOUT_READ)
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .redirect(reqwest::redirect::Policy::custom(|a| {
            let status = a.status().as_u16();
            // the two redirect types that discard the body
            if status == 302 || status == 303 {
                let to = a.url().clone();
                return a.error(LossyRedirect { status, to });
            }
            // delegate to default impl
            reqwest::redirect::Policy::default().redirect(a)
        }))
        .build()
        .unwrap();

    let sender_pool_size = cfg.sender_pool_size;
    let collect_target = cfg.collect_target;
    let collect_timeout = cfg.collect_timeout;
    let service_name = cfg.service_name;
    let rt = tokio::runtime::Handle::current();

    let evt_handle = event_dataset_id.map(|dataset_id| {
        let mut evt_rx = evt_rx.take().unwrap();
        let evt_client = client.clone();
        let evt_ingest_url =
            cfg.base_url.join(&format!("v1/ingest/{dataset_id}")).unwrap();
        let evt_bearer = bearer.clone();
        let evt_task = async move {
            let mut slots = Vec::with_capacity(sender_pool_size);
            for _ in 0..sender_pool_size {
                slots.push(SenderSlot::default());
            }
            let slots: Box<[SenderSlot]> = slots.into_boxed_slice();
            let (idle_tx, mut idle_rx) =
                tokio::sync::mpsc::channel(sender_pool_size);

            let coord = evt_coord_task(
                &mut evt_rx,
                &mut idle_rx,
                &slots,
                collect_target,
                collect_timeout,
                service_name,
            );
            let senders = slots
                .iter()
                .enumerate()
                .map(|(id, slot)| SenderFut {
                    done: false,
                    fut: sender_task(
                        id,
                        slot,
                        &idle_tx,
                        &evt_client,
                        &evt_ingest_url,
                        &evt_bearer,
                    ),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice();
            let mut senders = Box::into_pin(senders);
            let senders =
                std::future::poll_fn(|cx| poll_senders(senders.as_mut(), cx));

            let ((), ()) = tokio::join!(coord, senders);
        };
        // NOTE: don't capture the caller's current dispatch here. In telm the
        // global subscriber is installed after `init()`, so freezing the current
        // dispatch would pin this bg task to the pre-init no-op subscriber and
        // hide its internal logs from stderr/journal forever.
        rt.spawn(evt_task)
    });

    let met_handle = metric_dataset_id.map(|dataset_id| {
        let mut met_rx = met_rx.take().unwrap();
        let met_client = client.clone();
        let met_ingest_url = cfg.base_url.join("v1/metrics").unwrap();
        let met_bearer = bearer.clone();
        let met_dataset =
            reqwest::header::HeaderValue::try_from(dataset_id).unwrap();
        let met_task = async move {
            let mut slots = Vec::with_capacity(sender_pool_size);
            for _ in 0..sender_pool_size {
                slots.push(SenderSlot::default());
            }
            let slots: Box<[SenderSlot]> = slots.into_boxed_slice();
            let (idle_tx, mut idle_rx) =
                tokio::sync::mpsc::channel(sender_pool_size);

            let coord = met_coord_task(
                &mut met_rx,
                &mut idle_rx,
                &slots,
                collect_target,
                collect_timeout,
                service_name,
                met_dataset,
            );
            let senders = slots
                .iter()
                .enumerate()
                .map(|(id, slot)| SenderFut {
                    done: false,
                    fut: sender_task(
                        id,
                        slot,
                        &idle_tx,
                        &met_client,
                        &met_ingest_url,
                        &met_bearer,
                    ),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice();
            let mut senders = Box::into_pin(senders);
            let senders =
                std::future::poll_fn(|cx| poll_senders(senders.as_mut(), cx));

            let ((), ()) = tokio::join!(coord, senders);
        };
        rt.spawn(met_task)
    });

    Axiom { met_tx, evt_tx, evt_handle, met_handle }
}

impl<X: Send> Axiom<X> {
    /// Call this instead of dropping.
    ///
    /// Drop any cloned `evt_tx` senders first if you want a clean shutdown
    /// without warnings. This waits for the bg sender task to flush and exit.
    pub async fn deinit(self) {
        // Non-dropping destructure. We drop the fields in this fn ourselves.
        let (evt_tx, met_tx, evt_handle, met_handle) = unsafe {
            let this = std::mem::ManuallyDrop::new(self);
            let Axiom { evt_tx, met_tx, evt_handle, met_handle } = &*this;
            (
                std::ptr::read(evt_tx),
                std::ptr::read(met_tx),
                std::ptr::read(evt_handle),
                std::ptr::read(met_handle),
            )
        };

        let senders = evt_tx.strong_count() - 1;
        if senders > 0 {
            tracing::warn!(
                senders,
                "deinit Axiom handle while event senders still exist!"
            );
        }
        let senders = met_tx.strong_count() - 1;
        if senders > 0 {
            tracing::warn!(
                senders,
                "deinit Axiom handle while metric senders still exist!"
            );
        }
        // This should be the last strong sender and so close the channel.
        // The bg tasks will detect this for a graceful shutdown.
        drop(evt_tx);
        drop(met_tx);

        if let Some(evt_handle) = evt_handle {
            evt_handle.await.unwrap();
        }
        if let Some(met_handle) = met_handle {
            met_handle.await.unwrap();
        }
    }
}

// RAII + async = nonsense
//
/// Please DO NOT rely on this drop handler. This is nasty nonsense in
/// case of real panics.
///
/// Might even be worth catching panics in the calling code if it makes
/// sense so that the caller can properly call deinit().
impl<X: Send> Drop for Axiom<X> {
    fn drop(&mut self) {
        // This is a last ditch effort to not drop events.
        let mut bogus = Self {
            met_tx: tokio::sync::mpsc::channel(1).0,
            evt_tx: tokio::sync::mpsc::channel(1).0,
            evt_handle: None,
            met_handle: None,
        };
        std::mem::swap(self, &mut bogus);
        let actual = bogus;

        // block_in_place depends on a tokio feature flag.
        std::thread::scope(move |s| {
            let rt = tokio::runtime::Handle::current();
            s.spawn(move || rt.block_on(actual.deinit()));
        });

        if !std::thread::panicking() {
            panic!("call Axiom::deinit() instead of dropping it!");
        }
    }
}

pub fn layer<X>(
    evt_tx: tokio::sync::mpsc::WeakSender<Event<X>>,
) -> layer::Layer<X> {
    layer::Layer::<X> { sender: evt_tx }
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum Event<Extra = Never> {
    // Order of fields is optimized for json human readability.
    //
    // For Axiom trace schema list of required field names for tracing
    // integration, see https://axiom.co/docs/query-data/traces
    Log {
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<Cow<'static, str>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        module_path: Option<Cow<'static, str>>,
        target: Cow<'static, str>,
        level: Level,
        name: Cow<'static, str>,
        /// This field should be a map field in Axiom.
        ///
        /// See <https://axiom.co/docs/apl/data-types/map-fields>.
        data: layer::FieldCascade,
        #[serde(rename = "_time", with = "time::serde::rfc3339")]
        time: time::OffsetDateTime,
        #[serde(
            skip_serializing_if = "Option::is_none",
            serialize_with = "ser_opt_hex"
        )]
        trace_id: Option<[u8; 16]>,
        #[serde(
            skip_serializing_if = "Option::is_none",
            serialize_with = "ser_opt_hex"
        )]
        parent_span_id: Option<[u8; 8]>,
        kind: BogusKind,
    },
    Span {
        #[serde(skip_serializing_if = "Option::is_none")]
        module_path: Option<Cow<'static, str>>,
        target: Cow<'static, str>,
        level: Level,
        name: Cow<'static, str>,
        // We shouldn't need this but seems rust type inference and/or macro
        // expansion order needs a little help.
        #[serde(
            skip_serializing_if = "Option::is_none",
            serialize_with = "layer::ser_option_field_cascade"
        )]
        /// This field should be a map field in Axiom.
        ///
        /// See <https://axiom.co/docs/apl/data-types/map-fields>.
        data: Option<std::sync::Arc<std::sync::Mutex<layer::FieldCascade>>>,
        #[serde(rename = "_time", with = "time::serde::rfc3339")]
        time: time::OffsetDateTime,
        #[serde(flatten, skip_serializing_if = "Option::is_none")]
        span_info: Option<SpanInfo>,
        #[serde(
            skip_serializing_if = "Option::is_none",
            serialize_with = "ser_opt_hex"
        )]
        parent_span_id: Option<[u8; 8]>,
        kind: BogusKind,
    },
    Extra(Extra),
}

impl<Extra: serde::Serialize> std::fmt::Debug for Event<Extra> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        struct FmtIo<'a, 'b>(&'a mut std::fmt::Formatter<'b>);

        impl std::io::Write for FmtIo<'_, '_> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let s =
                    std::str::from_utf8(buf).map_err(std::io::Error::other)?;
                self.0.write_str(s).map_err(std::io::Error::other)?;
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        serde_json::to_writer(FmtIo(f), self).map_err(|_| std::fmt::Error)
    }
}

/// Axiom absolutely wants this for trace/span recognition so here it is.
pub struct BogusKind;
impl serde::Serialize for BogusKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("server")
    }
}

#[derive(Debug, serde::Serialize)]
pub struct SpanInfo {
    #[serde(serialize_with = "ser_hex")]
    pub span_id: [u8; 8],
    #[serde(serialize_with = "ser_hex")]
    pub trace_id: [u8; 16],
    #[serde(rename = "duration")]
    pub duration_ns: u64,
    pub idle_ns: u64,
    pub busy_ns: u64,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}
impl From<tracing::Level> for Level {
    fn from(value: tracing::Level) -> Self {
        match value {
            tracing::Level::TRACE => Level::Trace,
            tracing::Level::DEBUG => Level::Debug,
            tracing::Level::INFO => Level::Info,
            tracing::Level::WARN => Level::Warn,
            tracing::Level::ERROR => Level::Error,
        }
    }
}

// Edge ingest API response schema:
// https://axiom.co/docs/restapi/endpoints/ingestToDataset
#[allow(dead_code)]
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct IngestStatus {
    failed: u64,
    ingested: u64,
    processed_bytes: u64,
    blocks_created: Option<u32>,
    failures: Vec<IngestFailure>,
    wal_length: Option<u32>,
}

#[allow(dead_code)]
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct IngestFailure {
    error: String,
    timestamp: String,
}

#[derive(Debug)]
pub struct LossyRedirect {
    status: u16,
    to: reqwest::Url,
}

impl std::fmt::Display for LossyRedirect {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // Following such a redirect drops the request body, and will likely
        // give an HTTP 200 response even though nobody ever looked at the POST
        // body.
        //
        // This can e.g. happen for login redirects when you post to a
        // login-protected URL.
        write!(
            f,
            "lossy HTTP {} redirect to {} would cut off our body",
            self.status, self.to
        )
    }
}

impl std::error::Error for LossyRedirect {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    Number(serde_json::Number),
    String(Cow<'static, str>),
    Json(serde_json::Value),
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

macro_rules! from_integer {
    ($($ty:ident)*) => {
        $(
            impl From<$ty> for Value {
                fn from(n: $ty) -> Self {
                    Value::Number(n.into())
                }
            }
        )*
    };
}

from_integer! {
    i8 i16 i32 i64 isize
    u8 u16 u32 u64 usize
}

impl From<f32> for Value {
    /// Convert 32-bit floating point number to `Value::Number`, or
    /// `Value::String` if infinite or NaN.
    fn from(f: f32) -> Self {
        // serde_json making Number::from_f32 private has forced my hand
        f64::from(f).into()
    }
}

impl From<f64> for Value {
    /// Convert 64-bit floating point number to `Value::Number`, or
    /// `Value::String` if infinite or NaN.
    fn from(f: f64) -> Self {
        serde_json::Number::from_f64(f)
            .map(Self::Number)
            // this is a little slimy but good behavior for honeycomb specifically
            // there's not really much else since we don't have a Null variant
            .unwrap_or_else(|| Self::String(format!("{}", f).into()))
    }
}

impl From<String> for Value {
    fn from(f: String) -> Self {
        Value::String(Cow::Owned(f))
    }
}

impl From<&str> for Value {
    fn from(f: &str) -> Self {
        Value::String(Cow::Owned(f.to_owned()))
    }
}

// we sacrifice generality here to prevent the footgun of
//  Cow<'static, str>::Borrowed(&'static str)::into() silently
//  converting into Cow::Owned
impl From<Cow<'static, str>> for Value {
    fn from(f: Cow<'static, str>) -> Self {
        Value::String(f)
    }
}

impl From<serde_json::Number> for Value {
    /// Convert `Number` to `Value::Number`.
    ///
    /// # Examples
    ///
    /// ```
    /// use serde_json::{Number, Value};
    ///
    /// let n = Number::from(7);
    /// let x: Value = n.into();
    /// ```
    fn from(f: serde_json::Number) -> Self {
        Value::Number(f)
    }
}

impl serde::Serialize for Value {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::ser::Serializer,
    {
        match self {
            Self::Bool(b) => serializer.serialize_bool(*b),
            Self::Number(n) => n.serialize(serializer),
            Self::String(s) => serializer.serialize_str(s),
            Self::Json(v) => v.serialize(serializer),
        }
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, PartialEq)]
pub enum Never {}

#[derive(serde::Serialize)]
struct EventWrapper<'a, X> {
    // NOTE: Axiom wants this otel artifact for their tooling to work properly
    //       ideally we'd just not have this or have this as an unnested
    //       `service_name` field, but alas.
    service: EventService<'a>,
    #[serde(flatten)]
    event: &'a Event<X>,
}
#[derive(serde::Serialize)]
struct EventService<'a> {
    name: &'a str,
}

// Support team told me 1<<20 max per ndjson line
const NDJSON_LINE_LEN_MAX: usize = 1 << 20;
const WARN_JSON_LEN_MAX: usize = 2 << 10;
const WARN_JSON_SUFFIX: &str = "...<truncated>";

struct CountWrite<W> {
    inner: W,
    bytes: usize,
}
impl<W: std::io::Write> std::io::Write for CountWrite<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let bytes = self.inner.write(buf)?;
        self.bytes += bytes;
        Ok(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct TruncWrite<'a> {
    buf: &'a mut Vec<u8>,
    limit: usize,
    trunc: bool,
}
impl<'a> TruncWrite<'a> {
    fn new(buf: &'a mut Vec<u8>, limit: usize) -> Self {
        Self { buf, limit, trunc: false }
    }
}
impl std::io::Write for TruncWrite<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let left = self.limit.saturating_sub(self.buf.len());
        let keep = left.min(buf.len());
        self.buf.extend_from_slice(&buf[..keep]);
        self.trunc |= keep < buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_ndjson_line<W, X>(
    writer: &mut W,
    evt: &EventWrapper<'_, X>,
) -> std::io::Result<usize>
where
    W: std::io::Write,
    X: serde::Serialize,
{
    let mut writer = CountWrite { inner: writer, bytes: 0 };
    serde_json::to_writer(&mut writer, evt).map_err(std::io::Error::other)?;
    std::io::Write::write_all(&mut writer, b"\n")?;
    Ok(writer.bytes)
}

fn warn_json_dump<'a, X>(
    buf: &'a mut Vec<u8>,
    evt: &EventWrapper<'_, X>,
) -> Result<(&'a str, bool), serde_json::Error>
where
    X: serde::Serialize,
{
    buf.clear();

    let limit = WARN_JSON_LEN_MAX.saturating_sub(WARN_JSON_SUFFIX.len());
    let trunc = {
        let mut writer = TruncWrite::new(buf, limit);
        serde_json::to_writer(&mut writer, evt)?;
        writer.trunc
    };

    let end = match std::str::from_utf8(buf) {
        Ok(_) => buf.len(),
        Err(err) => err.valid_up_to(),
    };
    buf.truncate(end);
    if trunc {
        buf.extend_from_slice(WARN_JSON_SUFFIX.as_bytes());
    }
    Ok((std::str::from_utf8(buf).unwrap(), trunc))
}

enum BatchCtl {
    Continue,
    DropBatch,
    Shutdown,
}

fn encode_evt_line<W, X>(
    encoder: &mut zstd::Encoder<'_, W>,
    buf_warn_json: &mut Vec<u8>,
    evt: &EventWrapper<'_, X>,
    evts_count: usize,
) -> BatchCtl
where
    W: std::io::Write,
    X: serde::Serialize,
{
    let len = match write_ndjson_line(encoder, evt) {
        Ok(bytes_line) => bytes_line,
        Err(err) => {
            tracing::error!(
                target: INTERNAL_TARGET,
                ?err,
                evts_count,
                "failed to encode event batch. dropping batch"
            );
            return BatchCtl::DropBatch;
        }
    };
    if len > NDJSON_LINE_LEN_MAX {
        match warn_json_dump(buf_warn_json, evt) {
            Ok((event_json_truncated, event_json_was_truncated)) => {
                tracing::warn!(
                    target: INTERNAL_TARGET,
                    len,
                    bytes_limit = NDJSON_LINE_LEN_MAX,
                    evts_count,
                    event_json_truncated,
                    event_json_was_truncated,
                    "ndjson line exceeds limit"
                );
            }
            Err(err) => {
                tracing::warn!(
                    target: INTERNAL_TARGET,
                    len,
                    bytes_limit = NDJSON_LINE_LEN_MAX,
                    evts_count,
                    ?err,
                    concat!(
                        "ndjson line exceeds limit and warning ",
                        "dump serialization failed"
                    )
                );
            }
        }
    }
    BatchCtl::Continue
}

#[derive(Default)]
struct SenderSlot {
    /// Set by the coordinator, taken by exactly one sender task.
    state: tokio::sync::Mutex<SenderSlotState>,
    /// Wake the sender task after publishing a blob or closure.
    ready: tokio::sync::Notify,
}

#[derive(Default)]
struct SenderSlotState {
    blob: Option<BatchBlob>,
    closed: bool,
}

struct BatchBlob {
    body: bytes::Bytes,
    items_count: usize,
    content_type: &'static str,
    response_kind: ResponseKind,
    axiom_dataset: Option<reqwest::header::HeaderValue>,
}

#[derive(Clone, Copy)]
enum ResponseKind {
    IngestStatus,
    MetricsExport,
}

struct SenderFut<F> {
    done: bool,
    fut: F,
}

fn poll_senders<F>(
    mut senders: std::pin::Pin<&mut [SenderFut<F>]>,
    cx: &mut std::task::Context<'_>,
) -> std::task::Poll<()>
where
    F: std::future::Future<Output = ()>,
{
    let mut pending = false;

    // SAFETY:
    // `senders` is pinned once as a boxed slice and never moved again.
    // We never reorder or replace elems, and only poll each future in place.
    for sender in unsafe { senders.as_mut().get_unchecked_mut() }.iter_mut() {
        if sender.done {
            continue;
        }

        match unsafe { std::pin::Pin::new_unchecked(&mut sender.fut) }.poll(cx)
        {
            std::task::Poll::Ready(()) => sender.done = true,
            std::task::Poll::Pending => pending = true,
        }
    }

    if pending { std::task::Poll::Pending } else { std::task::Poll::Ready(()) }
}

async fn sender_task(
    id: usize,
    slot: &SenderSlot,
    idle_tx: &tokio::sync::mpsc::Sender<usize>,
    client: &reqwest::Client,
    ingest_url: &reqwest::Url,
    bearer: &reqwest::header::HeaderValue,
) {
    loop {
        if idle_tx.send(id).await.is_err() {
            return;
        }

        slot.ready.notified().await;

        let blob = {
            let mut state = slot.state.lock().await;
            match state.blob.take() {
                Some(blob) => blob,
                None if state.closed => return,
                None => unreachable!(),
            }
        };
        send_blob(client, ingest_url, bearer, blob).await;

        if slot.state.lock().await.closed {
            return;
        }
    }
}

async fn evt_coord_task<X>(
    evt_rx: &mut tokio::sync::mpsc::Receiver<Event<X>>,
    idle_rx: &mut tokio::sync::mpsc::Receiver<usize>,
    slots: &[SenderSlot],
    collect_target: usize,
    collect_timeout: std::time::Duration,
    service_name: &'static str,
) where
    X: serde::Serialize + Send + 'static,
{
    use bytes::BufMut as _;

    let mut zstd_ctx = zstd::zstd_safe::CCtx::try_create().unwrap();
    let mut body = bytes::BytesMut::with_capacity(2048);
    let mut evts_buf = Vec::with_capacity(collect_target);
    let mut buf_warn_json = Vec::with_capacity(WARN_JSON_LEN_MAX);
    'batch: loop {
        let mut evts_count = 0;

        body.clear();
        let mut body_writer = body.writer();
        let mut encoder = zstd::Encoder::with_context(
            &mut body_writer, //.
            &mut zstd_ctx,
        );

        let mut rest = collect_target;
        while evts_count == 0 {
            match tokio::time::timeout(collect_timeout, async {
                while rest > 0 {
                    evts_buf.clear();
                    let read = evt_rx.recv_many(&mut evts_buf, rest).await;
                    assert_eq!(read, evts_buf.len());
                    if read == 0 {
                        if evts_count == 0 && evts_buf.is_empty() {
                            return BatchCtl::Shutdown;
                        }
                        return BatchCtl::Continue;
                    }
                    rest -= read;
                    evts_count += read;
                    for evt in &evts_buf {
                        let evt = EventWrapper {
                            service: EventService { name: service_name },
                            event: evt,
                        };
                        match encode_evt_line(
                            &mut encoder,
                            &mut buf_warn_json,
                            &evt,
                            evts_count,
                        ) {
                            BatchCtl::Continue => {}
                            BatchCtl::DropBatch => {
                                return BatchCtl::DropBatch;
                            }
                            BatchCtl::Shutdown => unreachable!(),
                        }
                    }
                }
                BatchCtl::Continue
            })
            .await
            {
                Ok(BatchCtl::Shutdown) => {
                    close_slots(slots).await;
                    return;
                }
                Ok(BatchCtl::DropBatch) => {
                    drop(encoder);
                    body = body_writer.into_inner();
                    zstd_ctx
                        .reset(zstd::zstd_safe::ResetDirective::SessionOnly)
                        .unwrap();
                    continue 'batch;
                }
                Ok(BatchCtl::Continue)
                | Err(tokio::time::error::Elapsed { .. }) => {}
            };
        }
        assert!(evts_count > 0);
        assert!(evts_count <= collect_target);

        encoder.finish().unwrap();
        body = body_writer.into_inner();
        let blob = BatchBlob {
            body: body.split().freeze(),
            items_count: evts_count,
            content_type: "application/json",
            response_kind: ResponseKind::IngestStatus,
            axiom_dataset: None,
        };

        let id = idle_rx
            .recv()
            .await
            .expect("sender tasks exited before coordinator");
        let slot = &slots[id];
        {
            let mut state = slot.state.lock().await;
            assert!(state.blob.is_none());
            assert!(!state.closed);
            state.blob = Some(blob);
        }
        slot.ready.notify_one();
    }
}

async fn met_coord_task(
    met_rx: &mut tokio::sync::mpsc::Receiver<metrics::Metric>,
    idle_rx: &mut tokio::sync::mpsc::Receiver<usize>,
    slots: &[SenderSlot],
    collect_target: usize,
    collect_timeout: std::time::Duration,
    service_name: &'static str,
    axiom_dataset: reqwest::header::HeaderValue,
) {
    use bytes::BufMut as _;

    let mut zstd_ctx = zstd::zstd_safe::CCtx::try_create().unwrap();
    let mut body = bytes::BytesMut::with_capacity(2048);
    let mut mets_buf = Vec::with_capacity(collect_target);
    let mut mets = Vec::with_capacity(collect_target);
    loop {
        let mut mets_count = 0;
        mets.clear();
        let mut rest = collect_target;

        while mets_count == 0 {
            match tokio::time::timeout(collect_timeout, async {
                while rest > 0 {
                    mets_buf.clear();
                    let read = met_rx.recv_many(&mut mets_buf, rest).await;
                    assert_eq!(read, mets_buf.len());
                    if read == 0 {
                        if mets_count == 0 && mets_buf.is_empty() {
                            return BatchCtl::Shutdown;
                        }
                        return BatchCtl::Continue;
                    }
                    rest -= read;
                    mets_count += read;
                    mets.append(&mut mets_buf);
                }
                BatchCtl::Continue
            })
            .await
            {
                Ok(BatchCtl::Shutdown) => {
                    close_slots(slots).await;
                    return;
                }
                Ok(BatchCtl::Continue)
                | Err(tokio::time::error::Elapsed { .. }) => {}
                Ok(BatchCtl::DropBatch) => unreachable!(),
            }
        }

        assert!(mets_count > 0);
        assert!(mets_count <= collect_target);

        let time_unix_nano =
            time::OffsetDateTime::now_utc().unix_timestamp_nanos() as u64;
        let resource_attrs = Some(BTreeMap::from([(
            "service.name".to_string(),
            metrics::AttrValue::Str(service_name.to_string()),
        )]));
        let batch =
            std::mem::replace(&mut mets, Vec::with_capacity(collect_target));
        let proto =
            metrics::metrics_to_proto(batch, time_unix_nano, resource_attrs);

        body.clear();
        let mut body_writer = body.writer();
        let mut encoder = zstd::Encoder::with_context(
            &mut body_writer, //.
            &mut zstd_ctx,
        );
        std::io::Write::write_all(&mut encoder, &proto.encode_to_vec())
            .unwrap();
        encoder.finish().unwrap();
        body = body_writer.into_inner();
        let blob = BatchBlob {
            body: body.split().freeze(),
            items_count: mets_count,
            content_type: "application/x-protobuf",
            response_kind: ResponseKind::MetricsExport,
            axiom_dataset: Some(axiom_dataset.clone()),
        };

        let id = idle_rx
            .recv()
            .await
            .expect("sender tasks exited before coordinator");
        let slot = &slots[id];
        {
            let mut state = slot.state.lock().await;
            assert!(state.blob.is_none());
            assert!(!state.closed);
            state.blob = Some(blob);
        }
        slot.ready.notify_one();
    }
}

async fn close_slots(slots: &[SenderSlot]) {
    for slot in slots {
        {
            let mut state = slot.state.lock().await;
            state.closed = true;
        }
        slot.ready.notify_one();
    }
}

async fn send_blob(
    client: &reqwest::Client,
    ingest_url: &reqwest::Url,
    bearer: &reqwest::header::HeaderValue,
    blob: BatchBlob,
) {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RetryErrKind {
        Transport,
        HttpStatus(reqwest::StatusCode),
    }
    enum SendCtl {
        Done,
        Retry,
    }

    // Mostly mirroring axiom-go's retry cfg and 5xx-only HTTP retry policy.
    // Docs: https://pkg.go.dev/github.com/axiomhq/axiom-go/axiom
    // Src: https://github.com/axiomhq/axiom-go/blob/main/axiom/client.go#L259
    const INITIAL_BACKOFF: std::time::Duration =
        std::time::Duration::from_millis(200);
    const MAX_RETRY_ELAPSED: std::time::Duration =
        std::time::Duration::from_secs(10);
    const BACKOFF_MULT: f32 = 1.5;

    let retry_start = tokio::time::Instant::now();
    let mut backoff = INITIAL_BACKOFF;
    // NOTE: to avoid spamming logs and causing data loss due to truncation, we
    // only print the first of a repeated sequence of the same retry error.
    let mut last_err_kind = None;
    macro_rules! log_retry {
        ($kind:expr, $($arg:tt)*) => {{
            let kind = $kind;
            if last_err_kind == Some(kind) {
                tracing::debug!($($arg)*);
            } else {
                tracing::error!($($arg)*);
            }
            last_err_kind = Some(kind);
        }};
    }
    let mut attempts = 0u16;
    let ctl = loop {
        attempts += 1;
        let mut req = client
            .post(ingest_url.clone())
            .header(reqwest::header::AUTHORIZATION, bearer)
            .header(reqwest::header::CONTENT_TYPE, blob.content_type)
            .header(reqwest::header::CONTENT_ENCODING, "zstd");
        if let Some(axiom_dataset) = &blob.axiom_dataset {
            req = req.header(AXIOM_DATASET_HEADER, axiom_dataset);
        }
        let res = req
            .body(blob.body.clone())
            .send()
            .await
            .and_then(|resp| resp.error_for_status());
        let ctl = match res {
            Ok(resp) => match resp.bytes().await {
                Ok(status_raw) => {
                    match blob.response_kind {
                        ResponseKind::IngestStatus => {
                            match serde_json::from_slice::<IngestStatus>(
                                &status_raw,
                            ) {
                                Ok(status)
                                    if status.failed > 0
                                        || !status.failures.is_empty() =>
                                {
                                    tracing::error!(
                                        target: INTERNAL_TARGET,
                                        attempt = attempts - 1,
                                        failed = status.failed,
                                        ingested = status.ingested,
                                        status=?status_raw,
                                        items_count = blob.items_count,
                                        "axiom reported partial ingest."
                                    );
                                }
                                Ok(_) => {}
                                Err(err) => {
                                    tracing::error!(
                                        target: INTERNAL_TARGET,
                                        attempt = attempts - 1,
                                        ?err,
                                        status = ?status_raw,
                                        items_count = blob.items_count,
                                        "failed to parse ingest response body."
                                    );
                                }
                            }
                        }
                        ResponseKind::MetricsExport => {
                            match proto::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceResponse::decode(
                                status_raw.as_ref(),
                            ) {
                                Ok(status) => {
                                    if let Some(partial_success) =
                                        status.partial_success
                                    {
                                        let rejected_data_points =
                                            partial_success
                                                .rejected_data_points;
                                        let error_message =
                                            partial_success.error_message;
                                        if rejected_data_points > 0 {
                                            tracing::error!(
                                                target: INTERNAL_TARGET,
                                                attempt = attempts - 1,
                                                rejected_data_points,
                                                error_message,
                                                items_count = blob.items_count,
                                                "axiom reported partial metrics ingest."
                                            );
                                        } else if !error_message.is_empty() {
                                            tracing::warn!(
                                                target: INTERNAL_TARGET,
                                                attempt = attempts - 1,
                                                rejected_data_points,
                                                error_message,
                                                items_count = blob.items_count,
                                                "axiom reported metrics ingest warning."
                                            );
                                        }
                                    }
                                }
                                Err(err) => {
                                    tracing::error!(
                                        target: INTERNAL_TARGET,
                                        attempt = attempts - 1,
                                        ?err,
                                        bytes_len = status_raw.len(),
                                        items_count = blob.items_count,
                                        "failed to parse metrics ingest response body."
                                    );
                                }
                            }
                        }
                    }
                    SendCtl::Done
                }
                Err(err) => {
                    tracing::error!(
                        target: INTERNAL_TARGET,
                        attempt = attempts - 1,
                        ?err,
                        items_count = blob.items_count,
                        "failed to read ingest response body. dropping blob"
                    );
                    SendCtl::Done
                }
            },
            Err(err) => {
                if err.is_connect() {
                    log_retry!(
                        RetryErrKind::Transport,
                        target: INTERNAL_TARGET,
                        ?backoff,
                        attempt = attempts - 1,
                        ?err,
                        items_count = blob.items_count,
                        "axiom connect failed"
                    );
                    SendCtl::Retry
                // Axiom-go retries HTTP status >=500 here:
                // https://github.com/axiomhq/axiom-go/blob/main/axiom/client.go#L277
                } else if matches!(
                    err.status(),
                    Some(status) if status.is_server_error()
                ) {
                    log_retry!(
                        RetryErrKind::HttpStatus(err.status().unwrap()),
                        target: INTERNAL_TARGET,
                        ?backoff,
                        attempt = attempts - 1,
                        ?err,
                        items_count = blob.items_count,
                        "axiom request failed"
                    );
                    SendCtl::Retry
                } else {
                    tracing::error!(
                        target: INTERNAL_TARGET,
                        attempt = attempts - 1,
                        ?err,
                        items_count = blob.items_count,
                        "non-retryable axiom request failure. dropping blob"
                    );
                    SendCtl::Done
                }
            }
        };
        match ctl {
            SendCtl::Done => break SendCtl::Done,
            SendCtl::Retry => {
                let Some(remaining) =
                    MAX_RETRY_ELAPSED.checked_sub(retry_start.elapsed())
                else {
                    break SendCtl::Retry;
                };
                if remaining.is_zero() {
                    break SendCtl::Retry;
                }
                tokio::time::sleep(backoff.min(remaining)).await;
                backoff = backoff.mul_f32(BACKOFF_MULT);
            }
        }
    };
    if matches!(ctl, SendCtl::Retry) {
        tracing::error!(
            target: INTERNAL_TARGET,
            max_retry_elapsed_ms = MAX_RETRY_ELAPSED.as_millis(),
            attempts,
            items_count = blob.items_count,
            "reached retry deadline for ingest batch. dropping items!"
        );
    }
}

fn ser_hex<const N: usize, S>(
    bytes: &[u8; N],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::ser::Serializer,
{
    fn chr(v: u8) -> u8 {
        u8::try_from(char::from_digit(v.into(), 16).unwrap()).unwrap()
    }

    // max size in onur use case is trace id which is 16 bytes
    const BUF_LEN: usize = 32;
    assert!(N * 2 <= BUF_LEN);
    let mut buf = [0u8; BUF_LEN];

    for (i, b) in bytes.iter().enumerate() {
        buf[i * 2] = chr(b / 16);
        buf[i * 2 + 1] = chr(b % 16);
    }

    serializer.serialize_str(unsafe { str::from_utf8_unchecked(&buf[..N * 2]) })
}
fn ser_opt_hex<const N: usize, S>(
    bytes: &Option<[u8; N]>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::ser::Serializer,
{
    match bytes {
        Some(bytes) => ser_hex(bytes, serializer),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        net::SocketAddr,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use axum::{
        Router,
        body::to_bytes,
        extract::{Request, State},
        response::IntoResponse,
        routing::post,
    };
    use prost::Message as _;
    use serde::{Deserialize, Serialize};

    use super::{
        AXIOM_DATASET_HEADER, Config, DatasetIds, Event, EventService,
        EventWrapper, WARN_JSON_LEN_MAX, WARN_JSON_SUFFIX, init, metrics,
        proto, warn_json_dump, write_ndjson_line,
    };

    #[derive(Clone)]
    struct ServerCfg {
        delay: Duration,
        resps: Vec<StubResp>,
    }

    #[derive(Clone, Copy)]
    enum StubResp {
        Ok,
        Http400,
        Http500,
        Partial,
        OkBadJson,
    }

    #[derive(Clone, Debug)]
    struct ReqObs {
        /// Lowest event seq seen in this ingest request body.
        start_seq: u64,
        /// Event count in this ingest request body.
        evts: usize,
    }

    #[derive(Clone, Debug)]
    struct MetricReqObs {
        /// Metric count in this request body.
        metrics: usize,
        first_name: String,
        first_i64: Option<i64>,
        dataset: Option<String>,
        content_type: Option<String>,
        content_encoding: Option<String>,
        service_name: Option<String>,
    }

    #[derive(Deserialize, Serialize)]
    struct TestEvt {
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<String>,
    }

    #[derive(Default)]
    struct ServerObs {
        /// Request log in arrival order.
        reqs: Vec<ReqObs>,
        /// Metric request log in arrival order.
        metric_reqs: Vec<MetricReqObs>,
        /// Current concurrent requests inside the stub server.
        in_flight: usize,
        /// Peak concurrent requests inside the stub server.
        max_in_flight: usize,
    }

    #[derive(Clone)]
    struct StubServer {
        /// Static behavior knobs for this server instance.
        cfg: ServerCfg,
        /// Scripted response sequence for this server instance.
        resps: Arc<Mutex<VecDeque<StubResp>>>,
        /// Mutable observations shared with the tests.
        obs: Arc<Mutex<ServerObs>>,
    }

    struct RunObs {
        /// Completed request log in arrival order.
        reqs: Vec<ReqObs>,
        /// Completed metric request log in arrival order.
        metric_reqs: Vec<MetricReqObs>,
        /// Peak concurrent requests observed by the stub server.
        max_in_flight: usize,
    }

    struct TestServer {
        /// Bound local addr of the stub ingest server.
        addr: SocketAddr,
        /// Shared server state used both by the handler and the test.
        stub: StubServer,
        /// Triggers graceful server shutdown after the test run.
        shutdown_tx: tokio::sync::oneshot::Sender<()>,
        /// Join handle for the stub ingest server task.
        server: tokio::task::JoinHandle<()>,
    }

    fn decode_req(body: bytes::Bytes) -> ReqObs {
        let body =
            zstd::stream::decode_all(std::io::Cursor::new(body.as_ref()))
                .unwrap();

        let mut start_seq = None;
        let mut evts = 0usize;
        for line in body.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let evt: TestEvt = serde_json::from_slice(line).unwrap();
            start_seq.get_or_insert(evt.seq);
            evts += 1;
        }

        ReqObs { start_seq: start_seq.unwrap(), evts }
    }

    async fn decode_metric_req(req: Request) -> MetricReqObs {
        let (parts, body) = req.into_parts();
        let dataset = parts
            .headers
            .get(AXIOM_DATASET_HEADER)
            .map(|v| v.to_str().unwrap().to_string());
        let content_type = parts
            .headers
            .get(axum::http::header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string());
        let content_encoding = parts
            .headers
            .get(axum::http::header::CONTENT_ENCODING)
            .map(|v| v.to_str().unwrap().to_string());
        let body = to_bytes(body, usize::MAX).await.unwrap();
        let body =
            zstd::stream::decode_all(std::io::Cursor::new(body.as_ref()))
                .unwrap();
        let req =
            proto::opentelemetry::proto::collector::metrics::v1::ExportMetricsServiceRequest::decode(
                body.as_slice(),
            )
            .unwrap();

        let mut metrics_count = 0usize;
        let mut first_name = None;
        let mut first_i64 = None;
        let mut service_name = None;

        for resource_metrics in req.resource_metrics {
            if let Some(resource) = resource_metrics.resource {
                for attr in resource.attributes {
                    if attr.key == "service.name" {
                        let value = attr
                            .value
                            .and_then(|v| v.value)
                            .and_then(|v| match v {
                                proto::opentelemetry::proto::common::v1::any_value::Value::StringValue(s) => Some(s),
                                _ => None,
                            });
                        service_name = service_name.or(value);
                    }
                }
            }

            for scope_metrics in resource_metrics.scope_metrics {
                for metric in scope_metrics.metrics {
                    metrics_count += 1;
                    first_name.get_or_insert_with(|| metric.name.clone());

                    if first_i64.is_none() {
                        let value = metric.data.and_then(|data| match data {
                            proto::opentelemetry::proto::metrics::v1::metric::Data::Gauge(gauge) => gauge
                                .data_points
                                .into_iter()
                                .find_map(|point| point.value)
                                .and_then(|value| match value {
                                    proto::opentelemetry::proto::metrics::v1::number_data_point::Value::AsInt(i) => Some(i),
                                    _ => None,
                                }),
                            proto::opentelemetry::proto::metrics::v1::metric::Data::Sum(sum) => sum
                                .data_points
                                .into_iter()
                                .find_map(|point| point.value)
                                .and_then(|value| match value {
                                    proto::opentelemetry::proto::metrics::v1::number_data_point::Value::AsInt(i) => Some(i),
                                    _ => None,
                                }),
                            _ => None,
                        });
                        first_i64 = first_i64.or(value);
                    }
                }
            }
        }

        MetricReqObs {
            metrics: metrics_count,
            first_name: first_name.unwrap(),
            first_i64,
            dataset,
            content_type,
            content_encoding,
            service_name,
        }
    }

    fn record_req(stub: &StubServer, req: &ReqObs) -> StubResp {
        let mut obs = stub.obs.lock().unwrap();
        obs.in_flight += 1;
        obs.max_in_flight = obs.max_in_flight.max(obs.in_flight);
        obs.reqs.push(req.clone());
        drop(obs);
        stub.resps.lock().unwrap().pop_front().unwrap_or(StubResp::Ok)
    }

    fn record_metric_req(stub: &StubServer, req: &MetricReqObs) -> StubResp {
        let mut obs = stub.obs.lock().unwrap();
        obs.in_flight += 1;
        obs.max_in_flight = obs.max_in_flight.max(obs.in_flight);
        obs.metric_reqs.push(req.clone());
        drop(obs);
        stub.resps.lock().unwrap().pop_front().unwrap_or(StubResp::Ok)
    }

    fn ingest_ok(evts: usize) -> impl IntoResponse {
        axum::Json(serde_json::json!({
            "failed": 0,
            "ingested": evts,
            "processedBytes": 0,
            "failures": [],
            "blocksCreated": null,
            "walLength": null,
        }))
    }

    fn ingest_partial(evts: usize) -> impl IntoResponse {
        axum::Json(serde_json::json!({
            "failed": 1,
            "ingested": evts.saturating_sub(1),
            "processedBytes": 0,
            "failures": [{
                "timestamp": "2026-03-19T00:00:00Z",
                "error": "boom",
            }],
            "blocksCreated": null,
            "walLength": null,
        }))
    }

    async fn ingest(
        State(stub): State<StubServer>,
        req: Request,
    ) -> impl IntoResponse {
        let req =
            decode_req(to_bytes(req.into_body(), usize::MAX).await.unwrap());
        let resp = record_req(&stub, &req);

        if stub.cfg.delay > Duration::ZERO {
            tokio::time::sleep(stub.cfg.delay).await;
        }

        stub.obs.lock().unwrap().in_flight -= 1;

        match resp {
            StubResp::Ok => (
                axum::http::StatusCode::OK,
                ingest_ok(req.evts).into_response(),
            ),
            StubResp::Http400 => (
                axum::http::StatusCode::BAD_REQUEST,
                ingest_ok(req.evts).into_response(),
            ),
            StubResp::Http500 => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                ingest_ok(req.evts).into_response(),
            ),
            StubResp::Partial => (
                axum::http::StatusCode::OK,
                ingest_partial(req.evts).into_response(),
            ),
            StubResp::OkBadJson => {
                (axum::http::StatusCode::OK, "nope".into_response())
            }
        }
    }

    async fn metrics_ingest(
        State(stub): State<StubServer>,
        req: Request,
    ) -> impl IntoResponse {
        let req = decode_metric_req(req).await;
        let resp = record_metric_req(&stub, &req);

        if stub.cfg.delay > Duration::ZERO {
            tokio::time::sleep(stub.cfg.delay).await;
        }

        stub.obs.lock().unwrap().in_flight -= 1;

        match resp {
            StubResp::Ok => (
                axum::http::StatusCode::OK,
                ingest_ok(req.metrics).into_response(),
            ),
            StubResp::Http400 => (
                axum::http::StatusCode::BAD_REQUEST,
                ingest_ok(req.metrics).into_response(),
            ),
            StubResp::Http500 => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                ingest_ok(req.metrics).into_response(),
            ),
            StubResp::Partial => (
                axum::http::StatusCode::OK,
                ingest_partial(req.metrics).into_response(),
            ),
            StubResp::OkBadJson => {
                (axum::http::StatusCode::OK, "nope".into_response())
            }
        }
    }

    impl TestServer {
        async fn new(cfg: ServerCfg) -> Self {
            let addr = SocketAddr::from(([127, 0, 0, 1], 0));
            Self::new_at(cfg, addr).await
        }

        async fn new_at(cfg: ServerCfg, addr: SocketAddr) -> Self {
            let stub = StubServer {
                resps: Arc::new(Mutex::new(
                    cfg.resps.iter().copied().collect(),
                )),
                cfg,
                obs: Arc::new(Mutex::new(ServerObs::default())),
            };
            let app = Router::new()
                .route("/v1/ingest/test", post(ingest))
                .route("/v1/metrics", post(metrics_ingest))
                .with_state(stub.clone());
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (shutdown_tx, shutdown_rx) =
                tokio::sync::oneshot::channel::<()>();
            let server = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .unwrap();
            });
            Self { addr, stub, shutdown_tx, server }
        }

        fn mk_axiom_no_metrics(
            &self,
            evt_que_len: usize,
            collect_target: usize,
            sender_pool_size: usize,
        ) -> super::Axiom<TestEvt> {
            init(Config {
                met_que_len: 1,
                evt_que_len,
                service_name: "test-service",
                base_url: format!("http://{}", self.addr).parse().unwrap(),
                api_key: "test-key",
                datasets: DatasetIds::Events { dataset_id: "test" },
                collect_target,
                collect_timeout: Duration::from_secs(30),
                sender_pool_size,
            })
        }

        async fn finish(self, axiom: super::Axiom<TestEvt>) -> RunObs {
            axiom.deinit().await;
            let _ = self.shutdown_tx.send(());
            self.server.await.unwrap();

            let obs = self.stub.obs.lock().unwrap();
            RunObs {
                reqs: obs.reqs.clone(),
                metric_reqs: obs.metric_reqs.clone(),
                max_in_flight: obs.max_in_flight,
            }
        }
    }

    async fn send_evts(axiom: &super::Axiom<TestEvt>, count: u64) {
        for seq in 0..count {
            axiom
                .evt_tx
                .send(Event::Extra(TestEvt { seq, payload: None }))
                .await
                .unwrap();
        }
    }

    fn test_metric(name: impl Into<String>, value: i64) -> metrics::Metric {
        metrics::Metric {
            name: name.into(),
            description: "test metric".to_string(),
            unit: metrics::MetricUnit::Count,
            data: metrics::MetricData::Gauge {
                value: metrics::MetricValue::I64(value),
            },
            attrs: BTreeMap::new(),
        }
    }

    async fn send_mets(axiom: &super::Axiom<TestEvt>, count: u64) {
        for seq in 0..count {
            axiom
                .met_tx
                .send(test_metric(format!("test.metric.{seq}"), seq as i64))
                .await
                .unwrap();
        }
    }

    #[test]
    fn ndjson_line_len_counts_bytes() {
        let evt = EventWrapper {
            service: EventService { name: "test-service" },
            event: &Event::Extra(TestEvt { seq: 7, payload: None }),
        };
        let mut buf = Vec::new();

        let bytes_line = write_ndjson_line(&mut buf, &evt).unwrap();

        assert_eq!(bytes_line, buf.len());
        assert_eq!(buf.last(), Some(&b'\n'));
    }

    #[test]
    fn warn_json_dump_truncates_without_large_alloc() {
        #[derive(Serialize)]
        struct BigEvt<'a> {
            seq: u64,
            payload: &'a str,
        }

        static PAYLOAD: [u8; WARN_JSON_LEN_MAX * 2] =
            [b'x'; WARN_JSON_LEN_MAX * 2];

        let evt = Event::Extra(BigEvt {
            seq: 7,
            payload: std::str::from_utf8(&PAYLOAD).unwrap(),
        });
        let evt = EventWrapper {
            service: EventService { name: "test-service" },
            event: &evt,
        };
        let mut buf = Vec::with_capacity(WARN_JSON_LEN_MAX);

        {
            let (dump, trunc) = warn_json_dump(&mut buf, &evt).unwrap();

            assert!(trunc);
            assert_eq!(dump.len(), WARN_JSON_LEN_MAX);
            assert!(dump.ends_with(WARN_JSON_SUFFIX));
        }
        assert_eq!(buf.len(), WARN_JSON_LEN_MAX);
        assert!(buf.ends_with(WARN_JSON_SUFFIX.as_bytes()));
    }

    #[test]
    fn warn_json_dump_clears_reused_buf() {
        let evt = EventWrapper {
            service: EventService { name: "test-service" },
            event: &Event::Extra(TestEvt { seq: 7, payload: None }),
        };
        let mut buf = b"stale-bytes".to_vec();

        {
            let (dump, trunc) = warn_json_dump(&mut buf, &evt).unwrap();
            assert!(!trunc);
            assert!(!dump.contains("stale-bytes"));
        }
        assert!(!std::str::from_utf8(&buf).unwrap().contains("stale-bytes"));
        assert!(!buf.windows("stale-bytes".len()).any(|s| s == b"stale-bytes"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metrics_ingest_sends_protobuf_zstd_with_axiom_headers() {
        let srv =
            TestServer::new(ServerCfg { delay: Duration::ZERO, resps: vec![] })
                .await;
        let axiom = init(Config {
            met_que_len: 8,
            evt_que_len: 1,
            service_name: "test-service",
            base_url: format!("http://{}", srv.addr).parse().unwrap(),
            api_key: "test-key",
            datasets: DatasetIds::Metrics { dataset_id: "test" },
            collect_target: 4,
            collect_timeout: Duration::from_secs(30),
            sender_pool_size: 1,
        });

        axiom.met_tx.send(test_metric("test.metric", 42)).await.unwrap();
        let obs = srv.finish(axiom).await;

        assert!(obs.reqs.is_empty());
        assert_eq!(obs.metric_reqs.len(), 1);
        let req = &obs.metric_reqs[0];
        assert_eq!(req.metrics, 1);
        assert_eq!(req.first_name, "test.metric");
        assert_eq!(req.first_i64, Some(42));
        assert_eq!(req.dataset.as_deref(), Some("test"));
        assert_eq!(req.content_type.as_deref(), Some("application/x-protobuf"));
        assert_eq!(req.content_encoding.as_deref(), Some("zstd"));
        assert_eq!(req.service_name.as_deref(), Some("test-service"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metrics_shutdown_flushes_partial_batch() {
        let srv =
            TestServer::new(ServerCfg { delay: Duration::ZERO, resps: vec![] })
                .await;
        let axiom = init(Config {
            met_que_len: 8,
            evt_que_len: 1,
            service_name: "test-service",
            base_url: format!("http://{}", srv.addr).parse().unwrap(),
            api_key: "test-key",
            datasets: DatasetIds::Metrics { dataset_id: "test" },
            collect_target: 4,
            collect_timeout: Duration::from_secs(30),
            sender_pool_size: 1,
        });

        send_mets(&axiom, 3).await;
        let obs = srv.finish(axiom).await;

        assert!(obs.reqs.is_empty());
        assert_eq!(obs.metric_reqs.len(), 1);
        assert_eq!(obs.metric_reqs[0].metrics, 3);
        assert_eq!(obs.metric_reqs[0].first_name, "test.metric.0");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn all_datasets_can_use_distinct_metric_dataset() {
        let srv =
            TestServer::new(ServerCfg { delay: Duration::ZERO, resps: vec![] })
                .await;
        let axiom = init(Config {
            met_que_len: 8,
            evt_que_len: 8,
            service_name: "test-service",
            base_url: format!("http://{}", srv.addr).parse().unwrap(),
            api_key: "test-key",
            datasets: DatasetIds::All {
                event_dataset_id: "test",
                metric_dataset_id: "metrics-test",
            },
            collect_target: 4,
            collect_timeout: Duration::from_secs(30),
            sender_pool_size: 1,
        });

        axiom
            .evt_tx
            .send(Event::Extra(TestEvt { seq: 0, payload: None }))
            .await
            .unwrap();
        axiom.met_tx.send(test_metric("test.metric", 42)).await.unwrap();
        let obs = srv.finish(axiom).await;

        assert_eq!(obs.reqs.len(), 1);
        assert_eq!(obs.reqs[0].start_seq, 0);
        assert_eq!(obs.metric_reqs.len(), 1);
        assert_eq!(obs.metric_reqs[0].dataset.as_deref(), Some("metrics-test"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metrics_only_closes_event_sender() {
        let srv =
            TestServer::new(ServerCfg { delay: Duration::ZERO, resps: vec![] })
                .await;
        let axiom: super::Axiom<TestEvt> = init(Config {
            met_que_len: 8,
            evt_que_len: 8,
            service_name: "test-service",
            base_url: format!("http://{}", srv.addr).parse().unwrap(),
            api_key: "test-key",
            datasets: DatasetIds::Metrics { dataset_id: "test" },
            collect_target: 4,
            collect_timeout: Duration::from_secs(30),
            sender_pool_size: 1,
        });

        let send = axiom
            .evt_tx
            .send(Event::Extra(TestEvt { seq: 0, payload: None }))
            .await;
        assert!(send.is_err());
        let obs = srv.finish(axiom).await;

        assert!(obs.reqs.is_empty());
        assert!(obs.metric_reqs.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_only_closes_metric_sender() {
        let srv =
            TestServer::new(ServerCfg { delay: Duration::ZERO, resps: vec![] })
                .await;
        let axiom = srv.mk_axiom_no_metrics(8, 4, 1);

        let send = axiom.met_tx.send(test_metric("test.metric", 42)).await;
        assert!(send.is_err());
        let obs = srv.finish(axiom).await;

        assert!(obs.reqs.is_empty());
        assert!(obs.metric_reqs.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingest_sender_pool_1_preserves_request_order() {
        let srv = TestServer::new(ServerCfg {
            delay: Duration::from_millis(50),
            resps: vec![],
        })
        .await;
        let axiom = srv.mk_axiom_no_metrics(8, 2, 1);

        send_evts(&axiom, 4).await;
        let obs = srv.finish(axiom).await;

        assert_eq!(obs.reqs.len(), 2);
        assert_eq!(obs.reqs[0].start_seq, 0);
        assert_eq!(obs.reqs[1].start_seq, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ingest_sender_pool_gt_1_allows_concurrent_sends() {
        let srv = TestServer::new(ServerCfg {
            delay: Duration::from_millis(150),
            resps: vec![],
        })
        .await;
        let axiom = srv.mk_axiom_no_metrics(8, 1, 2);

        send_evts(&axiom, 4).await;
        let obs = srv.finish(axiom).await;

        assert!(obs.max_in_flight >= 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ingest_full_sender_pool_pushes_backpressure_upstream() {
        let srv = TestServer::new(ServerCfg {
            delay: Duration::from_millis(250),
            resps: vec![],
        })
        .await;
        let axiom = srv.mk_axiom_no_metrics(1, 1, 1);

        send_evts(&axiom, 3).await;

        {
            let evt_tx = axiom.evt_tx.clone();
            let send =
                evt_tx.send(Event::Extra(TestEvt { seq: 3, payload: None }));
            tokio::pin!(send);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut send)
                    .await
                    .is_err()
            );
        }

        let _ = srv.finish(axiom).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ingest_retry_holds_sender_slot() {
        let srv = TestServer::new(ServerCfg {
            delay: Duration::ZERO,
            resps: vec![StubResp::Http500],
        })
        .await;
        let axiom = srv.mk_axiom_no_metrics(8, 1, 1);

        send_evts(&axiom, 2).await;
        let obs = srv.finish(axiom).await;

        assert_eq!(
            obs.reqs.iter().map(|req| req.start_seq).collect::<Vec<_>>(),
            vec![0, 0, 1]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingest_http_400_does_not_retry() {
        let srv = TestServer::new(ServerCfg {
            delay: Duration::ZERO,
            resps: vec![StubResp::Http400],
        })
        .await;
        let axiom = srv.mk_axiom_no_metrics(8, 1, 1);

        send_evts(&axiom, 1).await;
        let obs = srv.finish(axiom).await;

        assert_eq!(obs.reqs.len(), 1);
        assert_eq!(obs.reqs[0].start_seq, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingest_partial_does_not_retry() {
        let srv = TestServer::new(ServerCfg {
            delay: Duration::ZERO,
            resps: vec![StubResp::Partial],
        })
        .await;
        let axiom = srv.mk_axiom_no_metrics(8, 1, 1);

        send_evts(&axiom, 1).await;
        let obs = srv.finish(axiom).await;

        assert_eq!(obs.reqs.len(), 1);
        assert_eq!(obs.reqs[0].start_seq, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingest_http_500_retries() {
        let srv = TestServer::new(ServerCfg {
            delay: Duration::ZERO,
            resps: vec![StubResp::Http500],
        })
        .await;
        let axiom = srv.mk_axiom_no_metrics(8, 1, 1);

        send_evts(&axiom, 1).await;
        let obs = srv.finish(axiom).await;

        assert_eq!(obs.reqs.len(), 2);
        assert_eq!(obs.reqs[0].start_seq, 0);
        assert_eq!(obs.reqs[1].start_seq, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingest_connect_retries() {
        let addr = {
            let listener = std::net::TcpListener::bind(SocketAddr::from((
                [127, 0, 0, 1],
                0,
            )))
            .unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            addr
        };
        let axiom = init(Config {
            met_que_len: 8,
            evt_que_len: 8,
            service_name: "test-service",
            base_url: format!("http://{}", addr).parse().unwrap(),
            api_key: "test-key",
            datasets: DatasetIds::Events { dataset_id: "test" },
            collect_target: 1,
            collect_timeout: Duration::from_secs(30),
            sender_pool_size: 1,
        });

        send_evts(&axiom, 1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;

        let srv = TestServer::new_at(
            ServerCfg { delay: Duration::ZERO, resps: vec![] },
            addr,
        )
        .await;
        let obs = srv.finish(axiom).await;

        assert_eq!(obs.reqs.len(), 1);
        assert_eq!(obs.reqs[0].start_seq, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingest_malformed_success_body_does_not_retry() {
        let srv = TestServer::new(ServerCfg {
            delay: Duration::ZERO,
            resps: vec![StubResp::OkBadJson],
        })
        .await;
        let axiom = srv.mk_axiom_no_metrics(8, 1, 1);

        send_evts(&axiom, 1).await;
        let obs = srv.finish(axiom).await;

        assert_eq!(obs.reqs.len(), 1);
        assert_eq!(obs.reqs[0].start_seq, 0);
    }

    #[derive(Clone, Copy)]
    enum MaybeBadEvt {
        Good(u64),
        Bad,
    }

    impl serde::Serialize for MaybeBadEvt {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            match self {
                Self::Good(seq) => {
                    TestEvt { seq: *seq, payload: None }.serialize(serializer)
                }
                Self::Bad => Err(serde::ser::Error::custom("boom")),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingest_drops_bad_batch_and_keeps_running() {
        let srv =
            TestServer::new(ServerCfg { delay: Duration::ZERO, resps: vec![] })
                .await;
        let axiom = init(Config {
            met_que_len: 8,
            evt_que_len: 8,
            service_name: "test-service",
            base_url: format!("http://{}", srv.addr).parse().unwrap(),
            api_key: "test-key",
            datasets: DatasetIds::Events { dataset_id: "test" },
            collect_target: 2,
            collect_timeout: Duration::from_secs(30),
            sender_pool_size: 1,
        });

        axiom.evt_tx.send(Event::Extra(MaybeBadEvt::Good(0))).await.unwrap();
        axiom.evt_tx.send(Event::Extra(MaybeBadEvt::Bad)).await.unwrap();
        axiom.evt_tx.send(Event::Extra(MaybeBadEvt::Good(2))).await.unwrap();
        axiom.evt_tx.send(Event::Extra(MaybeBadEvt::Good(3))).await.unwrap();
        axiom.deinit().await;

        let _ = srv.shutdown_tx.send(());
        srv.server.await.unwrap();
        let obs = srv.stub.obs.lock().unwrap();

        assert_eq!(obs.reqs.len(), 1);
        assert_eq!(obs.reqs[0].start_seq, 2);
        assert_eq!(obs.reqs[0].evts, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingest_shutdown_flushes_partial_batch() {
        let srv =
            TestServer::new(ServerCfg { delay: Duration::ZERO, resps: vec![] })
                .await;
        let axiom = srv.mk_axiom_no_metrics(8, 4, 1);

        send_evts(&axiom, 3).await;
        let obs = srv.finish(axiom).await;

        assert_eq!(obs.reqs.len(), 1);
        assert_eq!(obs.reqs[0].start_seq, 0);
        assert_eq!(obs.reqs[0].evts, 3);
    }
}
