use rand::{Rng, SeedableRng, rngs};
use serde::{Serialize, Serializer, ser::SerializeSeq};
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

// see https://axiom.co/docs/query-data/traces?utm_source=chatgpt.com#trace-schema-overview for Axiom trace schema
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

fn serialize_time<S>(time: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let s = time
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(serde::ser::Error::custom)?;
    serializer.serialize_str(&s)
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct OtelField {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_id: Option<SpanId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<TraceId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<SpanId>,
    pub kind: &'static str,
    pub name: Cow<'static, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(rename = "_time", serialize_with = "serialize_time")]
    pub time: OffsetDateTime,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct AttributeField {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotation_type: Option<Cow<'static, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy_ns: Option<u64>,
    pub target: Cow<'static, str>,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct EventField {
    pub event_name: Cow<'static, str>,
    pub level: &'static str,
    #[serde(flatten)]
    pub event_field: Fields,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct ResourceField {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_name: Option<Cow<'static, str>>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct AxiomEvent {
    // see https://axiom.co/docs/query-data/traces#browse-traces-with-the-opentelemetry-app for required field names
    // first level fields
    #[serde(flatten)]
    pub otel: OtelField,
    // attributes field
    pub attributes: AttributeField,
    // events field
    pub events: EventField,
    // resources field
    pub resources: ResourceField,
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

#[derive(Serialize)]
struct CreateEventPayload<'a> {
    #[serde(flatten)]
    event: &'a AxiomEvent,
    #[serde(skip_deserializing)]
    extra_fields: &'a ExtraFields,
}
