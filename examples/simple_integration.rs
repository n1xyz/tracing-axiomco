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
        .build(tracing_axiomco::AXIOM_SERVER_US, AXIOM_TEST_DATASET_NAME)
        .unwrap();
    let handle = tokio::spawn(task);
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(LevelFilter::ERROR))
        .with(layer);

    tracing::subscriber::set_global_default(subscriber).unwrap();

    let mut tasks = Vec::new();

    for work_id in 0..4 {
        tasks.push(tokio::spawn(async move {
            let payload = "x".repeat(1_047_000);
            for i in 0..40 {
                let gp_span = tracing::span!(
                    Level::INFO,
                    "gp span",
                    work_id = work_id,
                    span_iteration = i,
                    big_msg = %payload,
                );

                let _gp_enter = gp_span.enter();

                {
                    let p_span =
                        tracing::span!(Level::TRACE, "parent span", span_iteration = i, big_msg = %payload,);
                    let _p_enter = p_span.enter();

                    let msg: String = format!("random event No. {i}");

                    tracing::event!(Level::DEBUG, message = msg, event_iteration = i, big_msg = %payload,);

                    let c_span = tracing::span!(Level::INFO, "child span", span_iteration = i, big_msg = %payload);
                    let _c_enter = c_span.enter();
                    {
                        let msg: String = format!("random event No. {i} duplicate");

                        tracing::event!(Level::WARN, message = msg, event_iteration = i, big_msg = %payload, link_metadata.trace_id = "0a95aacc7f732e4831667e9ffa5129ed");
                    }
                }
            }
        }))
    }

    for t in tasks {
        let _ = t.await;
    }

    controller.shutdown().await;
    let _ = handle.await;
}
