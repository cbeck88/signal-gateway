//! Signal Gateway binary - receives alerts and logs, forwards to Signal messenger.

#![deny(missing_docs)]

use conf::Conf;
use http::{Method, Request, Response, StatusCode};
use http_body::Body;
use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper_util::{rt::TokioIo, server::conn::auto};
use signal_gateway::{CommandRouter, Gateway, GatewayConfig, Handling, alertmanager::AlertPost};
use signal_gateway_app_code::AppCodeTools;
use signal_gateway_assistant_claude::{ClaudeAssistant, ClaudeConfig};
use std::{
    convert::Infallible, env, fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration,
};
use tokio::net::TcpListener;
use tokio_util::{bytes::Buf, sync::CancellationToken};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

mod admin_http;
use admin_http::AdminHttpConfig;

mod app_code;
use app_code::AppCodeConfigExt;

use signal_gateway_log_ingest::{JsonConfig, SyslogConfig};

/// Top-level configuration for signal-gateway.
#[derive(Conf, Debug)]
#[conf(serde, test)]
pub struct Config {
    /// Path to a TOML config file (optional).
    /// This is parsed before other args, so config file values can be overridden by CLI args.
    #[allow(dead_code)] // Parsed early via find_parameter, kept here for --help
    #[conf(long)]
    config_file: Option<PathBuf>,
    /// If true, just validate config and don't start
    #[conf(long)]
    dry_run: bool,
    /// Socket to listen for HTTP requests (GET /health, POST /alert)
    #[conf(long, env, default_value = "0.0.0.0:8000")]
    http_listen_addr: SocketAddr,
    #[conf(flatten, prefix)]
    syslog: Option<SyslogConfig>,
    #[conf(flatten, prefix)]
    json: Option<JsonConfig>,
    /// Optional HTTP endpoint for forwarding admin messages.
    #[conf(flatten, prefix)]
    admin_http: Option<AdminHttpConfig>,
    /// Application source code configurations for Claude tools.
    #[conf(long, env, value_parser = serde_json::from_str, default, default_help_str = "[]")]
    app_code: Vec<AppCodeConfigExt>,
    /// Claude API configuration for AI-powered responses.
    #[conf(flatten, prefix)]
    claude: Option<ClaudeConfig>,
    #[conf(flatten, serde(flatten))]
    gateway: GatewayConfig,
}

