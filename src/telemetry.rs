//! Constructs the two *optional* tracing_subscriber layers this project
//! adds on top of its always-on stdout JSON layer: OTLP trace export and a
//! Loki log push. Both are wired up in `main.rs` only when their
//! corresponding `Config` field is `Some` — see
//! `docs/superpowers/specs/2026-08-14-otel-tracing-and-logs-design.md`.

use anyhow::Context;
use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::WithExportConfig;

/// Builds an OTLP/gRPC (tonic) span exporter pointed at `otlp_endpoint`,
/// wraps it in a batching `SdkTracerProvider` tagged with `service.name =
/// wardn`, registers that provider as the process-global OTel tracer
/// provider, and returns a `Tracer` from it — ready to hand to
/// `tracing_opentelemetry::layer().with_tracer(...)` in `main.rs`.
pub fn init_tracer(otlp_endpoint: &str) -> anyhow::Result<opentelemetry_sdk::trace::Tracer> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(otlp_endpoint)
        .build()
        .context("failed to build OTLP span exporter")?;
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name("wardn")
        .build();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());
    Ok(provider.tracer("wardn"))
}

/// Builds a `tracing-loki` layer pushing to `loki_url`, labeled
/// `service_name=wardn` — matching the `service_name` label the Tempo
/// datasource's `tracesToLogsV2` query filters on
/// (`grafana/provisioning/datasources/tempo.yml`), so both point at the
/// same log stream. Returns the layer plus its background delivery task,
/// which the caller (`main.rs`) must `tokio::spawn` — the layer only
/// buffers lines, it does not send them itself.
pub fn init_loki_layer(
    loki_url: &str,
) -> anyhow::Result<(tracing_loki::Layer, tracing_loki::BackgroundTask)> {
    let url = url::Url::parse(loki_url).context("invalid LOKI_URL")?;
    let (layer, task) = tracing_loki::builder()
        .label("service_name", "wardn")
        .context("invalid tracing-loki label")?
        .build_url(url)
        .context("failed to build tracing-loki layer")?;
    Ok((layer, task))
}
