use rand::{Rng, SeedableRng, rngs};
use serde::{
    Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq},
};
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    num::{NonZeroU64, NonZeroU128},
};
use time::OffsetDateTime;
use tracing::field::{Field, Visit};

pub mod background;
pub mod builder;
pub mod layer;

pub use builder::{AXIOM_SERVER_EU, AXIOM_SERVER_US, Builder, builder};
pub use reqwest::Url;

// see https://axiom.co/docs/query-data/traces?utm_source=chatgpt.com#trace-schema-overview for Axiom trace schema
pub const OTEL_FIELD_SPAN_ID: &str = "span_id";
pub const OTEL_FIELD_TRACE_ID: &str = "trace_id";
pub const OTEL_FIELD_PARENT_ID: &str = "parent_span_id";
pub const OTEL_FIELD_NAME: &str = "name";
pub const OTEL_FIELD_KIND: &str = "kind";
pub const OTEL_FIELD_DURATION_MS: &str = "duration";
pub const EVENT_LEVEL: &str = "level";
pub const EVENT_TIMESTAMP: &str = "_time";
pub const EVENT_NAME: &str = "name";
pub const ATTR_ANNOTATION_TYPE: &str = "annotation_type";
pub const ATTR_IDLE_NS: &str = "idle_ns";
pub const ATTR_BUSY_NS: &str = "busy_ns";
pub const ATTR_TARGET: &str = "target";
pub const RESOURCES_SERVICE_NAME: &str = "service.name";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    // fields whose value is `null` seem to be ignored by Honeycomb, so no Null variant
    // arrays and objects are not supported
    Bool(bool),
    Number(serde_json::Number),
    String(Cow<'static, str>),
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

macro_rules! from_integer {
    ($($ty:ident)*) => {
        $(
            impl From<$ty> for Value {
                fn from(n: $ty) -> Self {
                    Value::Number(n.into())
                }
            }
        )*
    };
}

from_integer! {
    i8 i16 i32 i64 isize
    u8 u16 u32 u64 usize
}

impl From<f32> for Value {
    /// Convert 32-bit floating point number to `Value::Number`, or
    /// `Value::String` if infinite or NaN.
    fn from(f: f32) -> Self {
        // serde_json making Number::from_f32 private has forced my hand
        f64::from(f).into()
    }
}

impl From<f64> for Value {
    /// Convert 64-bit floating point number to `Value::Number`, or
    /// `Value::String` if infinite or NaN.
    fn from(f: f64) -> Self {
        serde_json::Number::from_f64(f)
            .map(Self::Number)
            // this is a little slimy but good behavior for honeycomb specifically
            // there's not really much else since we don't have a Null variant
            .unwrap_or_else(|| Self::String(format!("{}", f).into()))
    }
}

impl From<String> for Value {
    fn from(f: String) -> Self {
        Value::String(Cow::Owned(f))
    }
}

impl From<&str> for Value {
    fn from(f: &str) -> Self {
        Value::String(Cow::Owned(f.to_owned()))
    }
}

// we sacrifice generality here to prevent the footgun of
//  Cow<'static, str>::Borrowed(&'static str)::into() silently
//  converting into Cow::Owned
impl From<Cow<'static, str>> for Value {
    fn from(f: Cow<'static, str>) -> Self {
        Value::String(f)
    }
}

impl From<serde_json::Number> for Value {
    /// Convert `Number` to `Value::Number`.
    ///
    /// # Examples
    ///
    /// ```
    /// use serde_json::{Number, Value};
    ///
    /// let n = Number::from(7);
    /// let x: Value = n.into();
    /// ```
    fn from(f: serde_json::Number) -> Self {
        Value::Number(f)
    }
}

impl Serialize for Value {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Bool(b) => serializer.serialize_bool(*b),
            Self::Number(n) => n.serialize(serializer),
            Self::String(s) => serializer.serialize_str(s),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default, Serialize)]
pub struct Fields {
    #[serde(flatten)]
    pub fields: HashMap<Cow<'static, str>, Value>,
}

// list of reserved field names (case insensitive):
// trace_id
// span_id
// parent_span_id
// name
// kind
// duration
// [resources] service.name
// [events] _time
// [events] event_name
// [events] level
// [events] event_field
// [attributes] annotation_type
// [attributes] idle_ns
// [attributes] busy_ns
// [attributes] target

impl Fields {
    pub fn new() -> Self {
        Self {
            fields: HashMap::new(),
        }
    }

    pub fn record<T: Into<Value>>(&mut self, field: &Field, value: T) {
        self.fields
            .insert(Cow::Borrowed(field.name()), value.into());
    }
}

