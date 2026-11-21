# relayfold-agents

A **crash-isolated external agent protocol** and its Rust SDK: run your policy, WAF or
content check in a *separate process*, over a socket, and consult it from the data plane.

## Why this instead of the obvious alternative

The obvious way to extend a proxy is a trait: implement it, compile it into the proxy, done.
That couples the extension's failure to the proxy's. A segmentation fault, an infinite loop, or
a memory leak in a WAF agent takes the data plane down with it, and redeploying the agent means
redeploying the proxy.

Running the extension as a **separate process over a socket** fixes that by construction:

- a crashed agent is restarted, redeployed, or rewritten in another language **without touching
  the proxy binary**;
- the data plane keeps serving; the agent's verdict is simply missing, which is a decision the
  caller makes;
- the proxy's request path never blocks on the agent's memory.

This is the same architectural shape other Rust proxies use for agents. The *pattern* is
borrowed; the implementation here is original — no third-party agent-protocol crate is vendored
or copied.

## What you get

| item | what it is |
|---|---|
| `protocol` | the wire format: the request and response frames, and the framing rules |
| `Agent` | the trait your implementation implements |
| `AgentRunner<A>` | runs your agent, serving the protocol on a socket |
| `AgentClient` | the client side: connects, asks, reads the verdict |

The client is one-shot per call: a fresh connection per question. That is a deliberate scope
limit, not an accident of the implementation.

## Deliberately not here — disclosed, not silently skipped

- **UDS only.** There is no gRPC transport.
- **One connection per call.** No persistent, multiplexed connection.
- **Rust only.** No multi-language SDKs; the wire format is the contract if you want another
  language to speak it.
- **Not wired into a specific proxy's request path.** This crate is the transport and the SDK;
  calling it from a particular proxy's request filter is a separate piece of wiring.

## Build

```sh
cargo build --release
```

## Integration

```toml
[dependencies]
relayfold-agents = { path = "../relayfold-agents" }
```

or from git:

```toml
[dependencies]
relayfold-agents = { git = "https://github.com/webfold-ai/relayfold", rev = "<pin>" }
```

## License

SPDX: `Apache-2.0` (declared in `Cargo.toml`).
