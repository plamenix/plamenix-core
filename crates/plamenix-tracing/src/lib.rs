//! Tracing-subscriber bootstrap for Plamenix Rust binaries.
//!
//! Every Plamenix binary (desktop Tauri shell, web NAPI module) calls
//! [`init`] exactly once on startup. The subscriber always attaches a
//! `fmt` layer driven by [`tracing_subscriber::EnvFilter`] so spans and
//! events land in stderr in development. When the `otlp` cargo feature
//! is enabled AND the `OTEL_EXPORTER_OTLP_ENDPOINT` environment variable
//! is set, the subscriber additionally exports spans to an OTLP
//! collector over gRPC (`tonic`).
//!
//! The returned [`TracingGuard`] owns the OpenTelemetry tracer provider
//! (when the OTLP exporter is active). Dropping the guard shuts the
//! provider down, which flushes any pending span batches. Binaries
//! should hold the guard for the entire process lifetime — losing it
//! mid-run will silently stop exports.
//!
//! ## Environment variables
//!
//! | Variable | Default | Effect |
//! |----------|---------|--------|
//! | `RUST_LOG` | `info` | `tracing_subscriber::EnvFilter` directives. |
//! | `OTEL_SERVICE_NAME` | `plamenix` | Service name attribute on exported spans. |
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | _(unset)_ | When set, enables the OTLP exporter. Typical value: `http://localhost:4317`. |

use thiserror::Error;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Initialisation outcome reported back to the caller.
#[derive(Debug)]
#[non_exhaustive]
pub enum InitOutcome {
    /// Only the `fmt` layer is active.
    FmtOnly,
    /// The OTLP exporter is active and shipping spans to `endpoint`.
    OtlpEnabled {
        /// The endpoint URL the exporter is targeting.
        endpoint: String,
        /// The service name attached to exported spans.
        service_name: String,
    },
}

/// Errors raised by [`init`].
#[derive(Debug, Error)]
pub enum InitError {
    /// The global subscriber could not be installed (probably already
    /// initialised by another caller).
    #[error("subscriber install failed: {0}")]
    SubscriberInstall(String),
    /// The OTLP exporter pipeline failed to start.
    #[cfg(feature = "otlp")]
    #[error("otlp pipeline failed: {0}")]
    Otlp(String),
}

/// RAII guard returned by [`init`]. Holding the guard keeps the
/// OpenTelemetry tracer provider alive; dropping it flushes pending
/// spans and shuts the provider down.
#[must_use = "drop the TracingGuard at process exit to flush spans"]
pub struct TracingGuard {
    #[cfg(feature = "otlp")]
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

#[cfg(feature = "otlp")]
impl Drop for TracingGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            // Best-effort flush. We deliberately swallow errors: the
            // process is shutting down, there is no one to surface them
            // to, and logging from inside Drop is risky.
            let _ = provider.shutdown();
        }
    }
}

/// Installs the global tracing subscriber.
///
/// Call once near `main`. Stash the returned guard until process exit.
///
/// # Errors
///
/// Returns [`InitError::SubscriberInstall`] when another subscriber is
/// already installed (typical in tests).
pub fn init() -> Result<(TracingGuard, InitOutcome), InitError> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer();

    #[cfg(feature = "otlp")]
    {
        if let Some(endpoint) = read_otlp_endpoint() {
            let service_name =
                std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "plamenix".to_string());
            let (provider, otel_layer) = build_otlp_layer(&endpoint, &service_name)?;
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .with(otel_layer)
                .try_init()
                .map_err(|err| InitError::SubscriberInstall(err.to_string()))?;
            return Ok((
                TracingGuard {
                    provider: Some(provider),
                },
                InitOutcome::OtlpEnabled {
                    endpoint,
                    service_name,
                },
            ));
        }
    }

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .try_init()
        .map_err(|err| InitError::SubscriberInstall(err.to_string()))?;

    Ok((
        TracingGuard {
            #[cfg(feature = "otlp")]
            provider: None,
        },
        InitOutcome::FmtOnly,
    ))
}

#[cfg(feature = "otlp")]
fn read_otlp_endpoint() -> Option<String> {
    let raw = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(feature = "otlp")]
fn build_otlp_layer<S>(
    endpoint: &str,
    service_name: &str,
) -> Result<
    (
        opentelemetry_sdk::trace::SdkTracerProvider,
        tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>,
    ),
    InitError,
>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    use opentelemetry::KeyValue;
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_otlp::{SpanExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::trace::SdkTracerProvider;

    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|err| InitError::Otlp(err.to_string()))?;

    let resource = Resource::builder()
        .with_attribute(KeyValue::new("service.name", service_name.to_string()))
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("plamenix");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    Ok((provider, layer))
}
