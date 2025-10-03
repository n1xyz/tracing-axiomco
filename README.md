# tracing-axiomco

`tracing-axiomco` provides a `tracing` layer that exports distributed tracing information to axiom.co.

## Usage

A `tokio` runtime is required.
The builder creates the `tracing` layer, and background future to spawn, and a controller for the background future:

```rs
let (layer, task, controller) = tracing_axiomco::builder(&api_key)
    .build(tracing_axiomco::AXIOM_SERVER_US, dataset_name)
    .unwrap();
let subscriber = tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer())
    .with(layer);
tracing::subscriber::set_global_default(subscriber).unwrap();
let handle = tokio::spawn(task);

// to exit early:
controller.shutdown().await;
let _ = handle.await;
```

The events are submitted in the background using the [Ingest Data API](https://axiom.co/docs/restapi/endpoints/ingestIntoDataset).

## Distributed Tracing

OpenTelemetry submits the span and all events contained within it at the same time.
This can make debugging harder in the case of a deadlock, infinite loop, etc. because you won't be able to see events just before the issue.

In this crate, events are sent in real-time.
The events reference the span ID of the parent span, which hasn't been submitted yet.
The span is sent when it closes (by then, the duration and any fields dynamically added with `.record` are present).
Axiom will store the metadata of external spans needed for referencing, but it will not be shown as a graph edge. 

Traces are created whenever a span is created at the root-level (that is, with no other spans enclosing it).
All events and spans created within that root span are included in the trace.

Both events and spans are submitted in the same "event" format, the fields are what distinguish the two (see below section).

## Field Values

Every span/event includes a timestamp (not a field, sent out of band), a service name, and any extra fields passed to the builder.
Events and spans also have the `level`, `name`, and `target` from tracing's metadata.
The level is lowercased to match the conventions of Axiom's entity names.

Trace IDs and Span IDs are generated pseudorandomly, and follow the same hex-string format and length as in OpenTelemetry.

```
Example Span ID: 2e41f2d3b0c5c951
Example Trace ID: 56252c3cc92befb05c0e56a6993a18cc
```

Both events and spans have a `trace_id` field.
Spans have a `span_id`, and events have a corresponding `parent_span_id`.

Events have `annotation_type` set to `span_event` so that they show up as "Span Events" in the distributed trace.

Spans have a `duration` for the duration from open to close, in the unit of milliseconds.
They also have `busy_ns`, which measures time the span is actively entered, and `idle_ns`, which measures time the span is open but not entered.
Think a span that instruments an async function: `idle_ns` is time between polls, and `busy_ns` is the time spent in `poll`.

Fields set on an individual event or span should override any set by the library.

Fields whose value is an error type (`&(dyn std::error::Error + 'static)`) will expand into a few fields:

- `{name}`: `Display` format of the root error
- `{name}.debug`: `Debug` format of the root error
- `{name}.chain`: a string with the `Display` implementation of each error in the `source` chain of the root error, enumerated

All span/event messages follows Axiom's [trace schema](https://axiom.co/docs/query-data/traces#trace-schema-overview), with extra fields flattened and inserted under the `events` section.

## Performance

The layer implementation minimizes allocations to minimize blocking user code.
To reduce allocations, use a `Cow::Borrowed` with an `&'static str` for the service name and names of extra fields.
