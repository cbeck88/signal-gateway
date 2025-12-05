use conf::Conf;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto;
use signal_gateway::{Gateway, GatewayConfig};
use std::{net::SocketAddr, str::FromStr, sync::Arc, time::Duration};
use syslog_rfc5424::SyslogMessage;
use tokio::net::{TcpListener, UdpSocket};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Conf, Debug)]
struct Config {
    /// If true, just validate config and don't start
    #[conf(long)]
    dry_run: bool,
    /// Socket to listen for HTTP requests (GET /health, POST /alert)
    #[conf(long, env, default_value = "0.0.0.0:8000")]
    http_listen_addr: SocketAddr,
    /// Socket to listen for UDP messages, in syslog RFC 5424 format
    #[conf(long, env, default_value = "0.0.0.0:5424")]
    udp_listen_addr: SocketAddr,
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

    let gateway = Arc::new(Gateway::new(config.gateway, token.clone()).await);

    let listener = TcpListener::bind(config.http_listen_addr).await.unwrap();
    info!("Listening for http on {}", config.http_listen_addr);

    let udp_socket = UdpSocket::bind(config.udp_listen_addr).await.unwrap();
    info!("Listening for udp on {}", config.udp_listen_addr);

    // Listen for ctrl-c
    let thread_token = token.clone();
    tokio::task::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        warn!("ctrl-c: Stop requested");
        thread_token.cancel();
    });

    // Start the two server tasks
    let _http_task = start_http_task(listener, gateway.clone());
    let _udp_task = start_udp_task(udp_socket, gateway.clone());

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

fn start_udp_task(udp_socket: UdpSocket, gateway: Arc<Gateway>) -> tokio::task::JoinHandle<()> {
    // Loop waiting for http incoming connections, and pass them to gateway
    tokio::task::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            let Ok((len, _addr)) = udp_socket
                .recv_from(&mut buf)
                .await
                .inspect_err(|err| error!("Error receiving UDP packet: {err}"))
            else {
                continue;
            };

            let Ok(text) = str::from_utf8(&buf[0..len])
                .inspect_err(|err| error!("UDP packet was not utf8: {err}"))
            else {
                continue;
            };

            let Ok(msg) = SyslogMessage::from_str(text)
                .inspect_err(|err| error!("UDP packet was not valid syslog: {err}:\n{text}"))
            else {
                continue;
            };

            gateway.handle_syslog_message(msg).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_syslog_parsing() {
        SyslogMessage::from_str(
            "<12>1 2025-11-08T02:24:10.815221698+00:00 ip-172-31-5-8 app 92748 - - Dropped 3/4 reports",
        )
        .unwrap();
        SyslogMessage::from_str(
            "<12>1 2025-11-08T02:24:10.815221698+00:00 ip-172-31-5-8 app 92748 - - Dropped 3/4 reports",
        )
        .unwrap();
        SyslogMessage::from_str(
            "<12>1 2025-11-08T02:24:10.815+00:00 ip-172-31-5-8 app 92748 - - Dropped 3/4 reports due to staleness"
        )
        .unwrap();
    }
}
