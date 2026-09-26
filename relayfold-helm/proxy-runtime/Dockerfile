# Multi-stage build for relayfold-proxy — matches values.yaml's image.repository/tag once built
# and pushed (not done in this environment: no registry credentials here — see README).
FROM rust:1.82-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p relayfold-proxy-runtime

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/relayfold-proxy /usr/local/bin/relayfold-proxy
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/relayfold-proxy"]
