use std::error::Error;
use std::fmt;
use std::time::Duration;
use tracing::Level;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Debug)]
struct ErrorA;

impl fmt::Display for ErrorA {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Error A")
    }
}

impl Error for ErrorA {}

#[derive(Debug)]
struct ErrorB {
    source: ErrorA,
}

impl fmt::Display for ErrorB {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Error B")
    }
}

impl Error for ErrorB {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug)]
struct ErrorC {
    source: ErrorB,
}

impl fmt::Display for ErrorC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Error C")
    }
}

impl Error for ErrorC {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

#[tokio::main]
async fn main() {
    // TODO chaneg this too when testing
    let api_key = std::env::var("HONEYCOMB_API_KEY")
        .expect("HONEYCOMB_API_KEY environment variable to be valid");
    let (layer, task, controller) = tracing_axiomco::builder(&api_key)
        .build(tracing_axiomco::AXIOM_SERVER_US, "test")
        .unwrap();
    let handle = tokio::spawn(task);
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(layer);
    tracing::subscriber::set_global_default(subscriber).unwrap();

    {
        let span = tracing::span!(Level::INFO, "global span");
        let _enter = span.enter();
        tracing::event!(tracing::Level::INFO, value = 42, "start");
        tokio::time::sleep(Duration::from_millis(100)).await;
        tracing::event!(tracing::Level::INFO, value = 42, "end");
        let err = ErrorC {
            source: ErrorB { source: ErrorA },
        };
        tracing::error!(
            error = &err as &dyn std::error::Error,
            "error opening nonexistant file"
        );
    }

    controller.shutdown().await;
    let _ = handle.await;
}
