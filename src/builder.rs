use crate::{
    ExtraFields,
    Url,
    Value,
    background::{BackgroundTask, BackgroundTaskController}, // TODO link layer to builder docs
};
use reqwest::header::{self, HeaderMap, HeaderName};
use std::borrow::Cow;
use tokio::sync::mpsc;

pub const AXIOM_SERVER_US: &str = "https://api.axiom.co/";
pub const AXIOM_SERVER_EU: &str = "https://api.eu.axiom.co/";

pub const DEFAULT_CHANNEL_SIZE: usize = 1024;

/// Builder for constructing a [`Layer`] and its corresponding
/// [`BackgroundTask`].
pub struct Builder {
    pub service_name: Option<Cow<'static, str>>,
    pub extra_fields: ExtraFields,
    pub http_headers: reqwest::header::HeaderMap,

    pub event_channel_size: usize,
}

#[derive(Debug)]
pub enum AddHeaderError {
    InvalidHttpHeaderName(String),
    InvalidHttpHeaderValue(String),
}

impl std::fmt::Display for AddHeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidHttpHeaderName(name) => write!(f, "invalid HTTP header name {:?}", name),
            Self::InvalidHttpHeaderValue(val) => write!(f, "invalid HTTP header value {:?}", val),
        }
    }
}

impl std::error::Error for AddHeaderError {}

#[derive(Debug)]
pub struct InvalidEndpointConfig {
    api_host: String,
    dataset_name: String,
}

impl std::fmt::Display for InvalidEndpointConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot build Axiom endpoint from API host {:?} and dataset {:?}",
            self.api_host, self.dataset_name
        )
    }
}

impl std::error::Error for InvalidEndpointConfig {}

impl Builder {
    /// Set the logical name of the service, using the `service.name` field as defined
    /// by OpenTelemetry. Used for distributed tracing.
    pub fn service_name(mut self, service_name: Cow<'static, str>) -> Self {
        self.service_name = Some(service_name);
        self
    }

    /// Insert an extra field that is sent with every event.
    pub fn extra_field(
        mut self,
        field_name: Cow<'static, str>,
        // take Value rather than impl Into<Value>, because since we can't specialize
        //  the From implementations for specific lifetimes like 'static, they allocate
        //  even when unnecessary. Specific type hopefully cues caller to create e.g. a
        //  Cow borrowed from an &'static str, which saves clones over the entire
        //  lifetime of the program
        field_val: Value,
    ) -> Self {
        self.extra_fields.push((field_name, field_val));
        self
    }

    /// Set an extra HTTP header to be sent with all requests sent to Axiom.
    pub fn http_header<S: AsRef<str>, T: AsRef<str>>(
        mut self,
        name: S,
        val: T,
    ) -> Result<Self, AddHeaderError> {
        self.http_headers.insert(
            HeaderName::from_bytes(name.as_ref().as_bytes())
                .map_err(|_| AddHeaderError::InvalidHttpHeaderName(name.as_ref().to_owned()))?,
            val.as_ref()
                .try_into()
                .map_err(|_| AddHeaderError::InvalidHttpHeaderValue(val.as_ref().to_owned()))?,
        );
        Ok(self)
    }

    /// Size of the [`std::sync::mpsc`] channel used to send events from the layer to
    /// the background task. Events are silently dropped if this limit is reached, so
    /// the default is large such that it will only be reached by a buggy program.
    pub fn event_channel_size(mut self, size: usize) -> Self {
        self.event_channel_size = size;
        self
    }

    /// Build using the US instance.
    pub fn build(
        self,
        api_host: &str,
        dataset_name: &str,
    ) -> Result<
        (
            crate::layer::Layer,
            BackgroundTask,
            BackgroundTaskController,
        ),
        InvalidEndpointConfig,
    > {
        // endpoint is {api_host}/v1/datasets/{dataset_name}/ingest
        // ref: https://axiom.co/docs/restapi/ingest
        let endpoint = Url::parse(api_host)
            .and_then(|host| host.join("v1/datasets/"))
            .and_then(|endpoint| endpoint.join(&format!("{}/ingest", dataset_name)))
            .map_err(|_| InvalidEndpointConfig {
                api_host: api_host.to_string(),
                dataset_name: dataset_name.to_string(),
            })?;
        Ok(self.build_with_custom_endpoint(endpoint))
    }

    /// Build using a custom "Create Events" endpoint [`Url`].
    pub fn build_with_custom_endpoint(
        self,
        axiom_endpoint_url: Url,
    ) -> (
        crate::layer::Layer,
        BackgroundTask,
        BackgroundTaskController,
    ) {
        let (sender, receiver) = mpsc::channel(self.event_channel_size);
        let layer = crate::layer::Layer::new(self.service_name, sender.clone());
        let background_task = BackgroundTask::new(
            axiom_endpoint_url,
            self.http_headers,
            self.extra_fields,
            receiver,
        );
        let background_controller = BackgroundTaskController::new(sender);
        (layer, background_task, background_controller)
    }
}

/// Create a [`Builder`] with the given `api_key`.
///
/// It is recommended that an Ingest API key is used for sending events. It is also what is used here.
/// A Configuration API key will work, and must have the Send Events permission.
/// Learn more about API keys:
/// https://axiom.co/docs/restapi/ingest
///
/// Panics if `api_key` is not a valid HTTP header value.
///
pub fn builder(api_key: &str) -> Builder {
    let mut builder = Builder {
        service_name: None,
        extra_fields: ExtraFields::new(),
        http_headers: HeaderMap::new(),
        event_channel_size: DEFAULT_CHANNEL_SIZE,
    };
    let mut auth_value = header::HeaderValue::from_str(format!("Bearer {}", api_key).as_str())
        .expect("api_key to be a valid HTTP header value");
    auth_value.set_sensitive(true);
    builder
        .http_headers
        .insert(header::AUTHORIZATION, auth_value);
    builder
}
