# Multi-stage build for relayfold-proxy — matches values.yaml's image.repository/tag once built
# and pushed (not done in this environment: no registry credentials here — see README).
#
# BUILD CONTEXT MUST BE THE REPOSITORY ROOT:
#     docker build -f relayfold-helm/proxy-runtime/Dockerfile .
# The crate path-depends on ../../relayfold (the Registry the routes live in) and
# ../../relayfold-agents, so the workspace has to be copied whole. Building with this
# directory as the context fails to resolve those path deps.
FROM rust:1.82-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p relayfold-proxy-runtime

FROM debian:bookworm-slim
RUN apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/relayfold-proxy /usr/local/bin/relayfold-proxy
# A working default config so the container starts. Mount over this path (a ConfigMap volume,
# or a bind mount) to configure it; the runtime reloads that file in place, so replacing it
# moves the running proxy's routes with no restart.
COPY relayfold-helm/proxy-runtime/proxy-runtime.example.yaml /etc/relayfold/proxy-runtime.yaml
ENV PROXY_CONFIG=/etc/relayfold/proxy-runtime.yaml
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/relayfold-proxy"]