impl From<HashMap<Cow<'static, str>, Value>> for Fields {
    fn from(value: HashMap<Cow<'static, str>, Value>) -> Self {
        Self { fields: value }
    }
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record(field, format!("{:?}", value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record(field, value);
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, value);
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, value);
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, value);
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value);
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record(field, value.to_string());

        self.fields.insert(
            Cow::Owned(format!("{}.debug", field.name())),
            Value::String(Cow::Owned(format!("{:#?}", value))),
        );

        let mut chain: Vec<String> = Vec::new();
        let mut next_err = value.source();
        let mut i = 0;
        while let Some(err) = next_err {
            chain.push(format!("{:>4}: {}", i, err));
            next_err = err.source();
            i += 1;
        }
        self.fields.insert(
            Cow::Owned(format!("{}.chain", field.name())),
            Value::String(Cow::Owned(chain.join("\n"))),
        );
    }
}

thread_local! {
    /// Store random number generator for each thread
    static CURRENT_RNG: RefCell<rngs::SmallRng> = RefCell::new(rngs::SmallRng::from_os_rng());
}

/// A 8-byte value which identifies a given span.
///
/// The id is valid if it contains at least one non-zero byte.
#[derive(Clone, PartialEq, Eq, Copy, Hash)]
#[repr(transparent)]
pub struct SpanId(NonZeroU64);

impl SpanId {
    fn generate() -> Self {
        CURRENT_RNG.with(|rng| Self::from(rng.borrow_mut().random::<NonZeroU64>()))
    }
}

impl From<NonZeroU64> for SpanId {
    fn from(value: NonZeroU64) -> Self {
        SpanId(value)
    }
}

impl std::fmt::Debug for SpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{:016x}", self.0))
    }
}

impl std::fmt::Display for SpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{:016x}", self.0))
    }
}

impl std::fmt::LowerHex for SpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::LowerHex::fmt(&self.0, f)
    }
}

impl Serialize for SpanId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(format!("{}", self).as_ref())
    }
}

/// A 16-byte value which identifies a given trace.
///
/// The id is valid if it contains at least one non-zero byte.
#[derive(Clone, PartialEq, Eq, Copy, Hash)]
#[repr(transparent)]
pub struct TraceId(NonZeroU128);

impl TraceId {
    fn generate() -> Self {
        CURRENT_RNG.with(|rng| Self::from(rng.borrow_mut().random::<NonZeroU128>()))
    }
}

impl From<NonZeroU128> for TraceId {
    fn from(value: NonZeroU128) -> Self {
        TraceId(value)
    }
}

impl std::fmt::Debug for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{:032x}", self.0))
    }
}

impl std::fmt::Display for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{:032x}", self.0))
    }
}

impl std::fmt::LowerHex for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::LowerHex::fmt(&self.0, f)
    }
}

impl Serialize for TraceId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(format!("{}", self).as_ref())
    }
}

#[derive(Clone, PartialEq, Eq, Copy, Hash, Debug)]
pub enum SpanKind {
    CLIENT,
    INTERNAL,
    SERVER,
    PRODUCER,
    CONSUMER,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AxiomEvent {
    // see https://axiom.co/docs/query-data/traces#browse-traces-with-the-opentelemetry-app for required field names
    // first level fields
    pub span_id: Option<SpanId>,
    pub trace_id: Option<TraceId>,
    pub parent_span_id: Option<SpanId>,
    pub kind: &'static str,
    pub name: Cow<'static, str>,
    pub duration_ms: Option<u64>,
    // attributes field
    pub annotation_type: Option<Cow<'static, str>>,
    pub idle_ns: Option<u64>,
    pub busy_ns: Option<u64>,
    pub target: Cow<'static, str>,
    // events field
    pub time: OffsetDateTime,
    pub event_name: Cow<'static, str>,
    pub level: &'static str,
    pub event_field: Fields,
    // resources field
    pub service_name: Option<Cow<'static, str>>,
}

impl AxiomEvent {
    fn serialize_otel_field<M: SerializeMap>(
        &self,
        m: &mut M,
    ) -> Result<(), <M as SerializeMap>::Error> {
        // first level fields
        if let Some(span_id) = self.span_id {
            m.serialize_entry(OTEL_FIELD_SPAN_ID, &span_id)?;
        }
        if let Some(trace_id) = self.trace_id {
            m.serialize_entry(OTEL_FIELD_TRACE_ID, &trace_id)?;
        }
        if let Some(parent_span_id) = self.parent_span_id {
            m.serialize_entry(OTEL_FIELD_PARENT_ID, &parent_span_id)?;
        }
        if let Some(duration_ms) = self.duration_ms {
            m.serialize_entry(OTEL_FIELD_DURATION_MS, &duration_ms)?;
        }
        m.serialize_entry(OTEL_FIELD_NAME, self.name.as_ref())?;
        m.serialize_entry(OTEL_FIELD_KIND, self.kind)?;

        Ok(())
    }

    fn serialize_events_field<M: SerializeMap>(
        &self,
        m: &mut M,
    ) -> Result<(), <M as SerializeMap>::Error> {
        m.serialize_entry(
            EVENT_TIMESTAMP,
            &self
                .time
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(serde::ser::Error::custom)?,
        )?;

        m.serialize_entry(EVENT_NAME, self.event_name.as_ref())?;

        m.serialize_entry(EVENT_LEVEL, self.level)?;

        for (k, v) in self.event_field.fields.iter() {
            m.serialize_entry(k, v)?;
        }

        Ok(())
    }

