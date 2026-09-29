//! The binary's exporter config must render request latency as a
//! Prometheus histogram (buckets), not a summary.
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

#[test]
fn latency_is_exported_as_histogram_buckets() {
    let recorder = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("ferryman_request_duration_seconds".into()),
            ferryman::LATENCY_BUCKETS,
        )
        .unwrap()
        .build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        metrics::histogram!("ferryman_request_duration_seconds", "route" => "/a", "upstream" => "h:1")
            .record(0.003);
    });
    let out = handle.render();
    assert!(out.contains("ferryman_request_duration_seconds_bucket{route=\"/a\",upstream=\"h:1\",le=\"0.005\"} 1"), "{out}");
}
