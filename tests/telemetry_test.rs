#![cfg(feature = "observability")]

use wardn::telemetry;

#[test]
fn init_loki_layer_rejects_an_invalid_url() {
    // `(Layer, BackgroundTask)` isn't `Debug`, so `unwrap_err()` (which
    // formats the Ok value on a mismatch) doesn't work here — match by hand.
    match telemetry::init_loki_layer("not a url") {
        Ok(_) => panic!("expected an error for an invalid URL"),
        Err(e) => assert!(e.to_string().contains("invalid LOKI_URL")),
    }
}

#[test]
fn init_loki_layer_builds_from_a_valid_url() {
    // Building the layer doesn't connect anywhere — it just configures
    // where the background task (never spawned here) would push to.
    telemetry::init_loki_layer("http://127.0.0.1:1/loki/api/v1/push").unwrap();
}

#[tokio::test]
async fn init_tracer_builds_from_a_valid_endpoint() {
    // Tonic's gRPC channel connects lazily (and needs a Tokio runtime
    // context to set that up), so this succeeds without any real OTLP
    // collector listening at the endpoint.
    telemetry::init_tracer("http://127.0.0.1:1").unwrap();
}
