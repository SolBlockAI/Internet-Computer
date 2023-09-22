//! Quic Transport utilities.
//!
//! Contains the actual wire format used for messages.
//! Request encoding Request<Bytes>:
//!     - Split into header and body.
//!     - Header contains a HeaderMap and the URI
//!     - Body is just the byte vector.
//!     - Both the header and body are encoded with bincode
//!     - At this point both header and body are just a vector of bytes.
//!       The two bytes vector both get length limited encoded and sent.
//!     - Reading a request involves doing two reads from the wire for the
//!       encoded header and body and reconstructing it into a typed request.
//! Response encoding Response<Bytes>:
//!     - Same as request expect that the header contains a HeaderMap and a Statuscode.
use std::io;

use axum::body::{Body, BoxBody, HttpBody};
use bincode::Options;
use bytes::{Buf, BufMut, Bytes};
use http::{Request, Response, StatusCode, Uri};
use quinn::{RecvStream, SendStream};
use serde::{Deserialize, Serialize};

const MAX_MESSAGE_SIZE_BYTES: usize = 8 * 1024 * 1024;

fn bincode_config() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_MESSAGE_SIZE_BYTES as u64)
}

fn bincode_error_to_std_io_error(err: bincode::Error) -> io::Error {
    match *err {
        bincode::ErrorKind::Io(io) => io,
        _ => io::Error::new(
            io::ErrorKind::Other,
            format!(
                "Bincode request wire header deserialization failed: {}",
                err
            ),
        ),
    }
}

pub(crate) async fn read_request(mut recv_stream: RecvStream) -> Result<Request<Body>, io::Error> {
    let raw_msg = recv_stream
        .read_to_end(MAX_MESSAGE_SIZE_BYTES)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e.to_string()))?;
    let msg: WireRequest = bincode_config()
        .deserialize(&raw_msg)
        .map_err(bincode_error_to_std_io_error)?;

    let mut request = Request::new(Body::from(Bytes::copy_from_slice(msg.body)));
    let _ = std::mem::replace(request.uri_mut(), msg.uri);
    Ok(request)
}

pub(crate) async fn read_response(
    mut recv_stream: RecvStream,
) -> Result<Response<Bytes>, io::Error> {
    let raw_msg = recv_stream
        .read_to_end(MAX_MESSAGE_SIZE_BYTES)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e.to_string()))?;
    let msg: WireResponse = bincode_config()
        .deserialize(&raw_msg)
        .map_err(bincode_error_to_std_io_error)?;

    let mut response = Response::new(Bytes::copy_from_slice(msg.body));
    let _ = std::mem::replace(response.status_mut(), msg.status);
    Ok(response)
}

pub(crate) async fn write_request(
    send_stream: &mut SendStream,
    request: Request<Bytes>,
) -> Result<(), io::Error> {
    let (parts, body) = request.into_parts();

    let msg = WireRequest {
        uri: parts.uri,
        body: &body,
    };

    let res = bincode_config()
        .serialize(&msg)
        .map_err(bincode_error_to_std_io_error)?;
    send_stream.write_all(&res).await?;

    Ok(())
}

pub(crate) async fn write_response(
    send_stream: &mut SendStream,
    response: Response<BoxBody>,
) -> Result<(), io::Error> {
    let (parts, body) = response.into_parts();
    // Check for axum error in body
    // TODO: Think about this. What is the error that can happen here?
    let b = to_bytes(body)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    let msg = WireResponse {
        status: parts.status,
        body: &b,
    };

    let res = bincode_config()
        .serialize(&msg)
        .map_err(bincode_error_to_std_io_error)?;
    send_stream.write_all(&res).await?;

    Ok(())
}

#[derive(Serialize, Deserialize)]
struct WireResponse<'a> {
    #[serde(with = "http_serde::status_code")]
    status: StatusCode,
    #[serde(with = "serde_bytes")]
    body: &'a [u8],
}

#[derive(Serialize, Deserialize)]
struct WireRequest<'a> {
    #[serde(with = "http_serde::uri")]
    uri: Uri,
    #[serde(with = "serde_bytes")]
    body: &'a [u8],
}

// Copied from hyper. Used to transform `BoxBodyBytes` to `Bytes`.
// It might look slow but since in our case the data is fully available
// the first data() call will immediately return everything.
// With hyper 1.0 etc. this situation will improve.
async fn to_bytes<T>(body: T) -> Result<Bytes, T::Error>
where
    T: HttpBody + Unpin,
{
    futures::pin_mut!(body);

    // If there's only 1 chunk, we can just return Buf::to_bytes()
    let mut first = if let Some(buf) = body.data().await {
        buf?
    } else {
        return Ok(Bytes::new());
    };

    let second = if let Some(buf) = body.data().await {
        buf?
    } else {
        return Ok(first.copy_to_bytes(first.remaining()));
    };

    // Don't pre-emptively reserve *too* much.
    let rest = (body.size_hint().lower() as usize).min(1024 * 16);
    let cap = first
        .remaining()
        .saturating_add(second.remaining())
        .saturating_add(rest);
    // With more than 1 buf, we gotta flatten into a Vec first.
    let mut vec = Vec::with_capacity(cap);
    vec.put(first);
    vec.put(second);

    while let Some(buf) = body.data().await {
        vec.put(buf?);
    }

    Ok(vec.into())
}
