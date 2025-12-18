# prometheus-http-client

[![Crates.io](https://img.shields.io/crates/v/prometheus-http-client?style=flat-square)](https://crates.io/crates/prometheus-http-client)
[![Crates.io](https://img.shields.io/crates/d/prometheus-http-client?style=flat-square)](https://crates.io/crates/prometheus-http-client)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue?style=flat-square)](LICENSE-APACHE)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE-MIT)

[API Docs](https://docs.rs/prometheus-http-client/latest/prometheus_http_client/)

Makes requests to the prometheus query API. With `plot` feature, also provides
a way to plot responses from prometheus.

## Why a custom implementation?

There are several prometheus query clients for Rust, but none quite fit the requirements:

### [`prometheus-http-query`](https://docs.rs/prometheus-http-query)

The most complete and actively maintained option. However, it uses a structured
`Selector` builder that doesn't support raw PromQL selector strings. This means
queries like `__name__=~"http_.*"` or complex label matchers must be constructed
programmatically rather than passed as strings. For use cases where selectors
come from configuration files or user input, this is a significant limitation.

### [`prometheus-http-api`](https://docs.rs/prometheus-http-api)

Supports raw selector strings, which is great. However, it only implements
instant and range queries (`/api/v1/query` and `/api/v1/query_range`). It lacks
support for `/api/v1/series`, `/api/v1/labels`, `/api/v1/label/.../values`,
and `/api/v1/alerts` endpoints that this crate uses.

### [`proq`](https://docs.rs/proq) / [`prometheus-query`](https://docs.rs/prometheus-query)

Both are unmaintained (last updates 4+ years ago) and use outdated dependencies
like `tokio 0.1` and `hyper 0.12`.

### This crate

This implementation supports both raw PromQL selector strings and the full set of
API endpoints needed (query, query_range, series, labels, label values, alerts).
The `plot` feature adds time-series visualization using plotters.
