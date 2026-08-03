# tracing-axiom

[Axiom.co](https://axiom.co) backend for the tracing crate, including wrapper around axiom ingests for events and metrics. 

Metrics ingest relies on vendoring opentelemetry protobuf files rather than us pulling in their rust crate as dependency.
These files can be sourced [here](https://github.com/open-telemetry/opentelemetry-proto/tree/main/opentelemetry/proto), are stored in `/proto`.

## Usage

Assumptions:
- `tokio` async runtime.
- `data` field configured as a map field in your Axiom dataset.
- `base_url` set to your org's Axiom edge deployment base domain:
  <https://axiom.co/docs/reference/regions>
- `api_key` set per Axiom ingest auth docs:
  <https://axiom.co/docs/restapi/ingest>

```rs
let axiom: tracing_axiom::Axiom =
    tracing_axiom::init(tracing_axiom::Config {
        evt_que_len: 4 << 10,
        met_que_len: 4 << 10, 
        service_name: "example-service", 
        base_url: "https://us-east-1.aws.edge.axiom.co".parse().unwrap(),
        api_key: &api_key,
        datasets: tracing_axiom::DatasetIds::All {
            event_dataset_id: "example-dataset",
            metric_dataset_id: "example-dataset",
        },
        collect_target: 4 << 10,
        collect_timeout: std::time::Duration::from_millis(500),
        sender_pool_size: 1,
    });

// NOTE: can clone `axiom.evt_tx` and send custom events to it as long as they
//       implement `serde::Serialize`.

let subscriber = tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer())
    .with(tracing_axiom::layer(axiom.evt_tx.clone().downgrade()));
tracing::subscriber::set_global_default(subscriber).unwrap();

// Don't forget to deinit! Drop will panic! (if not panicking already)
axiom.deinit().await;
```

See `examples/simple.rs` for a working example.

## Structured JSON fields

The optional `serde` feature records values selected with tracing's `@` sigil
as structured JSON rather than Debug strings:

```rust
tracing::info!(x = @vec![1u64, 2, 3], "...");
```

This support currently requires the [serde-enabled tracing fork](https://github.com/n1xyz/tracing/tree/serde-at). Enable the feature
on both dependencies:

```toml
[dependencies]
tracing = { version = "0.1", features = ["serde"] }
tracing-axiom-n1 = { version = "0.1", features = ["serde"] }
```

Then patch any tracing crates used in the workspace root `Cargo.toml`:

```toml
[patch.crates-io]
tracing = { git = "https://github.com/n1xyz/tracing.git", rev = "5989b57242d84ed144776aa54b2d879fb098cff2" }
tracing-core = { git = "https://github.com/n1xyz/tracing.git", rev = "5989b57242d84ed144776aa54b2d879fb098cff2" }
tracing-serde = { git = "https://github.com/n1xyz/tracing.git", rev = "5989b57242d84ed144776aa54b2d879fb098cff2" }
tracing-subscriber = { git = "https://github.com/n1xyz/tracing.git", rev = "5989b57242d84ed144776aa54b2d879fb098cff2" }
```
