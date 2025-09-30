use quanta::Instant;
use std::borrow::Cow;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tracing::{Level, Subscriber, span};
use tracing_subscriber::registry::LookupSpan;

use crate::{AxiomEvent, Fields, SpanId, SpanKind, TraceId};

fn level_as_axiom_str(level: &Level) -> &'static str {
    match *level {
        Level::TRACE => "trace",
        Level::DEBUG => "debug",
        Level::INFO => "info",
        Level::WARN => "warn",
        Level::ERROR => "error",
    }
}

fn kind_as_axiom_str(kind: &SpanKind) -> &'static str {
    match *kind {
        SpanKind::INTERNAL => "internal",
        SpanKind::SERVER => "server",
        SpanKind::CLIENT => "client",
        SpanKind::PRODUCER => "producer",
        SpanKind::CONSUMER => "consumer",
    }
}

struct Timings {
    start_instant: Instant,
    start_dt: OffsetDateTime,
    idle: u64,
    busy: u64,
    last: Instant,
    entered_depth: u64,
}

impl Timings {
    fn new() -> Self {
        let start_instant = Instant::now();
        let start_dt = OffsetDateTime::now_utc();
        Self {
            start_instant,
            start_dt,
            idle: 0,
            busy: 0,
            last: start_instant,
            entered_depth: 0,
        }
    }
}

pub struct Layer {
    service_name: Option<Cow<'static, str>>,
    sender: mpsc::Sender<Option<AxiomEvent>>,
}

