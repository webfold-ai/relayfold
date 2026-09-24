use biota_http_server::{serve, Proxy, Router, Upstream};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind = std::env::var("BENCH_BIND").unwrap_or_else(|_| "127.0.0.1:18080".into());
    let upstream_host = std::env::var("BENCH_UPSTREAM_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let upstream_port: u16 = std::env::var("BENCH_UPSTREAM_PORT")
        .unwrap_or_else(|_| "18081".into())
        .parse()?;
    let proxy = Proxy::new().upstream("*", Upstream::http(upstream_host, upstream_port));
    let router = Router::new().fallback(move |request| {
        let proxy = proxy.clone();
        async move { proxy.handle(request).await }
    });
    serve(bind, router).await?;
    Ok(())
}
