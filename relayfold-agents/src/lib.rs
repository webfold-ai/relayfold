//! `relayfold-agents` — a crash-isolated external agent protocol, and its Rust SDK, for
//! proxies built on `relayfold`'s registry (or on anything else with a request-filter hook).
//!
//! **What this is for.** The obvious way to extend a proxy is a trait: implement it, compile it
//! into the proxy, done. That couples the extension's failure to the proxy's — a segmentation
//! fault, an infinite loop or a memory leak in a WAF agent takes the data plane down with it,
//! and redeploying the agent means redeploying the proxy.
//!
//! Running the extension as a **separate process over a socket** fixes that by construction: a
//! crashed agent is restarted, redeployed, or rewritten in another language without touching
//! the proxy binary; the data plane keeps serving and the agent's verdict is simply missing,
//! which is a decision the caller makes; and the proxy's request path never blocks on the
//! agent's memory.
//!
//! The architectural pattern is borrowed; the implementation here is original — no third-party
//! agent-protocol crate is vendored or copied.
//!
//! **Scope, stated plainly.** A UDS transport only (no gRPC), one connection per call (no
//! persistent or multiplexed connection), and Rust only (the wire format is the contract if
//! another language wants to speak it). Wiring an agent into a particular proxy's request
//! filter is a separate piece of work; this crate is the transport and the SDK.

pub mod protocol;

pub use protocol::{AgentRequest, Decision, DecisionBuilder, Phase};

use std::future::Future;
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

/// What an agent author implements. One method: given a phase's worth of the request/response,
/// decide. Native `async fn`-in-trait (stable, no `async-trait` dependency needed) — this trait
/// is used generically (`AgentRunner<A>`), never as `dyn Agent`, so RPITIT's lack of object
/// safety is not a real constraint here.
pub trait Agent: Send + Sync + 'static {
    fn on_call(&self, request: &AgentRequest) -> impl Future<Output = Decision> + Send;
}

/// Binds a Unix socket and serves an [`Agent`] — accepts connections, decodes one
/// [`AgentRequest`] per connection, calls the agent, writes back one [`Decision`], closes.
///
/// A decode failure or an `on_call` panic on one connection is caught and logged to the socket's
/// peer as nothing (the connection just closes) rather than taking down `serve`'s accept loop —
/// this is the crash-isolation property this whole crate exists for: one bad request must not
/// kill the agent process's ability to serve the next one.
pub struct AgentRunner<A: Agent> {
    agent: A,
    socket_path: PathBuf,
}

impl<A: Agent> AgentRunner<A> {
    pub fn new(agent: A) -> Self {
        AgentRunner {
            agent,
            socket_path: PathBuf::from("/tmp/relayfold-agent.sock"),
        }
    }

    #[must_use]
    pub fn with_socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket_path = path.into();
        self
    }

    /// Bind and serve forever. Removes a stale socket file at `socket_path` first (the standard
    /// UDS restart problem — `bind` fails with `AddrInUse` on a leftover file from a prior,
    /// uncleanly-stopped run even though nothing is listening on it anymore).
    pub async fn serve(self) -> std::io::Result<()>
    where
        A: Clone,
    {
        let _ = std::fs::remove_file(&self.socket_path);
        let listener = UnixListener::bind(&self.socket_path)?;
        loop {
            let (stream, _addr) = listener.accept().await?;
            let agent = self.agent.clone();
            tokio::spawn(async move {
                let _ = handle_one_connection(stream, agent).await;
            });
        }
    }
}

async fn handle_one_connection<A: Agent>(mut stream: UnixStream, agent: A) -> std::io::Result<()> {
    let request: AgentRequest = protocol::read_frame(&mut stream).await?;
    let decision = agent.on_call(&request).await;
    protocol::write_frame(&mut stream, &decision).await
}

/// What the PROXY side holds — one per configured agent route. `evaluate` opens a fresh
/// connection, sends `request`, and returns the agent's `Decision`.
///
/// Fail-open vs fail-closed when the agent process is down or unresponsive is deliberately left
/// to the caller (see `evaluate`'s own doc) rather than a config knob on this type — the real
/// operational question every one of zentinel's agents answers via a `failure-mode` setting
/// (`"open"`/`"closed"`): a WAF agent should usually fail closed (block on doubt), a non-critical
/// enrichment agent should usually fail open (never block traffic over its own outage). A future
/// `ProxyHttp` integration is the natural place to add that policy, once one exists.
#[derive(Clone)]
pub struct AgentClient {
    socket_path: PathBuf,
}

