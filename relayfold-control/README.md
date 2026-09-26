# relayfold-control

A real fleet-management control plane for relayfold-based proxy instances — the Rust equivalent
of zentinel's `zentinel-control-plane` (Elixir/Phoenix, self-declared Beta). No Elixir/Phoenix
here: built on `biota-http-server`'s own hand-rolled `Router`/`Client` and `sangdb` for
disk-persisted registry state.

## What it does (real, tested end-to-end)

- **Register**: `POST /register {id, addr, version}` — a proxy instance announces itself.
- **Heartbeat**: `POST /heartbeat {id, healthy, config_version}` — updates health + config
  version; 404 for an unregistered id (never silently creates one).
- **List**: `GET /instances` — every registered instance and its current health snapshot.
- **Config push**: `POST /config/push {target, config}` — `target` is one instance's id or `"*"`
  for the whole fleet. Delivers a real HTTP POST to each matching instance's
  `/relayfold-control/config` endpoint and reports per-instance success/failure.
- **Durable**: registry state is `sangdb`-backed and survives a process restart.

## What's out of scope (disclosed, not hidden)

Multi-tenant dashboards, canary/staged rollout, an authenticated/RBAC'd admin UI, mTLS or even a
shared bearer token between control plane and instances (plain HTTP), and instance
auto-discovery (an instance must call `/register` itself — no scanning/service-discovery).

## Run it

```
cargo run --bin relayfold-control -- 0.0.0.0:8088 ./data
```
