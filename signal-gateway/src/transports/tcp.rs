use std::io::Error;

use futures_util::stream::StreamExt;
use jsonrpsee::core::client::{TransportReceiverT, TransportSenderT};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio_util::codec::Decoder;

use super::{Receiver, Sender, stream_codec::StreamCodec};

/// Connect to a JSON-RPC TCP server.
pub async fn connect(
    socket: impl ToSocketAddrs,
) -> Result<(impl TransportSenderT + Send, impl TransportReceiverT + Send), Error> {
    let connection = TcpStream::connect(socket).await?;
    let (sink, stream) = StreamCodec::stream_incoming().framed(connection).split();

    let sender = Sender { inner: sink };
    let receiver = Receiver { inner: stream };

    Ok((sender, receiver))
}
