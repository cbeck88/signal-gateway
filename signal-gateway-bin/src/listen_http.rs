use http::{Method, Request, Response, StatusCode};
use http_body::Body;
use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper_util::{rt::TokioIo, server::conn::auto};
use signal_gateway::{Gateway, alertmanager::AlertPost};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio_util::bytes::Buf;
use tracing::{error, info, warn};

/// Start http listening task
pub fn start_http_task(listener: TcpListener, gateway: Arc<Gateway>) -> tokio::task::JoinHandle<()> {
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

