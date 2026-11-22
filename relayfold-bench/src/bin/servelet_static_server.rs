//! A servelet server answering one static route, as a benchmark subject: `servelet-static-server
//! [port]` (default 18081). Compare it against another server serving the same body.

use servelet_core::{Request, Response};
use servelet_router::Router;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(18081);
    let router = Router::new().get("/", |_req: Request| async { Response::ok("ok") });
    println!("LISTENING {port}");
    servelet_h1::serve(("127.0.0.1", port), router)
        .await
        .expect("serve failed");
}
