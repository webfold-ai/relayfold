//! Native async upstream fixture for gateway benchmarks.
//! The performance path must not include Python scheduling or HTTP server overhead.

use biota_http_server::{serve, Method, Response, Router, StatusCode};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind = std::env::var("BENCH_UPSTREAM_BIND").unwrap_or_else(|_| "127.0.0.1:18081".into());
    let router = Router::new().route_sync(Method::GET, "/1k", |_request| {
        Response::new(StatusCode::OK, vec![b'x'; 1024])
    });
    serve(bind, router).await?;
    Ok(())
}
