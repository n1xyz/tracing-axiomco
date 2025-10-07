use criterion::{Criterion, criterion_group, criterion_main};
use std::{
    borrow::Cow,
    hint::black_box,
    pin::Pin,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    time::Instant,
};
use tokio::sync::mpsc;
use tracing::{Level, instrument::WithSubscriber, subscriber::NoSubscriber};
use tracing_axiomco::{
    CreateEventsPayload, Value,
    background::{Backend, BackgroundTaskFut},
    builder::DEFAULT_CHANNEL_SIZE,
};
use tracing_subscriber::layer::SubscriberExt;

struct NoHttpBackend {
    compression_context: zstd::zstd_safe::CCtx<'static>,
}

impl NoHttpBackend {
    fn new() -> Self {
        Self {
            compression_context: zstd::zstd_safe::CCtx::try_create()
                .expect("zstd context allocation to succeed"),
        }
    }
}

impl Backend for NoHttpBackend {
    type Err = Box<dyn std::error::Error>;
    type Fut = Pin<Box<dyn Future<Output = Result<(), Self::Err>> + Send + 'static>>;

    fn submit_events<'a>(&mut self, payload: CreateEventsPayload<'a>) -> Self::Fut {
        let mut body = Vec::with_capacity(128);
        let mut encoder = zstd::Encoder::with_context(&mut body, &mut self.compression_context);
        let () = serde_json::to_writer(&mut encoder, &payload)
                    .expect("none of the tracing field types can fail to serialize and zstd compression should succeed");
        encoder.finish().expect("zstd compression should succeed");

        Box::pin(
            async move {
                black_box(body);
                Ok(())
            }
            .with_subscriber(NoSubscriber::default()),
        )
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct RandomStruct {
    tuple_field: (String, u32),
    boolean: bool,
    list: Vec<RandomStruct>,
}

fn send_logs_synthetic() {
    let span = tracing::span!(Level::INFO, "my_span", my_answer = black_box(42));
    let _enter = span.enter();
    span.record(black_box("greeting"), black_box("hello world!"));

    let my_random_struct = black_box(RandomStruct {
        tuple_field: ("asdfasdf ghjkl".to_string(), 0x54321),
        boolean: true,
        list: vec![RandomStruct {
            tuple_field: ("ghggghhg".to_string(), 0x12345),
            boolean: false,
            list: vec![],
        }],
    });
    tracing::event!(
        Level::WARN,
        user.email.extra = ?my_random_struct,
        "my log event 1 with a decently long log message",
    );
    tracing::event!(
        Level::WARN,
        "my log event 2 with a decently long log message",
    );
    tracing::event!(
        Level::WARN,
        "my log event 3 with a decently long log message",
    );

    let span2 = tracing::span!(Level::INFO, "sub_span", field_other = black_box("abcdefgh"));
    span2.record("span2_computed_val", black_box(0xabcdef));
    span2.in_scope(|| tracing::event!(Level::ERROR, "my log event 4 inside another span"));
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

pub fn criterion_benchmark(c: &mut Criterion) {
    // note: work neeeded if criterion.rs ever starts using tracing internally
    c.bench_function("send_logs_synthetic", |b| {
        b.iter_custom(|iters| {
            let extra_fields: Vec<(Cow<'static, str>, Value)> = vec![
                ("field1".into(), Cow::Borrowed("value1").into()),
                ("field2".into(), "longer val".repeat(4).into()),
            ];

            let (sender, receiver) = mpsc::channel(DEFAULT_CHANNEL_SIZE);
            let layer = tracing_axiomco::layer::Layer::new(
                black_box(Some("my-cool-service-name".into())),
                black_box(sender.clone()),
            );
            let subscriber = tracing_subscriber::registry().with(layer);
            let mut background_task = BackgroundTaskFut::new_with_backend(
                black_box(extra_fields),
                black_box(NoHttpBackend::new()),
                black_box(receiver),
            );
            // no need to make backgroundtask controller since we will manually use blocking_send
            let waker = dummy_waker();
            let mut cx = Context::from_waker(&waker);

            tracing::subscriber::with_default(subscriber, || {
                let start = Instant::now();
                for i in 0..iters {
                    send_logs_synthetic();
                    // keep the receiver from falling behind so that the channel doesn't fill up
                    if i != 0 && i % 10 == 0 {
                        assert!(matches!(
                            Pin::new(&mut background_task).poll(&mut cx),
                            Poll::Pending
                        ));
                    }
                }
                sender.blocking_send(None).unwrap();
                #[allow(clippy::while_let_loop)]
                loop {
                    // we won't panic on tokio sleep() creation without active runtime
                    // because backend always returns Ok so no backoff needed
                    match Pin::new(&mut background_task).poll(&mut cx) {
                        Poll::Pending => {}
                        Poll::Ready(()) => break,
                    }
                }

                start.elapsed()
            })
        })
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(300);
    targets = criterion_benchmark
}
criterion_main!(benches);
