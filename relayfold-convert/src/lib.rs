//! Convert a real, bounded subset of nginx and HAProxy configs into `relayfold`'s own declarative
//! YAML route format (see [`config`] for the format itself and why it's plain YAML, not a new
//! DSL). Two real, tested parsers — not six thin ones. See [`nginx`] and [`haproxy`] for exactly
//! what each one covers and what it deliberately refuses to guess at.
//!
//! Deliberately NOT here: Apache, Traefik, Caddy, and Envoy converters. Those are real gaps
//! against zentinel-convert's six-format claim, left for whoever needs one next rather than
//! stubbed out as fake coverage.
//!
//! Also NOT here: a loader wiring this crate's `ProxyConfig` back into a live
//! `relayfold::Registry`. This crate depends on nothing from `relayfold` on purpose (its job is
//! producing a correct, standalone config document); that loader is a small, separate function
//! for whichever consumer (bedrock, biota) actually builds a `Registry` from one.

pub mod config;
pub mod haproxy;
pub mod nginx;

pub use config::{AllowRule, ProxyConfig, Route, UpstreamCfg};
