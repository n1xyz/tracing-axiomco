// using the `simple.rs` file as template, this file directly interfaces with actual axiom api keys
// over covers the US server

use tracing::Level;
use tracing_subscriber::{Layer, filter::LevelFilter, layer::SubscriberExt};

const AXIOM_TEST_DATASET_NAME: &str = "porting_test";
const AXIOM_API_KEY: &str = "xaat-0d5d7b05-4e4e-4ae5-9126-dbf6af1923f1"; // TODO remove this hardcoded key before merging 

// thread_local! {
//     static CURRENT_RNG: RefCell<rngs::SmallRng> = RefCell::new(SeedableRng::from_os_rng());

//     // static RANDOM_LEVEL: RefCell<tracing::Level> = RefCell::new(Level::INFO);
// }

// pub(crate) fn thread_random_level() -> tracing::Level {
//     RANDOM_LEVEL.with(|lvl_cell| {
//         let mut lvl = lvl_cell.borrow_mut();
//         if *lvl == Level::INFO {
//             let n = CURRENT_RNG.with(|rng| rng.borrow_mut().random_range(0..=4));
//             *lvl = match n {
//                 0 => Level::TRACE,
//                 1 => Level::DEBUG,
//                 2 => Level::INFO,
//                 3 => Level::WARN,
//                 _ => Level::ERROR,
//             };
//         }
//         *lvl
//     })
// }

// TODO update comments
// the following tests integrates load testing (spawning 8 threads and 125 spans each, total of 1000 spans) and also directly interfaces with the axiom server
#[tokio::main(flavor = "multi_thread", worker_threads = 5)]
async fn main() {
    // let api_key =
    //     std::env::var("AXIOM_API_KEY").expect("AXIOM_API_KEY environment variable to be valid");
    let (layer, task, controller) = tracing_axiomco::builder(&AXIOM_API_KEY)
        .build(tracing_axiomco::AXIOM_SERVER_US, AXIOM_TEST_DATASET_NAME)
        .unwrap();
    let handle = tokio::spawn(task);
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(LevelFilter::DEBUG)) // TODO temporarily level filter
        .with(layer);

    tracing::subscriber::set_global_default(subscriber).unwrap();

    let mut tasks = Vec::new();

    for work_id in 0..5 {
        tasks.push(tokio::spawn(async move {
            for i in 0..85 {
                let top_span = tracing::span!(
                    Level::INFO,
                    "top span",
                    work_id = work_id,
                    span_iteration = i
                );

                let _enter = top_span.enter();

                {
                    let nested_span =
                        tracing::span!(Level::TRACE, "nested span", span_iteration = i);
                    let _nested_enter = nested_span.enter();

                    let msg: String = format!("random event No. {i}");

                    tracing::event!(Level::TRACE, message = msg, event_iteration = i);
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
