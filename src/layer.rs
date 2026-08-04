use std::{borrow::Cow, fmt::Write, ops::DerefMut};

use quanta::Instant;
use rand::{Rng as _, SeedableRng as _};
use serde::Serialize;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tracing::{Subscriber, span};
use tracing_subscriber::registry::LookupSpan;

use crate::Event;

struct SpanExtra {
    span_id: [u8; 8],
    trace_id: [u8; 16],
    timing: Timing,
    fields: std::sync::Arc<std::sync::Mutex<FieldCascade>>,
}
struct Timing {
    start_instant: Instant,
    start_dt: OffsetDateTime,
    idle: u64,
    busy: u64,
    last: Instant,
    entered_depth: u64,
}

#[derive(Debug)]
pub struct FieldCascade {
    parent: Option<std::sync::Arc<std::sync::Mutex<FieldCascade>>>,
    fields: std::collections::HashMap<Cow<'static, str>, crate::Value>,
}

impl FieldCascade {
    pub fn record(
        &mut self,
        field: &tracing::field::Field,
        value: impl Into<crate::Value>,
    ) {
        self.fields.insert(Cow::Borrowed(field.name()), value.into());
    }

    fn serialize_fields<S>(&self, map: &mut S) -> Result<(), S::Error>
    where
        S: serde::ser::SerializeMap,
    {
        if let Some(parent) = &self.parent {
            parent.lock().unwrap().serialize_fields(&mut *map)?;
        }
        for (k, v) in &self.fields {
            map.serialize_entry(k, v)?;
        }
        Ok(())
    }
}

// manual impl as serde derive tries to do the recursion in generic
// instantiation as opposed to at runtime for some reason and this
// makes the compiler crash.
impl serde::Serialize for FieldCascade {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap as _;
        let mut map = serializer.serialize_map(None)?;
        self.serialize_fields(&mut map)?;
        map.end()
    }
}

impl tracing::field::Visit for FieldCascade {
    fn record_debug(
        &mut self,
        field: &tracing::field::Field,
        value: &dyn std::fmt::Debug,
    ) {
        self.record(field, format!("{:?}", value));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.record(field, value);
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.record(field, value);
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.record(field, value);
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record(field, value);
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record(field, value);
    }
    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.record(field, value.to_string());

        self.fields.insert(
            Cow::Owned(format!("{}.debug", field.name())),
            crate::Value::String(Cow::Owned(format!("{:#?}", value))),
        );

        let mut chain: String = String::with_capacity(1024);
        let mut next_err = value.source();
        let mut i = 0;
        while let Some(err) = next_err {
            if i > 0 {
                chain.push('\n');
            }
            write!(&mut chain, "{:>4}: {}", i, err).unwrap();
            next_err = err.source();
            i += 1;
        }

        self.fields.insert(
            Cow::Owned(format!("{}.chain", field.name())),
            crate::Value::String(Cow::Owned(chain)),
        );
    }

    #[cfg(feature = "serde")]
    fn record_serde(
        &mut self,
        field: &tracing::field::Field,
        value: &tracing::field::SerdeValue<'_>,
    ) {
        match serde_json::to_value(value.as_serialize()) {
            Ok(value) => self.record(field, crate::Value::Json(value)),
            Err(error) => {
                tracing::warn!(
                    target: crate::INTERNAL_TARGET,
                    field = field.name(),
                    ?error,
                    "failed to serialize field as JSON, recording Debug"
                );
                self.record_debug(field, value);
            }
        }
    }
}

pub struct Layer<X> {
    pub sender: mpsc::WeakSender<Event<X>>,
}

impl<X: serde::Serialize> Layer<X> {
    fn enqueue_event(&self, evt: Event<X>) {
        // NOTE: this might be fast or slow idk. maybe we should use kanal
        //       so that channel can be explicitly closed instead of
        //       relying solely on tx count.
        let Some(sender) = self.sender.upgrade() else {
            tracing::error!(
                target: crate::INTERNAL_TARGET,
                event = ?evt,
                "queue closed. dropping event"
            );
            return;
        };
        match sender.try_send(evt) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(e)) => {
                tracing::error!(
                    target: crate::INTERNAL_TARGET,
                    event = ?e,
                    "queue closed. dropping event"
                );
            }
            Err(mpsc::error::TrySendError::Full(e)) => {
                tracing::error!(
                    target: crate::INTERNAL_TARGET,
                    event = ?e,
                    "queue full. dropping event"
                );
            }
        }
    }
}

thread_local! {
    /// Store random number generator for each thread
    static CURRENT_RNG: std::cell::RefCell<rand::rngs::SmallRng> = std::cell::RefCell::new(rand::rngs::SmallRng::from_os_rng());
}

