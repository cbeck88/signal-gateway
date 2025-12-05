use conf::{Conf, Subcommands};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto;
use signal_gateway::{Gateway, GatewayConfig};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

mod admin_http;
use admin_http::AdminHttpConfig;

mod admin_netcat;
use admin_netcat::AdminNetcatConfig;

mod syslog;
use syslog::SyslogConfig;

mod json;
use json::JsonConfig;

/// Admin message handler configuration - select how non-command messages are handled
#[derive(Clone, Debug, Subcommands)]
enum AdminHandlerCommand {
    /// Forward (unhandled) admin messages to a TCP endpoint (netcat-style)
    /// Useful if an http server would be heavy in the target process
    #[conf(name = "admin-netcat")]
    Netcat(AdminNetcatConfig),
    /// Forward (unhandled) admin messages to an HTTP endpoint via POST
    #[conf(name = "admin-http")]
    Http(AdminHttpConfig),
}

impl AdminHandlerCommand {
    fn into_handler(self) -> Box<dyn signal_gateway::MessageHandler> {
        match self {
            AdminHandlerCommand::Netcat(config) => config.into_handler(),
            AdminHandlerCommand::Http(config) => config.into_handler(),
        }
    }
}

#[derive(Conf, Debug)]
struct Config {
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
    /// Optional admin message handler (netcat or http)
    #[conf(subcommands)]
    admin_handler: Option<AdminHandlerCommand>,
    #[conf(flatten)]
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
async fn main() {
    init_logging();

    let config = Config::parse();

    info!("Config = {config:#?}");

    if config.dry_run {
        return;
    }

    let token = CancellationToken::new();

    let message_handler = config.admin_handler.map(|c| c.into_handler());
    let gateway = Arc::new(Gateway::new(config.gateway, token.clone(), message_handler).await);

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
                        service_fn(|req| {
                            let thread_gateway = thread_gateway.clone();
                            async move { thread_gateway.handle_http_request(req).await }
                        }),
                    )
                    .await
                {
                    error!("Error serving connection: {err}");
                }
            });
        }
    })
}
