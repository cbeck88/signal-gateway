# signal-gateway

The `signal-gateway` library provides the `Gateway` struct which provides the core
of the gateway functionality. (See `https://github.com/cbeck88/signal-gateway` main README)

* Sends messages to `signal-cli` in response to logs or prometheus alerts which are
  passed to it
* Receives messages from `signal-cli`, authenticates them as coming from admins,
  and handles the messages appropriately
  * Spawns a tokio task to drive these loops
* Manages buffering of logs, and filtering rules to decide when a log should trigger an alert.
* Optionally, can forward requests to prometheus query API
* Optionally, can include a conversational AI integration (see `signal-gateway-assistant`)

The `GatewayConfig` struct and `GatewayBuilder` are the main means of configuring it.

Note that `Gateway` doesn't listen on any ports. Any endpoints
for log ingest etc. need to be configured outside the gateway. See `signal-gateway-bin` for an example.