impl<X: serde::Serialize + 'static, S: Subscriber + for<'a> LookupSpan<'a>>
    tracing_subscriber::Layer<S> for Layer<X>
{
    fn on_new_span(
        &self,
        attrs: &span::Attributes<'_>,
        id: &span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = ctx.span(id).unwrap();

        let (p_trace_id, p_fields) = match span.parent().and_then(|p| {
            p.extensions()
                .get::<SpanExtra>()
                .map(|p| (p.trace_id, p.fields.clone()))
        }) {
            Some((trace_id, fields)) => (Some(trace_id), Some(fields)),
            None => (None, None),
        };

        let trace_id = p_trace_id.unwrap_or_else(|| {
            CURRENT_RNG.with(|rng| rng.borrow_mut().random())
        });

        let mut fields2 =
            FieldCascade { parent: p_fields, fields: Default::default() };
        attrs.record(&mut fields2);

        span.extensions_mut().insert(SpanExtra {
            span_id: CURRENT_RNG.with(|rng| rng.borrow_mut().random()),
            trace_id,
            timing: {
                let start_instant = Instant::now();
                let start_dt = OffsetDateTime::now_utc();
                Timing {
                    start_instant,
                    start_dt,
                    idle: 0,
                    busy: 0,
                    last: start_instant,
                    entered_depth: 0,
                }
            },
            fields: std::sync::Arc::new(std::sync::Mutex::new(fields2)),
        });
    }

    fn on_record(
        &self,
        id: &span::Id,
        values: &span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        values.record(
            ctx.span(id)
                .unwrap()
                .extensions_mut()
                .get_mut::<SpanExtra>()
                .unwrap()
                .fields
                .lock()
                .unwrap()
                .deref_mut(),
        );
    }

    fn on_enter(
        &self,
        id: &span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = ctx.span(id).unwrap();
        let mut e = span.extensions_mut();
        let Some(extra) = e.get_mut::<SpanExtra>() else {
            return;
        };

        extra.timing.entered_depth += 1;
        if extra.timing.entered_depth == 1 {
            let now = Instant::now();
            extra.timing.idle += now
                .saturating_duration_since(extra.timing.last)
                .as_nanos() as u64;
            extra.timing.last = now;
        }
    }

    fn on_exit(
        &self,
        id: &span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = ctx.span(id).unwrap();
        let mut e = span.extensions_mut();
        let Some(extra) = e.get_mut::<SpanExtra>() else {
            return;
        };

        if extra.timing.entered_depth == 1 {
            let now = Instant::now();
            extra.timing.busy += now
                .saturating_duration_since(extra.timing.last)
                .as_nanos() as u64;
            extra.timing.last = now;
        }
        // this could be -= 1 and panic on underflow
        extra.timing.entered_depth =
            extra.timing.entered_depth.saturating_sub(1);
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        // Internal ingest-task logs should go to the app's other tracing layers
        // but must never be re-enqueued back into Axiom, or we'd recurse.
        if event.metadata().target().starts_with(crate::INTERNAL_TARGET) {
            return;
        }

        let span = ctx.event_span(event);

        let (parent_span_id, trace_id, s_fields) = match span.and_then(|p| {
            p.extensions()
                .get::<SpanExtra>()
                .map(|p| (p.span_id, p.trace_id, p.fields.clone()))
        }) {
            Some((span_id, trace_id, fields)) => {
                (Some(span_id), Some(trace_id), Some(fields))
            }
            None => (None, None, None),
        };

        let mut fields =
            FieldCascade { parent: s_fields, fields: Default::default() };
        let meta = event.metadata();
        event.record(&mut fields);

        let message = match fields.fields.get("message") {
            Some(crate::Value::String(_)) => {
                let Some(crate::Value::String(message)) =
                    fields.fields.remove("message")
                else {
                    unreachable!()
                };
                Some(message)
            }
            _ => None,
        };
        self.enqueue_event(Event::Log {
            time: OffsetDateTime::now_utc(),
            kind: crate::BogusKind,
            trace_id,
            parent_span_id,
            module_path: meta.module_path().map(Cow::Borrowed),
            target: Cow::Borrowed(meta.target()),
            level: crate::Level::from(*meta.level()),
            name: Cow::Borrowed(meta.name()),
            message,
            data: fields,
        });
    }

    fn on_close(
        &self,
        id: span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let now = Instant::now();
        let span = ctx.span(&id).unwrap();

        let parent_span_id = span
            .parent()
            .and_then(|p| p.extensions().get::<SpanExtra>().map(|p| p.span_id));
        let (time, data, span_info) =
            match span.extensions_mut().remove::<SpanExtra>() {
                Some(e) => (
                    Some(e.timing.start_dt),
                    Some(e.fields),
                    Some(crate::SpanInfo {
                        span_id: e.span_id,
                        trace_id: e.trace_id,
                        duration_ns: now
                            .saturating_duration_since(e.timing.start_instant)
                            .as_nanos()
                            as u64,
                        idle_ns: e.timing.idle,
                        busy_ns: e.timing.busy,
                    }),
                ),
                None => (None, None, None),
            };

        let meta = span.metadata();

        self.enqueue_event(Event::Span {
            time: time.unwrap_or_else(OffsetDateTime::now_utc),
            kind: crate::BogusKind,
            parent_span_id,
            span_info,
            module_path: meta.module_path().map(Cow::Borrowed),
            target: Cow::Borrowed(meta.target()),
            level: crate::Level::from(*meta.level()),
            name: Cow::Borrowed(meta.name()),
            data,
        });
    }
}

pub(crate) fn ser_option_field_cascade<S>(
    fc: &Option<std::sync::Arc<std::sync::Mutex<FieldCascade>>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::ser::Serializer,
{
    match fc {
        None => serializer.serialize_none(),
        Some(fc) => fc.as_ref().serialize(serializer),
    }
}
