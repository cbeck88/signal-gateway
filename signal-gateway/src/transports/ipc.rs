use std::{io::Error, path::Path};

use futures_util::stream::StreamExt;
use jsonrpsee::core::client::{TransportReceiverT, TransportSenderT};
use tokio::net::UnixStream;
use tokio_util::codec::Decoder;

use super::{Receiver, Sender, stream_codec::StreamCodec};

/// Connect to a JSON-RPC server via Unix domain socket.
pub async fn connect(
    socket: impl AsRef<Path>,
) -> Result<(impl TransportSenderT + Send, impl TransportReceiverT + Send), Error> {
    let connection = UnixStream::connect(socket).await?;
    let (sink, stream) = StreamCodec::stream_incoming().framed(connection).split();

    let sender = Sender { inner: sink };
    let receiver = Receiver { inner: stream };

    Ok((sender, receiver))
}
