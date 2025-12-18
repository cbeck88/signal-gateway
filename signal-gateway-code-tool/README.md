# signal-gateway-code-tool

[![Crates.io](https://img.shields.io/crates/v/signal-gateway-code-tool?style=flat-square)](https://crates.io/crates/signal-gateway-code-tool)
[![Crates.io](https://img.shields.io/crates/d/signal-gateway-code-tool?style=flat-square)](https://crates.io/crates/signal-gateway-code-tool)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue?style=flat-square)](LICENSE-APACHE)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE-MIT)

[API Docs](https://docs.rs/signal-gateway-code-tool/latest/signal_gateway_code_tool/)

A tool that can be provided to the `signal-gateway-assistant`, which allows it
to fetch and read code without giving it shell access.

If configured with a github read-only access token, and if you give it a route
to query the current git SHA of your deployed binary, it can transparently fetch the code
corresponding to that.
