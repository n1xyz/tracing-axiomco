// using the `simple.rs` file as template, this file directly interfaces with actual axiom api keys
// over covers the US server

use tracing::Level;
use tracing_subscriber::{Layer, filter::LevelFilter, layer::SubscriberExt};

const AXIOM_TEST_DATASET_NAME: &str = "porting_patch";

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

    for work_id in 0..1 {
        tasks.push(tokio::spawn(async move {
            let payload = "x".repeat(5);
            for i in 0..1 {
                let gp_span = tracing::span!(
                    Level::INFO,
                    "gp span",
                    work_id = work_id,
                    span_iteration = i,
                    big_msg = %payload,
                );

                let gp_enter = gp_span.enter();

                {
                    let p_span = tracing::span!(Level::TRACE, "parent span", big_msg = %payload,);
                    let p_enter = p_span.enter();

                    {
                        let msg: String = format!("random event No. {i}");

                        tracing::event!(Level::DEBUG, message = msg, msg = %payload,);

                        let c_span = tracing::span!(Level::INFO, "child span", span_iteration = i);
                        let c_enter = c_span.entered();
                        {
                            let msg: String = format!("first child event");

                            tracing::event!(Level::WARN, message = msg);
                            
                        }
                        let c_span = c_enter.exit();

                        let c_enter = c_span.enter();
                        {
                            let msg: String = format!("second child event");

                            tracing::event!(Level::WARN, message = msg);
                        }
                        drop(c_enter);
                    }
                    drop(p_enter);
                    // std::thread::sleep(std::time::Duration::from_secs(3600))
                }
                drop(gp_enter);
            }
        }))
    }

    for t in tasks {
        t.await.unwrap();
    }
    eprintln!("All tasks completed, shutting down");

    controller.shutdown().await;
    handle.await.unwrap();
}
