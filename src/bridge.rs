use crate::common::{Status, now};
use anyhow::{Result, bail};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub enum Frame {
    Data(Vec<u8>),
    Eof,
    Ignore,
    Closed,
    Invalid,
}
pub trait Wire: Send {
    fn data(b: Vec<u8>) -> Self;
    fn eof() -> Self;
    fn frame(self) -> Frame;
}
impl Wire for axum::extract::ws::Message {
    fn data(b: Vec<u8>) -> Self {
        Self::Binary(b.into())
    }
    fn eof() -> Self {
        Self::Text("eof".into())
    }
    fn frame(self) -> Frame {
        match self {
            Self::Binary(b) => Frame::Data(b.to_vec()),
            Self::Text(t) if t == "eof" => Frame::Eof,
            Self::Ping(_) | Self::Pong(_) => Frame::Ignore,
            Self::Close(_) => Frame::Closed,
            _ => Frame::Invalid,
        }
    }
}
impl Wire for tokio_tungstenite::tungstenite::Message {
    fn data(b: Vec<u8>) -> Self {
        Self::Binary(b.into())
    }
    fn eof() -> Self {
        Self::Text("eof".into())
    }
    fn frame(self) -> Frame {
        match self {
            Self::Binary(b) => Frame::Data(b.to_vec()),
            Self::Text(t) if t == "eof" => Frame::Eof,
            Self::Ping(_) | Self::Pong(_) => Frame::Ignore,
            Self::Close(_) => Frame::Closed,
            _ => Frame::Invalid,
        }
    }
}
struct Active(Arc<Mutex<Status>>);
impl Drop for Active {
    fn drop(&mut self) {
        let mut s = self.0.lock().unwrap();
        s.active = s.active.saturating_sub(1);
    }
}
pub async fn bridge<W, M, E>(tcp: TcpStream, ws: W, stats: Arc<Mutex<Status>>) -> Result<()>
where
    W: Sink<M, Error = E> + Stream<Item = std::result::Result<M, E>> + Unpin,
    M: Wire,
    E: std::error::Error + Send + Sync + 'static,
{
    tcp.set_nodelay(true)?;
    stats.lock().unwrap().active += 1;
    let _active = Active(stats.clone());
    let (mut read, mut write) = tcp.into_split();
    let (mut sink, mut source) = ws.split();
    let activity = AtomicU64::new(now());
    let upload = async {
        let mut buf = vec![0; 32768];
        loop {
            let n = read.read(&mut buf).await?;
            if n == 0 {
                sink.send(M::eof()).await?;
                break;
            }
            sink.send(M::data(buf[..n].to_vec())).await?;
            activity.store(now(), Ordering::Relaxed);
            stats.lock().unwrap().sent += n as u64;
        }
        Ok::<(), anyhow::Error>(())
    };
    let download = async {
        while let Some(m) = source.next().await {
            match m?.frame() {
                Frame::Data(b) => {
                    write.write_all(&b).await?;
                    activity.store(now(), Ordering::Relaxed);
                    stats.lock().unwrap().received += b.len() as u64;
                }
                Frame::Eof => {
                    write.shutdown().await?;
                    return Ok::<(), anyhow::Error>(());
                }
                Frame::Ignore => {}
                Frame::Closed => bail!("数据连接提前关闭"),
                Frame::Invalid => bail!("数据帧无效"),
            }
        }
        bail!("数据连接中断")
    };
    let idle = async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if now().saturating_sub(activity.load(Ordering::Relaxed)) >= 300 {
                break;
            }
        }
    };
    tokio::select! {r=async {tokio::try_join!(upload,download)?;Ok(())}=>r,_=idle=>bail!("数据连接空闲超过 300 秒")}
}
