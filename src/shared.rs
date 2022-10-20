//! Shared data structures, utilities, and protocol definitions.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{self, AsyncRead, AsyncWrite, BufWriter, BufReader, AsyncReadExt, AsyncWriteExt};

use tokio::time::timeout;
use tokio_util::codec::{AnyDelimiterCodec, Framed, FramedParts};
use tracing::trace;
use uuid::Uuid;

/// TCP port used for control connections with the server.
pub const CONTROL_PORT: u16 = 7835;

/// Maxmium byte length for a JSON frame in the stream.
pub const MAX_FRAME_LENGTH: usize = 256;

/// Timeout for network connections and initial protocol messages.
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);

/// A message from the client on the control connection.
#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Response to an authentication challenge from the server.
    Authenticate(String),

    /// Initial client message specifying a port to forward.
    Hello(u16),

    /// Accepts an incoming TCP connection, using this stream as a proxy.
    Accept(Uuid),
}

/// A message from the server on the control connection.
#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Authentication challenge, sent as the first message, if enabled.
    Challenge(Uuid),

    /// Response to a client's initial message, with actual public port.
    Hello(u16),

    /// No-op used to test if the client is still reachable.
    Heartbeat,

    /// Asks the client to accept a forwarded TCP connection.
    Connection(Uuid),

    /// Indicates a server error that terminates the connection.
    Error(String),
}

/// Transport stream with JSON frames delimited by null characters.
pub struct Delimited<U>(Framed<U, AnyDelimiterCodec>);

impl<U: AsyncRead + AsyncWrite + Unpin> Delimited<U> {
    /// Construct a new delimited stream.
    pub fn new(stream: U) -> Self {
        let codec = AnyDelimiterCodec::new_with_max_length(vec![0], vec![0], MAX_FRAME_LENGTH);
        Self(Framed::new(stream, codec))
    }

    /// Read the next null-delimited JSON instruction from a stream.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        trace!("waiting to receive json message");
        if let Some(next_message) = self.0.next().await {
            let byte_message = next_message.context("frame error, invalid byte length")?;
            let serialized_obj = serde_json::from_slice(&byte_message.to_vec())
                .context("unable to parse message")?;
            Ok(serialized_obj)
        } else {
            Ok(None)
        }
    }

    /// Read the next null-delimited JSON instruction, with a default timeout.
    ///
    /// This is useful for parsing the initial message of a stream for handshake or
    /// other protocol purposes, where we do not want to wait indefinitely.
    pub async fn recv_timeout<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        timeout(NETWORK_TIMEOUT, self.recv())
            .await
            .context("timed out waiting for initial message")?
    }

    /// Send a null-terminated JSON instruction on a stream.
    pub async fn send<T: Serialize>(&mut self, msg: T) -> Result<()> {
        trace!("sending json message");
        self.0.send(serde_json::to_string(&msg)?).await?;
        Ok(())
    }

    /// Consume this object, returning current buffers and the inner transport.
    pub fn into_parts(self) -> FramedParts<U, AnyDelimiterCodec> {
        self.0.into_parts()
    }
}


use std::sync::Mutex;

static PROXY_SEQ: Mutex<usize> = Mutex::new(0);

/// Copy data mutually between two read/write streams.
pub async fn proxy<S1, S2>(stream1: S1, stream2: S2) -> io::Result<()>
where
    S1: AsyncRead + AsyncWrite + Unpin,
    S2: AsyncRead + AsyncWrite + Unpin,
{
    let (mut s1_read, mut s1_write) = io::split(stream1);
    let (mut s2_read, mut s2_write) = io::split(stream2);
    let mut seq = 0;
    {
        let mut proxy_seq = PROXY_SEQ.lock().unwrap();
        *proxy_seq += 1;
        println!("proxy sequence: {:?}", *proxy_seq);
        seq = *proxy_seq;
        println!("seq: {:?}", seq);
    }
    let mut s1_read_buf: [u8; 8192] = [0; 8192];
    let mut s2_read_buf: [u8; 8192] = [0; 8192];

    if seq == 2 {
        let s1_len = tokio::select! {
            r = s1_read.read(&mut s1_read_buf) => r,
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => Ok(0),
        }?;

        let s2_len = tokio::select! {
            r = s2_read.read(&mut s2_read_buf) => r,
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => Ok(0),
        }?;
        println!("s1 len : {:?}", s1_len);
        println!("s2 len: {:?}", s2_len);

        if s1_len > 0 {
            let mut s1 = &s1_read_buf[..s1_len];
            println!("s1_read_buf: {:?}", s1);
            io::copy(&mut s1, &mut s2_write).await?;
        }
        if s2_len > 0 {
            let mut s2 = &s2_read_buf[..s2_len];
            println!("s2_read_buf: {:?}", s2);
            io::copy(&mut s2, &mut s1_write).await?;
        }
    } else if seq == 3 {
        let body = b"{\"status\":false}";
        io::copy(&mut &body[..], &mut s2_write).await?;
    } else {
        tokio::select! {
            res = io::copy(&mut s1_read, &mut s2_write) => res,
            res = io::copy(&mut s2_read, &mut s1_write) => res,
        }?;
    }
    Ok(())
}
