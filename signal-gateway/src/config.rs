use crate::gateway::GatewayConfig;
use conf::Conf;
use std::net::SocketAddr;

#[derive(Conf, Debug)]
pub struct Config {
    /// If true, just validate config and don't start
    #[conf(long)]
    pub dry_run: bool,
    /// Socket to listen for HTTP requests (GET /health, POST /alert)
    #[conf(long, env, default_value = "0.0.0.0:8000")]
    pub http_listen_addr: SocketAddr,
    /// Socket to listen for UDP messages, in syslog RFC 5424 format
    #[conf(long, env, default_value = "0.0.0.0:5424")]
    pub udp_listen_addr: SocketAddr,
    #[conf(flatten)]
    pub gateway: GatewayConfig,
}
