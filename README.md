# signal-gateway

An extensible monitoring tool, sending alerts via Signal messenger, and responding to requests for information
(status, metrics values, plots) or other commands from administrators.

[![License](https://img.shields.io/badge/license-Apache%202.0-blue?style=flat-square)](LICENSE-APACHE)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE-MIT)

## Overview

`signal-gateway` receives alerts and log messages from various sources and forwards them to [Signal
messenger](https://signal.org/) via [signal-cli](https://github.com/AsamK/signal-cli). It supports:

* **Alertmanager webhooks** - Receive Prometheus alerts and forward them to Signal
* **JSON log streams** - Accept JSON logs over TCP/UDP
* **Syslog (RFC 5424)** - Accept syslog messages over TCP/UDP

`signal-gateway` also allows you to define filtering and rate limiting schemes to decide if and when an
error log should be escalated to an alert and forwarded, while avoiding alert fatigue.
It also retains a buffer of recent logs to send as context.

Beyond simple forwarding, it enables admins to query the system interactively.

* **Prometheus querying** - With access to the prometheus query API, you can query metrics and generate plots directly from signal.
* **AI integration** - Can't remember PromQL syntax or the names of your metrics? Connect it to claude, and ask claude to generate plots for you. Claude also sees the log messages, retains context on the system, and can help you troubleshoot.

Additionally, admins can send "commands" with semantics interpreted by your services, if support is configured.

* **Secured by Signal** - Signal messages are a form of authenticated encryption, tied to your device. You can take the safety numbers
from the app and put them in the `signal-gateway` config. Then, even if your phone number is simjacked, and the attacker bypasses registration lock
somehow, they won't be able to send messages that are accepted by the `signal-gateway`, without physical access to your device.

* **Extensible**

The project is designed as both a library and a binary. You can either use the configurable binary (`signal-gateway-bin`) that is offered as a default, or use the library `signal-gateway`
and customize it for your needs. This allows you to add custom handling for admin commands, expose additional tools to the AI integration, and so on.

It's actually a workspace with multiple libraries, so that you can mix and match what features you want without pulling in unnecessary stuff, or easily swap
in alternative implementations of different parts.

* **Free**

Created to simplify devops for projects on a shoestring budget. This project will remain free and open-source.

## Requirements

* [signal-cli](https://github.com/AsamK/signal-cli) running in JSON-RPC daemon mode
  * You can configure it to listen on TCP, or on a unix domain socket
* A registered Signal account.
  * It's best to use a new number that you aren't already using with signal, such as a google voice number.
  * For security, *you should enable registration lock* on this number.

## Quickstart (signal-gateway-bin)

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

3. Configure Alertmanager to send webhooks to `http://signal-gateway:8000/alert`

4. (Optional) Configure `syslog` or `json` listener, and configure your app(s) to send logs over UDP (or TCP) to `signal-gateway`.

   *Note*: `signal-gateway` only buffers your logs temporarily in memory, it doesn't provide long term storage.

5. (Optional) Configure `signal-gateway` to have access to prometheus query API, e.g. `http://prometheus:9090`

   This allows `/plot` command and friends to work in the signal chat.

6. (Optional) Configure `signal-gateway` to use conversational AI (add a `[claude]` section to `config.toml`).

   This allows you to ask for new plots in plain language, ask for help in triaging alerts, making sense of logs, etc.

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
# Find the safety numbers in the Signal app, in your conversation with `signal_account`.
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

NOTE: This example is incomplete, you should refer to `signal-gateway-bin/src/main.rs` for the `Config` object
for exhaustive documentation of the options.

For rust projects, I had success using [`tracing-rfc-5424`](https://docs.rs/tracing-rfc-5424/latest/tracing_rfc_5424/)
to send logs in syslog format over UDP to `signal-gateway-bin`. It worked pretty much out of the box even if the
log messages contain `\n`, because it expects 1 log message per UDP packet.

### Log handler

The log handler controls (not exhaustive):

* How many log messages are cached from each source (`log_handler.log_buffer_size`)
* How we format log messages to be sent in signal (`log_handler.log_format`)
* When a log message can lead to an alert (by configuring one or more "routes")
* Overall limits on alerting (applies to all routes)

See docs for `LogHandlerConfig` for more specifics.

High level:

* A `Route` consists of an `alert_level`, a `LogFilter`, and a series of `Limit`'s.
  If a message is at the alert level or higher, and it passes the filter, then we test
  each `Limit` in the route.
  * A `LogFilter` is a test against the fields of the log message. It is stateless.
  * A `Limit` contains its own `LogFilter`, and a rate threshold.
    * A threshold of the form `> n / time` performs "burst detection", and is useful
      for suppressing transient errors, so that they only lead to alerts if they happen
      in rapid succession.
    * A threshold of the form `< n / time` performs "rate limiting", and is useful for
      suppressing spam. Only the first few events will pass the limit, and anything beyond
      that is suppressed.
    * A `Limit` may also apply in a `by_source_location` fashion. This means that
      there is a separate rate limit counter for each `file:lineno` pair.
      This allows you to be more surgical in what you choose to suppress.
  * `route.limits` contains limits that apply per source (app + hostname pair).
    `route.global_limits` contains limits that apply to all sources.
  * A message passes a route if it passed the filter, and each limit and global limit.
* In order to trigger an alert, a log message must pass at least one route, and then pass the `overall_limits`,
  if any are configured. This is an additional sequence of `Limit`'s.
  * If it passes these then it leads to an alert -- a signal message being sent to admins or to the group, containing this
    log message and then all recent log messages in the log buffer from this source.

Example TOML section:

```toml
# Log handler configuration
[log_handler]
# Overall rate limit: max 1 alert per 10m from same source location
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

For a complete list of `LogFilter` keys, see docs for `struct LogFilter`.

### Claude

`signal-gateway-bin` has a claude integration.

* Set a path to an anthropic api key.
* Configure one or more system prompts (1) explain its role (2) summarize what metrics are available
* Choose what model to use in conversation
* Configure how compaction works
  * What prompt to use when compacting, how large of a response to allow
  * What model to use for compacting
  * How many characters in the conversation should trigger compaction.

```toml
[claude]
api_key_file = "creds/anthropic_api_key"
system_prompt_files = ["system_prompt.md", "metrics_prompt.md"]
claude_model = "claude-sonnet-4-5-20250929"

[claude.compaction]
prompt_file = "compaction_prompt.md"
model = "claude-sonnet-4-5-20250929"
max_tokens = 2048
trigger_chars = 10000
```

This integration is still a work in progress -- it's useful as it is and can generate
complicated plots on demand and help figure out what might be wrong in the system.

But:

* It's not as sophisticated as some agent frameworks like [`rig`](https://docs.rs/rig-core/latest/rig/).
  It's possible that we'll switch to something like that if `rig` becomes more mature.
  For the moment I decided to just make the simplest thing that would meet my
  immediate needs.
* It isn't using very sophisticated compression techniques. Compressing information before
  generating a prompt can result in less tokens for a similar result. This would make it cost less
  to use it for a similar amount of log data. For systems that aren't very chatty it's
  pretty cost effective as is.

YMMV, contributions are welcome!

## License

MIT or Apache 2 at your option.
