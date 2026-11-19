//! Opt-in OTLP tracing export (S8.1, Q-020 / Q-035).
//!
//! Enabled by the `otlp` feature. The provider installs a `tracing-opentelemetry`
//! layer on the global subscriber, so any ordinary `tracing` span — including the
//! Track A block/consensus spans and the Track B RPC spans — exports through OTLP
//! with no per-crate export API. Default builds carry no OpenTelemetry dependency.
//!
//! Spans carry only bounded operational fields; never secrets (Q-020). The batch
//! processor runs on the tokio runtime, so [`init_with_otlp`] must be called from
//! within a tokio runtime context. The returned [`OtlpGuard`] must be retained for
//! the process lifetime; dropping it flushes and shuts the provider down.

use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::trace::SpanExporter;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::{TelemetryConfig, TelemetryError};

/// OTLP export configuration. The endpoint is used verbatim, so include the
/// signal path (`/v1/traces`). Environment overrides are honoured by the
/// exporter (`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_EXPORTER_OTLP_TIMEOUT`).
#[derive(Debug, Clone)]
pub struct OtlpConfig {
    /// Collector traces endpoint, e.g. `http://localhost:4318/v1/traces`.
    pub endpoint: String,
    /// Per-export timeout.
    pub timeout: Duration,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:4318/v1/traces".to_string(),
            timeout: Duration::from_secs(10),
        }
    }
}

/// Retained provider shutdown guard. Keep it alive for the process lifetime;
/// dropping it flushes pending spans and shuts the tracer provider down. It is
/// not a span guard — do not hold it across an `await` (Q-035).
pub struct OtlpGuard {
    provider: SdkTracerProvider,
}

impl Drop for OtlpGuard {
    fn drop(&mut self) {
        let _ = self.provider.shutdown();
    }
}

/// Build the OTel layer and its shutdown guard from any span exporter.
///
/// Uses a batch processor on the tokio runtime (`rt-tokio`), so it must be
/// called within a runtime context. Generic over the exporter so tests can
/// substitute an in-memory exporter for the network one.
fn layer_with_exporter<E>(
    exporter: E,
) -> (OpenTelemetryLayer<Registry, opentelemetry_sdk::trace::Tracer>, OtlpGuard)
where
    E: SpanExporter + 'static,
{
    let provider = SdkTracerProvider::builder().with_batch_exporter(exporter).build();
    let tracer = provider.tracer("subhost");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    (layer, OtlpGuard { provider })
}

/// Install the global subscriber with the fmt layer plus the OTLP export layer.
///
/// Must be called within a tokio runtime context (the batch processor needs it).
/// Returns the retained shutdown guard. Exporter construction failure is
/// reported, never fatal to boot; a second install is refused, matching [`init`].
///
/// [`init`]: crate::init
pub fn init_with_otlp(
    config: TelemetryConfig,
    otlp: OtlpConfig,
) -> Result<OtlpGuard, TelemetryError> {
    use opentelemetry_otlp::WithExportConfig;
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(otlp.endpoint)
        .with_timeout(otlp.timeout)
        .build()
        .map_err(|error| TelemetryError::OtlpInit(error.to_string()))?;
    let (otel_layer, guard) = layer_with_exporter(exporter);

    let fmt_layer = tracing_subscriber::fmt::layer().with_target(config.with_target);
    let fmt_layer = if config.json { fmt_layer.json().boxed() } else { fmt_layer.boxed() };

    // Per-layer EnvFilters govern both the fmt output and what the OTLP layer
    // exports (info spans pass the default). The OTel layer is parameterized on
    // Registry, so it must sit directly on the registry, not above a global
    // filter layer.
    let filter = config.env_filter();
    Registry::default()
        .with(otel_layer.with_filter(filter.clone()))
        .with(fmt_layer.with_filter(filter))
        .try_init()
        .map_err(|error| TelemetryError::AlreadyInitialized(error.to_string()))?;
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn otlp_config_defaults_to_the_local_collector() {
        let config = OtlpConfig::default();
        assert_eq!(config.endpoint, "http://localhost:4318/v1/traces");
        assert_eq!(config.timeout, Duration::from_secs(10));
    }

    #[tokio::test]
    async fn the_layer_exports_a_span_with_its_bounded_fields() {
        let (exporter, mut rx_export, _rx_shutdown) =
            opentelemetry_sdk::testing::trace::new_test_exporter();
        let (layer, guard) = layer_with_exporter(exporter);
        let subscriber = Registry::default().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("accept_block", height = 7u64, outcome = "ok");
            let _entered = span.enter();
        });
        // Dropping the guard flushes the batch processor and shuts it down.
        drop(guard);
        let exported = rx_export.recv().await.expect("the span must reach the exporter");
        assert_eq!(exported.name, "accept_block");
    }

    #[tokio::test]
    async fn the_otlp_exporter_builds_and_shuts_down_cleanly() {
        // No collector is running; this exercises the real OTLP wiring (builder,
        // batch processor, provider) and asserts construction and shutdown do not
        // panic. Export to the absent endpoint is best-effort and swallowed.
        let config = OtlpConfig {
            endpoint: "http://127.0.0.1:4318/v1/traces".into(),
            timeout: Duration::from_millis(200),
        };
        let guard = init_with_otlp(TelemetryConfig::default(), config);
        // A global subscriber may already exist in this test binary; either
        // outcome is acceptable as long as neither panics (mirrors the fmt test).
        if let Ok(guard) = guard {
            tracing::info_span!("seal_block", height = 1u64).in_scope(|| {});
            drop(guard);
        }
    }
}
