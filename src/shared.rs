//! Shared data structures, utilities, and protocol definitions.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite};

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

use clap::lazy_static::lazy_static;
use std::collections::btree_map::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::sync::{Mutex, RwLock};

lazy_static! {
    static ref PROXY_SEQ: Mutex<usize> = Mutex::new(0);
    static ref BUF_DATA: Arc<RwLock<BTreeMap<u128, (u8, Vec<u8>)>>> =
        Arc::new(RwLock::new(BTreeMap::new()));
}

/// Copy data mutually between two read/write streams.
pub async fn proxy<S1, S2>(stream1: S1, stream2: S2) -> io::Result<()>
where
    S1: AsyncRead + AsyncWrite + Unpin,
    S2: AsyncRead + AsyncWrite + Unpin,
{
    let (mut s1_read, mut s1_write) = io::split(stream1);
    let (mut s2_read, mut s2_write) = io::split(stream2);

    //tokio::select! {
    //    res = io::copy(&mut s1_read, &mut s2_write) => res,
    //    res = io::copy(&mut s2_read, &mut s1_write) => res,
    //}?;

    let mut s1_read_buf: [u8; 8192] = [0; 8192];
    let mut s2_read_buf: [u8; 8192] = [0; 8192];

    {
        let mut proxy_seq = PROXY_SEQ.lock().await;
        *proxy_seq += 1;
        println!("代理次数: {proxy_seq}");
    }

    let (s1_tx, mut s1_rx) = mpsc::channel::<Vec<u8>>(8192);
    let (s2_tx, mut s2_rx) = mpsc::channel::<Vec<u8>>(8192);

    let mut handle = None;
    {
        let proxy_seq = PROXY_SEQ.lock().await;
        let seq = *proxy_seq;
        if seq == 2 {
            println!("使用第一次请求的数据: 线程启动");
            handle = Some(tokio::spawn(async move {
                let buf_data = BUF_DATA.read().await;
                println!("使用第一次请求的数据: 开始转发数据");

                for (_, (n, data)) in buf_data.iter() {
                    if *n == 0 {
                        s1_tx.send(data.to_vec()).await.unwrap();
                    } else {
                        s2_tx.send(data.to_vec()).await.unwrap();
                    }
                }
            }));
        }
    }

    for _ in 0..20 {
        //let _ = loop {
        tokio::select! {
            res = {
                async {
                    let len = s1_read.read(&mut s1_read_buf).await?;
                    let mut s1 = &s1_read_buf[..len];

                    let mut _buf = Vec::new();
                    {
                        let proxy_seq = PROXY_SEQ.lock().await;
                        let seq = *proxy_seq;
                        if seq == 1 && len > 0 {
                            let time = match SystemTime::now().duration_since(UNIX_EPOCH) {
                                Ok(n) => n.as_nanos(),
                                Err(_) => panic!("SystemTime before UNIX EPOCH!"),
                            };
                            let mut buf_data = BUF_DATA.write().await;
                            buf_data.insert(time, (0, s1.to_vec()));
                            println!("远程->本地; 记录转发数据:\n{:?}", buf_data);
                        } else if seq == 2 {
                            while let Some(data) = s1_rx.recv().await {
                                _buf = data;
                                s1 = &_buf[..];
                            }
                            println!("远程->本地; 转发第一次请求的计算结果");
                        } else if seq == 3 {
                            s1 = b"{\"status\":false}";
                            println!("远程->本地; 转发非TEE环境计算结果");
                        }
                    }

                    io::copy(&mut s1, &mut s2_write).await.unwrap();
                    println!("远程->本地; 转发数据成功");
                    Ok::<(), std::io::Error>(())
                }
            } => res,
            res = {
                async {
                    let len = s2_read.read(&mut s2_read_buf).await?;
                    let mut s2 = &s2_read_buf[..len];

                    let mut _buf = Vec::new();
                    {
                        let proxy_seq = PROXY_SEQ.lock().await;
                        let seq = *proxy_seq;
                        if seq == 1 && len > 0 {
                            let time = match SystemTime::now().duration_since(UNIX_EPOCH) {
                                Ok(n) => n.as_nanos(),
                                Err(_) => panic!("SystemTime before UNIX EPOCH!"),
                            };
                            let mut buf_data = BUF_DATA.write().await;
                            buf_data.insert(time, (1, s2.to_vec()));
                            println!("本地->远程; 记录转发数据:\n{:?}", buf_data);
                        } else if seq == 2 {
                            while let Some(data) = s2_rx.recv().await {
                                _buf = data;
                                s2 = &_buf[..];
                            }
                            println!("本地->远程; 转发第一次请求的计算结果");
                        } else if seq == 3 {
                            s2 = b"{\"status\":false}";
                            println!("本地->远程; 转发非TEE环境计算结果");
                        }
                    }

                    io::copy(&mut s2, &mut s1_write).await.unwrap();
                    println!("远程->本地; 转发数据成功");
                    Ok::<(), std::io::Error>(())
                }
            }=> res,
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => Ok(()),
        }?;
    }
    if let Some(handle) = handle {
        handle.await?;
    }
    Ok(())
}