impl Layer {
    pub fn new(
        service_name: Option<Cow<'static, str>>,
        sender: mpsc::Sender<Option<AxiomEvent>>,
    ) -> Self {
        Self {
            service_name,
            sender,
        }
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> tracing_subscriber::Layer<S> for Layer {
    fn on_new_span(
        &self,
        attrs: &span::Attributes<'_>,
        id: &span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = ctx
            .span(id)
            .expect("span passed to on_new_span is open, valid, and stored by subscriber");
        let mut extensions = span.extensions_mut();
        debug_assert!(
            extensions.get_mut::<Fields>().is_none(),
            "on_new_span should only ever be called once, something is buggy"
        );
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        extensions.insert(fields);
        let timings = Timings::new();
        extensions.insert(timings);
        let trace_id = span
            .parent()
            .and_then(|p| p.extensions().get::<TraceId>().copied())
            .unwrap_or_else(TraceId::generate);
        extensions.insert(trace_id);
        extensions.insert(SpanId::generate());
    }

    fn on_record(
        &self,
        id: &span::Id,
        values: &span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = ctx
            .span(id)
            .expect("span passed to on_new_span is open, valid, and stored by subscriber");
        let mut extensions = span.extensions_mut();
        let fields = extensions
            .get_mut::<Fields>()
            .expect("fields extension was inserted by on_new_span");
        values.record(fields);
    }

    fn on_enter(&self, id: &span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let span = ctx
            .span(id)
            .expect("span passed to on_enter is open, valid, and stored by subscriber");
        if let Some(timings) = span.extensions_mut().get_mut::<Timings>() {
            if timings.entered_depth == 0 {
                let now = Instant::now();
                timings.idle += now.saturating_duration_since(timings.last).as_nanos() as u64;
                timings.last = now;
            }
            timings.entered_depth = timings.entered_depth.saturating_add(1);
        }
    }

    fn on_exit(&self, id: &span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let span = ctx
            .span(id)
            .expect("span passed to on_exit is open, valid, and stored by subscriber");
        if let Some(timings) = span.extensions_mut().get_mut::<Timings>() {
            if timings.entered_depth == 0 {
                let now = Instant::now();
                timings.busy += now.saturating_duration_since(timings.last).as_nanos() as u64;
                timings.last = now;
            }
            timings.entered_depth = timings.entered_depth.saturating_sub(0);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let span = event.parent().and_then(|id| ctx.span(id)).or_else(|| {
            event
                .is_contextual()
                .then(|| ctx.lookup_current())
                .flatten()
        });
        let timestamp = OffsetDateTime::now_utc();
        // removed: tracing-log support by calling .normalized_metadata()
        let meta = event.metadata();
        let mut fields = Fields::new();
        if let Some(ref associated_span) = span {
            for span in associated_span.scope().from_root() {
                fields.fields.extend(
                    span.extensions()
                        .get::<Fields>()
                        .expect("span registered in on_new_span")
                        .fields
                        .iter()
                        .map(|(field, val)| (field.clone(), val.clone())),
                );
            }
        }
        event.record(&mut fields);
        // don't care if channel closed. if capacity is reached, we have larger problems
        let _ = self.sender.try_send(Some(AxiomEvent {
            time: timestamp,
            span_id: None,
            trace_id: span
                .as_ref()
                .and_then(|s| s.extensions().get::<TraceId>().copied()),
            parent_span_id: span.and_then(|s| s.extensions().get::<SpanId>().copied()),
            service_name: self.service_name.clone(),
            annotation_type: Some(Cow::Borrowed("span_event")),
            duration_ms: None,
            idle_ns: None,
            busy_ns: None,
            level: level_as_axiom_str(meta.level()),
            name: Cow::Borrowed(meta.name()),
            target: Cow::Borrowed(meta.target()),
            fields,
            kind: kind_as_axiom_str(&SpanKind::INTERNAL),
        }));
    }

    fn on_close(&self, id: span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let now = Instant::now();
        let span = ctx
            .span(&id)
            .expect("span passed to on_new_span is open, valid, and stored by subscriber");
        let mut fields = Fields::new();

        let mut duration_ms = None;
        let mut idle_ns = None;
        let mut busy_ns = None;
        let timestamp = if let Some(timings) = span.extensions().get::<Timings>() {
            duration_ms = Some(
                now.saturating_duration_since(timings.start_instant)
                    .as_millis() as u64,
            );
            idle_ns = Some(timings.idle);
            busy_ns = Some(timings.busy);
            timings.start_dt
        } else {
            OffsetDateTime::now_utc()
        };

        let meta = span.metadata();
        let parent_id = span.parent().map(|p| p.id());
        if let Some(scope_iter) = parent_id.as_ref().and_then(|p_id| ctx.span_scope(p_id)) {
            for span in scope_iter.from_root() {
                fields.fields.extend(
                    span.extensions()
                        .get::<Fields>()
                        .expect("span registered in on_new_span")
                        .fields
                        .iter()
                        .map(|(field, val)| (field.clone(), val.clone())),
                );
            }
        }
        fields.fields.extend(
            span.extensions()
                .get::<Fields>()
                .expect("fields extension was inserted by on_new_span")
                .fields
                .clone(),
        );
        span.extensions_mut().remove::<Fields>();
        let trace_id = span.extensions_mut().remove::<TraceId>();

        let _ = self.sender.try_send(Some(AxiomEvent {
            time: timestamp,
            span_id: span.extensions_mut().remove::<SpanId>(),
            trace_id,
            parent_span_id: span
                .parent()
                .and_then(|p| p.extensions().get::<SpanId>().copied()),
            service_name: self.service_name.clone(),
            annotation_type: None,
            duration_ms,
            idle_ns,
            busy_ns,
            level: level_as_axiom_str(meta.level()),
            name: Cow::Borrowed(meta.name()),
            target: Cow::Borrowed(meta.target()),
            fields,
            kind: kind_as_axiom_str(&SpanKind::INTERNAL),
        }));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::{
        EVENT_LEVEL, EVENT_TIMESTAMP, OTEL_FIELD_PARENT_ID, OTEL_FIELD_SERVICE_NAME,
        OTEL_FIELD_SPAN_ID,
    };

    use super::*;
    use serde_json::{Value, json};
    use tracing::Level;
    use tracing_subscriber::{Registry, layer::SubscriberExt};

    fn check_ev_map_depth_one(ev_map: &serde_json::Map<String, Value>) {
        for (key, val) in ev_map.iter() {
            assert!(
                !matches!(val, Value::Object(_)),
                "event is not depth one: key {:#?} = {:#?}",
                key,
                val
            );
        }
    }

    fn get_span_id(span: &tracing::Span) -> SpanId {
        tracing::Dispatch::default()
            .downcast_ref::<Registry>()
            .unwrap()
            .span(&span.id().unwrap())
            .unwrap()
            .extensions()
            .get::<SpanId>()
            .copied()
            .unwrap()
    }

    #[test]
    fn tracing_layer() {
        let (sender, mut receiver) = mpsc::channel(16384);
        let layer = Layer {
            service_name: Some("service_name".into()),
            sender,
        };
        let subscriber = tracing_subscriber::registry().with(layer);

        let (or_val_gp, or_val_p, or_val_c, or_val_e) = (0, 1, 2, 3);
        let (gp_val, p_val, c_val, e_val) = (40, 41, 42, 43);

        let before = OffsetDateTime::now_utc();
        let (grandparent_id, parent_id, child_id) =
            tracing::subscriber::with_default(subscriber, || {
                let grandparent_span = tracing::span!(
                    Level::DEBUG,
                    "grandparent span",
                    overridden_field = or_val_gp,
                    grandparent_field = gp_val
                );
                let _gp_enter = grandparent_span.enter();

                let parent_span = tracing::span!(
                    Level::DEBUG,
                    "parent span",
                    overridden_field = or_val_p,
                    parent_field = p_val
                );
                let _p_enter = parent_span.enter();

                let child_span = tracing::span!(
                    Level::TRACE,
                    "child span",
                    overridden_field = or_val_c,
                    child_field = c_val
                );
                let _enter = child_span.enter();

                tracing::event!(
                    Level::INFO,
                    overridden_field = or_val_e,
                    event_field = e_val,
                    "my event"
                );
                (
                    get_span_id(&grandparent_span),
                    get_span_id(&parent_span),
                    get_span_id(&child_span),
                )
            });
        let after = OffsetDateTime::now_utc();

        let num_events = receiver.len();
        assert_eq!(
            num_events,
            4,
            "expected 4 events after test, got {}",
            receiver.len()
        );
        let mut events = Vec::with_capacity(num_events);
        assert_eq!(
            receiver.blocking_recv_many(&mut events, num_events),
            num_events,
            "expected to receive all events in one go"
        );
        let events = events.into_iter().map(|i| i.unwrap()).collect::<Vec<_>>();
        assert_eq!(
            events.iter().map(|evt| evt.trace_id).collect::<Vec<_>>(),
            std::iter::repeat_n(Some(events[0].trace_id.unwrap()), events.len())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            events
                .iter()
                .map(|evt| (evt.parent_span_id, evt.span_id))
                .collect::<Vec<_>>(),
            vec![
                (Some(child_id), None),                  // the event
                (Some(parent_id), Some(child_id)),       // child_span closing
                (Some(grandparent_id), Some(parent_id)), // parent_span closing
                (None, Some(grandparent_id))             // grandparent_span closing
            ]
        );
        assert_eq!(
            events
                .iter()
                .map(|evt| evt.fields.fields.get("overridden_field"))
                .collect::<Vec<_>>(),
            vec![
                (Some(&or_val_e.into())),  // the event
                (Some(&or_val_c.into())),  // child_span closing
                (Some(&or_val_p.into())),  // parent_span closing
                (Some(&or_val_gp.into()))  // grandparent_span closing
            ]
        );

        let log_event = &events[0];
        let root = match serde_json::to_value(log_event).unwrap() {
            Value::Object(root) => root,
            val => panic!(
                "expected event to serialize into map, instead got {:#?}",
                val
            ),
        };
        let ev_map = match root.get("data").unwrap() {
            Value::Object(data) => data,
            _ => panic!("data key has unexpected type"),
        };
        check_ev_map_depth_one(ev_map);

        assert_eq!(ev_map.get(OTEL_FIELD_SPAN_ID), None);
        assert_eq!(ev_map.get(OTEL_FIELD_PARENT_ID), Some(&json!(child_id)));
        assert_eq!(
            ev_map.get(OTEL_FIELD_SERVICE_NAME),
            Some(&json!("service_name"))
        );
        assert_eq!(ev_map.get(EVENT_LEVEL), Some(&json!("info")));
        assert!(
            before <= log_event.time && log_event.time <= after,
            "invalid timestamp: {:#?}",
            ev_map.get(EVENT_TIMESTAMP)
        );
        // "name" field is based on line number so cannot be easily checked
        assert_eq!(
            ev_map.get("target"),
            Some(&json!(format!(
                "{}::layer::tests",
                env!("CARGO_CRATE_NAME")
            )))
        );

        assert_eq!(ev_map.get("event_field"), Some(&json!(e_val)));
        assert_eq!(ev_map.get("child_field"), Some(&json!(c_val)));
        assert_eq!(ev_map.get("parent_field"), Some(&json!(p_val)));
        assert_eq!(ev_map.get("grandparent_field"), Some(&json!(gp_val)));
        assert_eq!(ev_map.get("overridden_field"), Some(&json!(or_val_e)));

        let child_closing_event = events.get(1).unwrap();
        assert_eq!(
            child_closing_event.fields.fields.get("child_field"),
            Some(&42.into())
        );

        let parent_closing_event = &events[2];
        let root = match serde_json::to_value(parent_closing_event).unwrap() {
            Value::Object(root) => root,
            val => panic!(
                "expected event to serialize into map, instead got {:#?}",
                val
            ),
        };
        let ev_map = match root.get("data").unwrap() {
            Value::Object(data) => data,
            _ => panic!("data key has unexpected type"),
        };
        check_ev_map_depth_one(ev_map);

        assert_eq!(ev_map.get(OTEL_FIELD_SPAN_ID), Some(&json!(parent_id)));
        assert_eq!(
            ev_map.get(OTEL_FIELD_PARENT_ID),
            Some(&json!(grandparent_id))
        );
        assert_eq!(ev_map.get("event_field"), None);
        assert_eq!(ev_map.get("child_field"), None);
        assert_eq!(ev_map.get("parent_field"), Some(&json!(p_val)));
        assert_eq!(ev_map.get("grandparent_field"), Some(&json!(gp_val)));
        assert_eq!(ev_map.get("overridden_field"), Some(&json!(or_val_p)));
    }

    #[test]
    fn explicit_parent() {
        let (sender, mut receiver) = mpsc::channel(16384);
        let layer = Layer {
            service_name: Some("service_name".into()),
            sender,
        };
        let subscriber = tracing_subscriber::registry().with(layer);

        let parent_id = tracing::subscriber::with_default(subscriber, || {
            let active_span = span!(Level::INFO, "active_span");
            let _guard = active_span.enter();

            let parent = span!(Level::INFO, "explicit_parent", parent_field = 40);
            let parent_id = parent.id().unwrap();
            tracing::event!(parent: &parent_id, Level::INFO, "message");

            get_span_id(&parent)
        });

        // assert_eq!(receiver.len(), 1);
        let mut events = Vec::with_capacity(1);
        assert_eq!(receiver.blocking_recv_many(&mut events, 128), 3);
        let event = events[0].take().unwrap();
        assert_eq!(event.fields.fields.get("message"), Some(&"message".into()));
        assert_eq!(event.span_id, None);
        assert_eq!(event.parent_span_id, Some(parent_id));
    }
}