fn init_logging() {
    // Build a default tracing subscriber, writing to STDERR
    // Uses RUST_LOG env var for filtering, defaults to "info" if not set
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_file(true)
        .with_line_number(true)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // load dotenv file
    match dotenvy::dotenv() {
        Ok(path) => info!("Read dotenv file from: {}", path.display()),
        Err(dotenvy::Error::Io(io_error)) => {
            if matches!(io_error.kind(), std::io::ErrorKind::NotFound) {
                info!("Couldn't find a dotenv file");
            } else {
                panic!("Io error when reading dot env file: {io_error}")
            }
        }
        Err(err) => {
            panic!("Error reading dotenv file: {err}")
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    // Check for --config-file before the main parse, so we can load it and pass to conf
    let config_file_path = conf::find_parameter("config-file", env::args_os());

    let config = if let Some(config_path) = config_file_path {
        let path_display = config_path.to_string_lossy();
        let file_contents = fs::read_to_string(&config_path)
            .map_err(|err| format!("Could not open config file '{path_display}': {err}"))?;
        let doc: toml::Value = toml::from_str(&file_contents)
            .map_err(|err| format!("Config file '{path_display}' is not valid TOML: {err}"))?;
        info!("Loaded config file: {path_display}");
        Config::conf_builder().doc(path_display, doc).parse()
    } else {
        Config::parse()
    };

    info!("Config = {config:#?}");

    if config.dry_run {
        return Ok(());
    }

    let token = CancellationToken::new();

    // Build the command router
    let mut router_builder = CommandRouter::builder()
        .route("--help", Handling::Help)
        .route("-h", Handling::Help);

    // Add admin HTTP handler with its configured prefix
    if let Some(admin_http) = config.admin_http {
        let prefix = admin_http.command_prefix.clone();
        let handler = admin_http.into_handler();
        router_builder = router_builder.route(prefix, Handling::Custom(handler));
    }

    // Add gateway commands for "/" prefix
    router_builder = router_builder.route("/", Handling::GatewayCommand);

    // Add Claude as default handler if configured
    if config.claude.is_some() {
        router_builder = router_builder.route("", Handling::Claude);
    }

    let command_router = router_builder.build();

    // Build AppCode tools if configured
    let app_code_tools = if !config.app_code.is_empty() {
        let mut apps = Vec::new();
        for app_config in config.app_code {
            let name = app_config.config.name.clone();
            match app_config.into_app_code() {
                Ok(app) => apps.push(app),
                Err(e) => {
                    error!("Failed to initialize app code '{name}': {e}");
                    return Err(e.into());
                }
            }
        }
        Some(Arc::new(AppCodeTools::new(apps)))
    } else {
        None
    };

    let mut gateway_builder = Gateway::builder(config.gateway)
        .with_cancellation_token(token.clone())
        .with_command_router(command_router);

    if let Some(tools) = app_code_tools {
        gateway_builder = gateway_builder.with_tools(tools);
    }

    // Add Claude assistant if configured
    if let Some(claude_config) = config.claude {
        gateway_builder = gateway_builder.with_assistant(move |tool_executor| {
            Box::new(
                ClaudeAssistant::new(claude_config, tool_executor)
                    .expect("Failed to initialize Claude assistant"),
            )
        });
    }

    let gateway = gateway_builder.build().await;

    let listener = TcpListener::bind(config.http_listen_addr).await.unwrap();
    info!("Listening for http on {}", config.http_listen_addr);

    // Listen for ctrl-c
    let thread_token = token.clone();
    tokio::task::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        warn!("ctrl-c: Stop requested");
        thread_token.cancel();
    });

    // Start the server tasks
    let _http_task = start_http_task(listener, gateway.clone());
    let _syslog_tasks = if let Some(syslog) = &config.syslog {
        Some(syslog.start_tasks(gateway.clone()).await.unwrap())
    } else {
        None
    };
    let _json_tasks = if let Some(json) = &config.json {
        Some(json.start_tasks(gateway.clone()).await.unwrap())
    } else {
        None
    };

    // Run gateway task and block on it returning. Note that it exits if the token is canceled.
    gateway.run().await;

    Ok(())
}

fn start_http_task(listener: TcpListener, gateway: Arc<Gateway>) -> tokio::task::JoinHandle<()> {
    // Loop waiting for http incoming connections, and pass them to gateway
    tokio::task::spawn(async move {
        loop {
            let Ok((stream, remote_addr)) = listener
                .accept()
                .await
                .inspect_err(|err| error!("Error accepting connection: {err}"))
            else {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            };
            info!("New connection from: {}", remote_addr);

            // Spawn a new task to handle each connection
            let thread_gateway = gateway.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);

                // Serve the connection using auto protocol detection (HTTP/1 or HTTP/2)
                if let Err(err) = auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(
                        io,
                        service_fn(|req| handle_http_request(thread_gateway.clone(), req)),
                    )
                    .await
                {
                    error!("Error serving connection: {err}");
                }
            });
        }
    })
}