    fn serialize_attr_field<M: SerializeMap>(
        &self,
        m: &mut M,
    ) -> Result<(), <M as SerializeMap>::Error> {
        if let Some(ref annotation_type) = self.annotation_type {
            m.serialize_entry(ATTR_ANNOTATION_TYPE, annotation_type)?;
        }
        if let Some(idle_ns) = self.idle_ns {
            m.serialize_entry(ATTR_IDLE_NS, &idle_ns)?;
        }
        if let Some(busy_ns) = self.busy_ns {
            m.serialize_entry(ATTR_BUSY_NS, &busy_ns)?;
        }
        m.serialize_entry(ATTR_TARGET, self.target.as_ref())?;
        Ok(())
    }

    fn serialize_resources_field<M: SerializeMap>(
        &self,
        m: &mut M,
    ) -> Result<(), <M as SerializeMap>::Error> {
        if let Some(ref service_name) = self.service_name {
            m.serialize_entry(RESOURCES_SERVICE_NAME, service_name)?;
        }
        Ok(())
    }
}

impl Serialize for AxiomEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut root = serializer.serialize_map(None)?;

        // first level fields
        if let Some(span_id) = &self.span_id {
            root.serialize_entry(OTEL_FIELD_SPAN_ID, span_id)?;
        }
        if let Some(trace_id) = &self.trace_id {
            root.serialize_entry(OTEL_FIELD_TRACE_ID, trace_id)?;
        }
        if let Some(parent_span_id) = &self.parent_span_id {
            root.serialize_entry(OTEL_FIELD_PARENT_ID, parent_span_id)?;
        }
        if let Some(duration_ms) = &self.duration_ms {
            root.serialize_entry(OTEL_FIELD_DURATION_MS, duration_ms)?;
        }
        root.serialize_entry(OTEL_FIELD_NAME, self.name.as_ref())?;
        root.serialize_entry(OTEL_FIELD_KIND, self.kind)?;

        // events field
        struct EventsField<'a>(&'a AxiomEvent);
        impl<'a> Serialize for EventsField<'a> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut m = serializer.serialize_map(None)?;
                self.0.serialize_events_field(&mut m)?;
                m.end()
            }
        }

        root.serialize_entry("events", &EventsField(self))?;

        // attributes field
        struct AttrField<'a>(&'a AxiomEvent);
        impl<'a> Serialize for AttrField<'a> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut m = serializer.serialize_map(None)?;
                self.0.serialize_attr_field(&mut m)?;

                m.end()
            }
        }
        root.serialize_entry("attributes", &AttrField(self))?;

        // resources field
        struct ResourcesField<'a>(&'a AxiomEvent);
        impl<'a> Serialize for ResourcesField<'a> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut m = serializer.serialize_map(None)?;
                self.0.serialize_resources_field(&mut m)?;
                m.end()
            }
        }
        root.serialize_entry("resources", &ResourcesField(self))?;

        root.end()
    }
}

pub type ExtraFields = Vec<(Cow<'static, str>, Value)>;

pub struct CreateEventsPayload<'a> {
    events: &'a Vec<AxiomEvent>,
    extra_fields: &'a ExtraFields,
}

impl<'a> Serialize for CreateEventsPayload<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut events_list = serializer.serialize_seq(None)?;
        for event in self.events.iter() {
            events_list.serialize_element(&CreateEventPayload {
                event,
                extra_fields: self.extra_fields,
            })?;
        }
        events_list.end()
    }
}

struct CreateEventPayload<'a> {
    event: &'a AxiomEvent,
    extra_fields: &'a ExtraFields,
}

impl<'a> Serialize for CreateEventPayload<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut root = serializer.serialize_map(None)?;

        // first level fields
        self.event.serialize_otel_field(&mut root)?;

        // events field
        struct EventsField<'a>(&'a AxiomEvent);
        impl<'a> Serialize for EventsField<'a> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut m = serializer.serialize_map(None)?;
                self.0.serialize_events_field(&mut m)?;
                m.end()
            }
        }

        root.serialize_entry("events", &EventsField(self.event))?;

        // attributes field
        struct AttrField<'a>(&'a AxiomEvent, &'a ExtraFields);
        impl<'a> Serialize for AttrField<'a> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut m = serializer.serialize_map(None)?;
                for (k, v) in self.1.iter() {
                    m.serialize_entry(k, v)?;
                }

                self.0.serialize_attr_field(&mut m)?;
                m.end()
            }
        }
        root.serialize_entry("attributes", &AttrField(self.event, self.extra_fields))?;

        // resources field
        struct ResourcesField<'a>(&'a AxiomEvent);
        impl<'a> Serialize for ResourcesField<'a> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut m = serializer.serialize_map(None)?;
                self.0.serialize_resources_field(&mut m)?;
                m.end()
            }
        }
        root.serialize_entry("resources", &ResourcesField(self.event))?;

        root.end()
    }
}
