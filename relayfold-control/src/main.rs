//! Real binary entry point: `relayfold-control <bind-addr> <data-dir>`.

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "0.0.0.0:8088".to_string());
    let data_dir = args
        .next()
        .unwrap_or_else(|| "./relayfold-control-data".to_string());

    let state = relayfold_control::AppState::new(&data_dir).await?;
    let router = relayfold_control::router(state);
    println!("relayfold-control listening on {addr}, data dir {data_dir}");
    servelet_h1::serve(&addr, router)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
}
