use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ferryman_core::{Route, RouteTable, Upstream};
use std::time::Duration;

fn build_table() -> RouteTable {
    let mut routes: Vec<Route> = (0..50)
        .map(|i| Route {
            prefix: format!("/svc-{i}"),
            upstream: Upstream::new(
                format!("http://localhost:{}", 9000 + i).parse().unwrap(),
                Duration::from_secs(30),
                3,
            ),
        })
        .collect();
    // A deep, specific prefix nested under one of the shallow ones, so
    // "hit deep" has to win a longest-prefix comparison instead of just
    // being the first (and only) candidate that starts_with-matches.
    routes.push(Route {
        prefix: "/svc-25/api/v1/users".to_string(),
        upstream: Upstream::new(
            "http://localhost:9999".parse().unwrap(),
            Duration::from_secs(30),
            3,
        ),
    });
    RouteTable::new(routes, Duration::from_secs(30))
}

fn bench_lookup(c: &mut Criterion) {
    let table = build_table();

    // Sorted DESC by prefix length, so this is near the front of the scan.
    c.bench_function("lookup_hit_deep", |b| {
        b.iter(|| table.lookup(black_box("/svc-25/api/v1/users/42")))
    });
    // Only matches a short prefix, so the scan runs past every longer
    // prefix (including the deep one above) before matching.
    c.bench_function("lookup_hit_shallow", |b| {
        b.iter(|| table.lookup(black_box("/svc-3/anything")))
    });
    // No match at all: full scan of every route.
    c.bench_function("lookup_miss", |b| {
        b.iter(|| table.lookup(black_box("/does-not-exist")))
    });
}

criterion_group!(benches, bench_lookup);
criterion_main!(benches);
