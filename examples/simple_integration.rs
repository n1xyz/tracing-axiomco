// using the `simple.rs` file as template, this file directly interfaces with actual axiom api keys
// over covers the US server

use tracing::Level;
use tracing_subscriber::{Layer, filter::LevelFilter, layer::SubscriberExt};

const AXIOM_TEST_DATASET_NAME: &str = "porting_test";

// the following tests integrates load testing (total of 1024 events)
// 1024 events max sent due to channel limit -- rest should be dropped
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let api_key =
        std::env::var("AXIOM_API_KEY").expect("AXIOM_API_KEY environment variable to be valid");
    let (layer, task, controller) = tracing_axiomco::builder(&api_key)
        .service_name(std::borrow::Cow::Borrowed("test integration"))
        .build(tracing_axiomco::AXIOM_SERVER_US, AXIOM_TEST_DATASET_NAME)
        .unwrap();
    let handle = tokio::spawn(task);
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(LevelFilter::INFO))
        .with(layer);

    tracing::subscriber::set_global_default(subscriber).unwrap();

    let mut tasks = Vec::new();

    tasks.push(tokio::spawn(async move {
        let payload = "x".repeat(5);
        for i in 0..1 {
            let gp_span = tracing::span!(
                Level::INFO,
                "gp span",
                span_iteration = i,
                big_msg = %payload,
            );

            let gp_enter = gp_span.enter();
            std::mem::forget(gp_enter);

            tracing::event!(Level::INFO, message = "hello gp");

            {
                let p_span = tracing::span!(Level::TRACE, "parent span");
                let _p_enter = p_span.enter();

                let msg: String = format!("random event No. {i}");

                tracing::event!(Level::INFO, message = msg);

                let c_span = tracing::span!(Level::INFO, "child span").entered();

                {
                    let msg: String = "first child event".to_string();

                    tracing::event!(Level::WARN, message = msg);
                }
                let c2_span = c_span.exit();

                let c2_enter = c2_span.enter();
                {
                    let msg: String = "second child event".to_string();

                    tracing::event!(Level::WARN, message = msg);
                }
                drop(c2_enter);
            }
        }
    }));

    for t in tasks {
        t.await.unwrap();
    }

    controller.shutdown().await;
    handle.await.unwrap();
}