async fn handle_http_request(
    gateway: Arc<Gateway>,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<String>, Infallible> {
    match handle_http_request_impl(gateway, req).await {
        Ok(resp) => Ok(resp),
        Err(resp) => Ok(resp),
    }
}

async fn handle_http_request_impl<B>(
    gateway: Arc<Gateway>,
    req: Request<B>,
) -> Result<Response<String>, Response<String>>
where
    B: Body + Send,
    B::Data: Buf + Send,
    B::Error: std::fmt::Display,
{
    info!(
        "Received http request: {} {} (version: {:?})",
        req.method(),
        req.uri().path(),
        req.version()
    );

    fn ok_resp() -> Response<String> {
        Response::new("OK".into())
    }
    fn err_resp(code: StatusCode, text: impl Into<String>) -> Response<String> {
        let mut resp = Response::new(text.into());
        *resp.status_mut() = code;
        resp
    }

    match req.uri().path() {
        "/" | "/health" | "/ready" => {
            if !matches!(req.method(), &Method::GET | &Method::HEAD) {
                Ok(err_resp(
                    StatusCode::NOT_IMPLEMENTED,
                    "Use GET or HEAD with this route",
                ))
            } else {
                Ok(ok_resp())
            }
        }
        "/alert" => {
            if !matches!(req.method(), &Method::POST) {
                return Ok(err_resp(
                    StatusCode::NOT_IMPLEMENTED,
                    "Use POST with this route",
                ));
            }
            let body_bytes = req
                .into_body()
                .collect()
                .await
                .map_err(|err| {
                    err_resp(
                        StatusCode::BAD_REQUEST,
                        format!("When reading body bytes: {err}"),
                    )
                })?
                .to_bytes()
                .to_vec();

            let body_text = str::from_utf8(&body_bytes).map_err(|err| {
                warn!("When reading body bytes: {err}");
                err_resp(StatusCode::BAD_REQUEST, "Request body was not utf-8")
            })?;

            let alert_msg: AlertPost = serde_json::from_str(body_text).map_err(|err| {
                error!("Could not parse json: {err}:\n{body_text}");
                err_resp(StatusCode::BAD_REQUEST, "Invalid Json")
            })?;

            if let Err(msg) = gateway.handle_alertmanager_post(alert_msg).await {
                error!("gateway (handle_alertmanager_post): {msg}");
                Ok(err_resp(StatusCode::INTERNAL_SERVER_ERROR, msg))
            } else {
                Ok(ok_resp())
            }
        }
        _ => Ok(err_resp(
            StatusCode::NOT_FOUND,
            format!("Not found '{} {}'", req.method(), req.uri().path()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conf::Conf;

    #[test]
    fn test_toml_config() {
        let toml_config = r#"
http_listen_addr = "0.0.0.0:8080"
signal_account = "+15551234567"
signal_cli_tcp_addr = "127.0.0.1:7583"
signal_cli_retry_delay = "10s"

[signal_admins]
"12345678-1234-1234-1234-123456789abc" = ["12345 67890 12345 67890 12345 67890 12345 67890 12345 67890 12345 67890"]
"abcdef12-abcd-abcd-abcd-abcdef123456" = []

[syslog]
listen_addr = "0.0.0.0:1514"
sd_id = "tracing-meta@64700"

[json]
listen_addr = "0.0.0.0:5000"

[log_handler]
log_buffer_size = 128
overall_limits = [{ threshold = ">= 100 / 1h" }]

[[log_handler.route]]
alert_level = "warn"
msg_contains = "critical"

[[log_handler.route]]
alert_level = "error"

[[log_handler.route]]
msg_contains = "connection reset"
limits = [
    { threshold = ">= 5 / 1m" },
    { threshold = ">= 20 / 1h" },
]
"#;

        // Parse TOML to a generic value, then use conf's builder to parse it
        let doc: toml::Value = toml::from_str(toml_config).expect("Failed to parse TOML");
        let empty_env: [(&str, &str); 0] = [];
        let config: Config = Config::conf_builder()
            .args(["."])
            .env(empty_env)
            .doc("test.toml", doc)
            .try_parse()
            .expect("Failed to parse config");

        assert_eq!(config.http_listen_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.gateway.signal_account, "+15551234567");
        assert_eq!(
            config.gateway.signal_cli_tcp_addr,
            Some("127.0.0.1:7583".parse().unwrap())
        );
        assert_eq!(
            config.gateway.signal_cli_retry_delay,
            Duration::from_secs(10)
        );
        assert_eq!(config.gateway.signal_admins.len(), 2);
        assert!(
            config
                .gateway
                .signal_admins
                .get("12345678-1234-1234-1234-123456789abc")
                .is_some()
        );

        let syslog = config.syslog.expect("syslog should be present");
        assert_eq!(syslog.listen_addr, "0.0.0.0:1514".parse().unwrap());

        let json = config.json.expect("json should be present");
        assert_eq!(json.listen_addr, "0.0.0.0:5000".parse().unwrap());

        assert_eq!(config.gateway.log_handler.log_buffer_size, 128);
        assert_eq!(config.gateway.log_handler.routes.len(), 3);
        assert_eq!(config.gateway.log_handler.overall_limits.len(), 1);
    }
}
