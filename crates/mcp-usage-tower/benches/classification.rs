use std::hint::black_box;
use std::time::Instant;

use mcp_usage_tower::classify_protocol_headers;

fn main() {
    const ITERATIONS: u32 = 5_000_000;
    let started = Instant::now();
    for _ in 0..ITERATIONS {
        let classified = classify_protocol_headers(
            black_box(Some("2026-07-28")),
            black_box(Some("tools/call")),
            black_box(Some("get_weather")),
        )
        .expect("fixed headers classify");
        black_box(classified);
    }
    let elapsed = started.elapsed();
    let nanos_per_request = elapsed.as_nanos() as f64 / f64::from(ITERATIONS);
    println!(
        "mcp-usage-tower header classification: {nanos_per_request:.2} ns/request ({ITERATIONS} iterations)"
    );
}
