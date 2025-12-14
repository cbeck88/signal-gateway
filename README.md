# signal-gateway

An extensible monitoring tool, bridging alerting and monitoring systems with Signal messenger.

[![License](https://img.shields.io/badge/license-Apache%202.0-blue?style=flat-square)](LICENSE-APACHE)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE-MIT)

## Overview

`signal-gateway` receives alerts and log messages from various sources and forwards them to [Signal
messenger](https://signal.org/) via [signal-cli](https://github.com/AsamK/signal-cli). It supports:

* **Alertmanager webhooks** - Receive Prometheus alerts and forward them to Signal
* **JSON log streams** - Accept JSON logs over TCP/UDP
* **Syslog (RFC 5424)** - Accept syslog messages over TCP/UDP

`signal-gateway` also allows you to define filtering and rate limiting schemes to decide if and when an
error log should be escalated to an alert and forwarded, while avoiding alert fatigure.
It also retains a buffer of recent logs to send as context.

Beyond simple forwarding, it enables admins to query the system interactively.

* **Prometheus querying** - With access to the prometheus query API, you can query metrics and generate plots directly from signal.
* **AI integration** - Can't remember PromQL syntax or the names of your metrics? Connect it to claude, and ask claude to generate plots for you. Claude also sees the log messages, retains context on the system, and can help you troubleshoot.

Additionally, admins can send "commands" with semantics interpreted by your services elsewhere in the cluster, if you configure this.

* **Secured by Signal** - Signal messages are a form of authenticated encryption, tied to your device. You can take the safety numbers
from the app and put them in the `signal-gateway` config. Then, even if your phone number is simjacked, and you don't have registration
lock enabled, an attacker won't be able to send commands that are accepted by the `signal-gateway`.

* **Extensible**

The project is designed as both a library and a binary. You can either use the configurable binary (`signal-gateway-bin`) that is offered as a default, or use the library `signal-gateway`
and customize it for your needs. This allows you to add custom handling for admin commands, expose additional tools to the AI integration, and so on.

## Requirements

* [signal-cli](https://github.com/AsamK/signal-cli) running in JSON-RPC daemon mode
  * You can configure it to listen on TCP, or on a unix domain socket
* A registered Signal account.
  * It's best to use a new number that you aren't already using with signal, such as a google voice number.
  * For security, you should enable registration lock on this number, as well as your personal number.

## Quickstart

1. Start signal-cli in JSON-RPC mode:

   ```sh
   signal-cli -a +15551234567 daemon --tcp 127.0.0.1:7583
   ```

2. Run signal-gateway:

   ```sh
   signal-gateway \
     --signal-cli-tcp-addr 127.0.0.1:7583 \
     --signal-account +15551234567 \
     --signal-admins '["your-uuid-here"]'
   ```

3. Configure Alertmanager to send webhooks to `http://localhost:8000/alert`

## Configuration

`signal-gateway` supports hierarchical config, and can read config values from CLI arguments, environment variables, or a TOML config file,
or combinations thereof. See `--help` for details.
Use `--config-file path/to/config.toml` to load from a file.

Example TOML configuration:

```toml
http_listen_addr = "0.0.0.0:8000"
signal_account = "+15551234567"
signal_cli_tcp_addr = "127.0.0.1:7583"

# Admin UUIDs mapped to their safety numbers (empty list means no verification)
[signal_admins]
"12345678-1234-1234-1234-123456789abc" = []

# Optional: send alerts to a group instead of individual admins
# alert_group_id = "base64-encoded-group-id"

# Optional: Prometheus for /query, /plot, /alerts commands
[prometheus]
prometheus_url = "http://172.31.10.138:9090"

[prometheus.plot]
timezone = "US/Mountain"

# Syslog listener (optional)
[syslog]
listen_addr = "0.0.0.0:1514"

# JSON log listener (optional)
[json]
listen_addr = "0.0.0.0:5000"
```

Run `signal-gateway --help` for all available options.

### Log handler

TODO

```toml
# Log handler configuration
[log_handler]
# Overall rate limit: max 1 alert per 10m from same source location (suppress 2nd+)
overall_limits = [
    { threshold = "< 2 / 10m", by_source_location = true }
]

# Log formatting
[log_handler.log_format]
format_module = true
format_source_location = true

# Single route matching errors, with burst detection for noisy patterns
# (only alert if pattern occurs 2+ times in 10m)
[[log_handler.route]]
alert_level = "error"
limits = [
    { threshold = ">= 2 / 10m", module_equals = "ws", msg_contains = "WebSocket protocol error: Connection reset without closing handshake" },
    { threshold = ">= 2 / 10m", module_equals = "ws", msg_contains = "did not respond to ping, closing stream" },
    { threshold = ">= 2 / 10m", module_equals = "ws", msg_contains = "IO error: peer closed connection without sending TLS close_notify" },
    { threshold = ">= 2 / 10m", module_equals = "main", msg_contains = "error sending request for url" },
]
```

### Claude

TODO

```toml
[claude]
api_key_file = "creds/anthropic_api_key"
system_prompt_files = ["system_prompt.md", "metrics_prompt.md"]
claude_model = "claude-sonnet-4-5-20250929"

[claude.compaction]
prompt_file = "compaction_prompt.md"
model = "claude-sonnet-4-5-20250929"
max_tokens = 2048
trigger_chars = 50000
```

## License

MIT or Apache 2 at your option.