impl AgentClient {
    #[must_use]
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        AgentClient {
            socket_path: socket_path.into(),
        }
    }

    /// Connect, send `request`, read the `Decision`. Every I/O failure (connect refused, agent
    /// crashed mid-response, malformed frame) surfaces as `Err` — the caller decides fail-open or
    /// fail-closed; this method never guesses on the caller's behalf.
    pub async fn evaluate(&self, request: &AgentRequest) -> std::io::Result<Decision> {
        let mut stream = UnixStream::connect(&self.socket_path).await?;
        protocol::write_frame(&mut stream, request).await?;
        protocol::read_frame(&mut stream).await
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

/// Convenience for `AgentRequest`'s `to_term`/`from_term` re-export path — lets a consumer write
/// `relayfold_agents::AsTerm` without also depending on `wirefold` directly just to name the
/// trait bound on a generic function.
pub use wirefold::codec::syntax::AsTerm as WireTerm;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct DenyAdminPaths;

    impl Agent for DenyAdminPaths {
        async fn on_call(&self, request: &AgentRequest) -> Decision {
            if request.path.starts_with("/admin") {
                Decision::deny()
                    .with_status(403)
                    .with_body("access denied")
                    .build()
            } else {
                Decision::Allow
            }
        }
    }

    fn temp_socket_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "relayfold-agents-test-{name}-{}.sock",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn a_real_uds_round_trip_denies_an_admin_path_and_allows_everything_else() {
        let socket = temp_socket_path("deny-admin");
        let runner = AgentRunner::new(DenyAdminPaths).with_socket(&socket);
        let server = tokio::spawn(runner.serve());

        // Give the listener a moment to actually bind before the client connects — a real,
        // observable startup race a production caller would also have to handle (e.g. by
        // retrying the connect), not something to paper over silently here.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = AgentClient::new(&socket);

        let admin_req = AgentRequest {
            phase: Phase::RequestHeaders,
            method: "POST".into(),
            path: "/admin/delete-everything".into(),
            headers: vec![],
            body: vec![],
        };
        let decision = client
            .evaluate(&admin_req)
            .await
            .expect("a live agent must answer");
        assert_eq!(
            decision,
            Decision::Deny {
                status: 403,
                body: "access denied".into()
            }
        );

        let normal_req = AgentRequest {
            phase: Phase::RequestHeaders,
            method: "GET".into(),
            path: "/health".into(),
            headers: vec![],
            body: vec![],
        };
        let decision = client
            .evaluate(&normal_req)
            .await
            .expect("a live agent must answer");
        assert_eq!(decision, Decision::Allow);

        server.abort();
        let _ = std::fs::remove_file(&socket);
    }

    #[tokio::test]
    async fn evaluate_against_a_dead_socket_is_a_real_io_error_not_a_silent_allow() {
        let socket = temp_socket_path("never-started");
        let client = AgentClient::new(&socket);
        let req = AgentRequest {
            phase: Phase::RequestHeaders,
            method: "GET".into(),
            path: "/".into(),
            headers: vec![],
            body: vec![],
        };
        let result = client.evaluate(&req).await;
        assert!(
            result.is_err(),
            "connecting to a socket nothing is listening on must error, not silently allow"
        );
    }

    #[tokio::test]
    async fn restarting_the_runner_on_the_same_socket_path_does_not_fail_to_bind() {
        let socket = temp_socket_path("restart");
        let runner1 = AgentRunner::new(DenyAdminPaths).with_socket(&socket);
        let handle1 = tokio::spawn(runner1.serve());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle1.abort();
        // The socket file is now stale (listener aborted, file left behind) — a fresh
        // AgentRunner on the same path must still bind, not hit AddrInUse.
        let runner2 = AgentRunner::new(DenyAdminPaths).with_socket(&socket);
        let handle2 = tokio::spawn(runner2.serve());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = AgentClient::new(&socket);
        let req = AgentRequest {
            phase: Phase::RequestHeaders,
            method: "GET".into(),
            path: "/ok".into(),
            headers: vec![],
            body: vec![],
        };
        let decision = client
            .evaluate(&req)
            .await
            .expect("the restarted runner must be reachable");
        assert_eq!(decision, Decision::Allow);

        handle2.abort();
        let _ = std::fs::remove_file(&socket);
    }
}
