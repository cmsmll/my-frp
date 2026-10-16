use crate::common::*;
use anyhow::{Result, bail};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, put},
};
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::{Semaphore, mpsc},
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(version, about = "my-frp 客户端，内嵌本地 Web 管理界面")]
pub struct Options {
    #[arg(long, default_value = "127.0.0.1:7401")]
    pub admin_listen: SocketAddr,
    #[arg(long, default_value = "data/client")]
    pub data_dir: PathBuf,
    #[arg(long, env = "MYFRP_ADMIN_PASSWORD", hide_env_values = true)]
    pub admin_password: Option<String>,
    #[arg(long)]
    pub reset_password: bool,
    #[arg(long)]
    pub allow_insecure: bool,
    #[arg(long)]
    pub ca_cert: Option<PathBuf>,
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,
    #[arg(long)]
    pub tls_key: Option<PathBuf>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    id: String,
    name: String,
    local_addr: String,
    token: String,
    enabled: bool,
}
#[derive(Deserialize)]
struct Input {
    name: String,
    local_addr: String,
    #[serde(default)]
    token: String,
    enabled: bool,
}
struct Runtime {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
    status: Arc<Mutex<Status>>,
}
struct Client {
    admin: Arc<Admin>,
    entries: Mutex<HashMap<String, Entry>>,
    runtimes: Mutex<HashMap<String, Runtime>>,
    mutation: tokio::sync::Mutex<()>,
    shutdown: CancellationToken,
    allow_insecure: bool,
    connector: Option<Connector>,
}
type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
async fn connect(
    endpoint: &str,
    path: &str,
    credential: &str,
    connector: Option<Connector>,
) -> Result<Ws> {
    let mut url = url::Url::parse(endpoint)?;
    url.set_path(path);
    let mut request = url.as_str().into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {credential}").parse()?);
    let config = WebSocketConfig::default()
        .max_message_size(Some(65536))
        .max_frame_size(Some(65536));
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async_tls_with_config(request, Some(config), true, connector),
    )
    .await??;
    Ok(socket)
}
impl Client {
    async fn stop(&self, id: &str) {
        let r = self.runtimes.lock().unwrap().remove(id);
        if let Some(r) = r {
            r.cancel.cancel();
            let _ = r.handle.await;
            tracing::info!(tunnel_id = id, "本地隧道连接已停止");
        }
    }
    fn start(self: &Arc<Self>, entry: Entry) {
        if !entry.enabled {
            return;
        }
        tracing::info!(tunnel_id = %entry.id, local_addr = %entry.local_addr, "正在启动本地隧道连接");
        let status = Arc::new(Mutex::new(Status {
            state: "连接中".into(),
            local: "未检测".into(),
            ..Default::default()
        }));
        let cancel = self.shutdown.child_token();
        let c = self.clone();
        let s = status.clone();
        let stop = cancel.clone();
        let id = entry.id.clone();
        let handle = tokio::spawn(async move {
            let mut delay = 1u64;
            loop {
                let claims = match discover(&entry.token, c.allow_insecure) {
                    Ok(claims) => claims,
                    Err(e) => {
                        tracing::warn!(tunnel_id = %entry.id, error = %e, "隧道凭证无效，已停止重连");
                        let mut s = s.lock().unwrap();
                        s.state = "凭证无效".into();
                        s.error = Some(e.to_string());
                        break;
                    }
                };
                s.lock().unwrap().state = "连接中".into();
                let session = stop.child_token();
                let result = tokio::select! {_=stop.cancelled()=>{session.cancel();break;},_=tokio::time::sleep(Duration::from_secs(claims.exp.saturating_sub(now())))=>Err(anyhow::anyhow!("凭证已过期")),r=c.session(&entry,&claims,s.clone(),session.clone())=>r};
                session.cancel();
                let terminal=result.as_ref().err().is_some_and(|e|e.downcast_ref::<tokio_tungstenite::tungstenite::Error>().is_some_and(|e|matches!(e,tokio_tungstenite::tungstenite::Error::Http(r) if r.status().as_u16()==401)));
                if let Err(error) = &result {
                    tracing::warn!(tunnel_id = %entry.id, error = %error, "服务端连接失败或已断开");
                }
                {
                    let mut st = s.lock().unwrap();
                    if st.state == "在线" {
                        delay = 1;
                    }
                    st.state = if terminal {
                        "凭证失效"
                    } else {
                        "重连中"
                    }
                    .into();
                    st.error = result.err().map(|e| e.to_string());
                }
                if terminal {
                    tracing::warn!(tunnel_id = %entry.id, "服务端拒绝了凭证，已停止重连，请替换连接凭证");
                    break;
                }
                let wait =
                    Duration::from_millis(delay * 1000 + rand::random::<u16>() as u64 % 1000);
                tracing::info!(tunnel_id = %entry.id, retry_after_ms = wait.as_millis() as u64, "等待重新连接服务端");
                tokio::select! {_=stop.cancelled()=>break,_=tokio::time::sleep(wait)=>{}}
                delay = (delay * 2).min(30);
            }
        });
        self.runtimes.lock().unwrap().insert(
            id,
            Runtime {
                cancel,
                handle,
                status,
            },
        );
    }
    async fn session(
        &self,
        e: &Entry,
        claims: &Claims,
        status: Arc<Mutex<Status>>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let socket = connect(
            &claims.endpoint,
            "/control",
            &e.token,
            self.connector.clone(),
        )
        .await?;
        let (mut sink, mut source) = socket.split();
        let ready = tokio::time::timeout(Duration::from_secs(10), source.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("服务端拒绝连接，可能已有客户端在线"))??;
        let Message::Text(ready) = ready else {
            bail!("服务端握手无效");
        };
        let Control::Ready {
            protocol: 1,
            public_port,
        } = serde_json::from_str(&ready)?
        else {
            bail!("不兼容的协议版本");
        };
        {
            let mut s = status.lock().unwrap();
            s.state = "在线".into();
            s.error = None;
            s.public_port = Some(public_port);
            s.last_seen = Some(now());
        }
        tracing::info!(tunnel_id = %e.id, endpoint = %claims.endpoint, public_port, "隧道控制连接已上线");
        let (tx, mut rx) = mpsc::channel::<Control>(128);
        let limit = Arc::new(Semaphore::new(10000));
        loop {
            tokio::select! {
                msg=tokio::time::timeout(Duration::from_secs(35),source.next())=>{
                    match msg?.ok_or_else(||anyhow::anyhow!("服务端已断开"))?? {
                        Message::Text(text)=>match serde_json::from_str::<Control>(&text)? {
                            Control::Ping=>{status.lock().unwrap().last_seen=Some(now());sink.send(Message::Text(serde_json::to_string(&Control::Pong)?.into())).await?;},
                            Control::Open{ticket}=>{
                                let Ok(permit)=limit.clone().try_acquire_owned()else{sink.send(Message::Text(serde_json::to_string(&Control::Failed{ticket})?.into())).await?;continue;};
                                let local=e.local_addr.clone();let endpoint=claims.endpoint.clone();let connector=self.connector.clone();let token=cancel.clone();let st=status.clone();let tx=tx.clone();let tunnel_id=e.id.clone();
                                tokio::spawn(async move {
                                    let _permit=permit;
                                    let work=async {
                                        let tcp=match tokio::time::timeout(Duration::from_secs(5),TcpStream::connect(&local)).await {
                                            Ok(Ok(t))=>{st.lock().unwrap().local="可连接".into();t},
                                            other=>{st.lock().unwrap().local="不可连接".into();return Err(anyhow::anyhow!("本地服务连接失败: {}",match other{Ok(Err(e))=>e.to_string(),_=>"连接超时".into()}));}
                                        };
                                        let ws=connect(&endpoint,"/data",&ticket,connector).await?;
                                        crate::bridge::bridge(tcp,ws,st.clone()).await
                                    };
                                    tokio::select!{_=token.cancelled()=>{},r=work=>{if let Err(e)=r{tracing::warn!(tunnel_id=%tunnel_id,local_addr=%local,error=%e,"本地服务连接或隧道数据转发失败");st.lock().unwrap().error=Some(e.to_string());let _=tx.try_send(Control::Failed{ticket});}}}
                                });
                            },
                            _=>bail!("服务端控制消息无效"),
                        },
                        Message::Ping(_)|Message::Pong(_)=>{},_=>bail!("服务端关闭了控制连接"),
                    }
                },
                Some(msg)=rx.recv()=>sink.send(Message::Text(serde_json::to_string(&msg)?.into())).await?,
            }
        }
    }
}
pub async fn run(o: Options) -> Result<()> {
    if o.tls_cert.is_some() != o.tls_key.is_some() {
        bail!("证书和私钥必须同时配置");
    }
    if !o.admin_listen.ip().is_loopback() && o.tls_cert.is_none() {
        bail!("远程访问客户端界面需要 TLS");
    }
    let db = Db::open(&o.data_dir)?;
    let admin = Admin::new(db.clone(), o.admin_password, o.reset_password, "client")?;
    let entries: Vec<Entry> = db.all()?;
    let shutdown = CancellationToken::new();
    let connector = if let Some(path) = o.ca_cert {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
        for cert in rustls_pemfile::certs(&mut reader) {
            roots.add(cert?)?;
        }
        Some(Connector::Rustls(Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )))
    } else {
        None
    };
    let c = Arc::new(Client {
        admin: admin.clone(),
        entries: Mutex::new(entries.iter().map(|e| (e.id.clone(), e.clone())).collect()),
        runtimes: Mutex::new(HashMap::new()),
        mutation: tokio::sync::Mutex::new(()),
        shutdown: shutdown.clone(),
        allow_insecure: o.allow_insecure,
        connector,
    });
    for e in entries {
        c.start(e);
    }
    let app = admin_routes(admin).merge(
        Router::new()
            .route("/api/tunnels", get(list).post(create))
            .route("/api/tunnels/{id}", put(update).delete(remove))
            .with_state(c),
    );
    let cancel = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel.cancel();
    });
    tracing::info!(admin=%o.admin_listen,"客户端启动");
    let result = serve(o.admin_listen, app, o.tls_cert, o.tls_key, shutdown.clone()).await;
    shutdown.cancel();
    if let Err(error) = &result {
        tracing::error!(error = %error, "客户端停止：管理服务异常");
    } else {
        tracing::info!("客户端已停止");
    }
    result
}
async fn list(State(c): State<Arc<Client>>, h: HeaderMap) -> ApiResult<Json<Value>> {
    c.admin.check(&h)?;
    let entries = c.entries.lock().unwrap();
    let runtimes = c.runtimes.lock().unwrap();
    let mut items:Vec<_>=entries.values().map(|e|{
        let claims=unverified_claims(&e.token).ok();let status=runtimes.get(&e.id).map(|r|r.status.lock().unwrap().clone()).unwrap_or(Status{state:"已停止".into(),local:"未检测".into(),..Default::default()});
        json!({"config":{"id":e.id,"name":e.name,"local_addr":e.local_addr,"enabled":e.enabled,"endpoint":claims.as_ref().map(|c|&c.endpoint),"expires_at":claims.as_ref().map(|c|c.exp)},"status":status})
    }).collect();
    items.sort_by_key(|v| v["config"]["name"].as_str().unwrap_or_default().to_owned());
    Ok(Json(
        json!({"items":items,"allow_insecure":c.allow_insecure}),
    ))
}
fn validate(c: &Client, v: &Input, token: &str) -> ApiResult<()> {
    if v.name.trim().is_empty() || v.name.len() > 100 {
        return Err(bad("名称长度应为 1–100 字节"));
    }
    let u = url::Url::parse(&format!("tcp://{}", v.local_addr))
        .map_err(|_| bad("本地地址格式应为 主机:端口"))?;
    if u.host_str().is_none()
        || u.port().is_none_or(|p| p == 0)
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || !u.path().is_empty()
    {
        return Err(bad("本地地址格式应为 主机:端口，IPv6 使用 [::1]:端口"));
    }
    discover(token, c.allow_insecure).map_err(|e| bad(e.to_string()))?;
    Ok(())
}
async fn create(
    State(c): State<Arc<Client>>,
    h: HeaderMap,
    Json(v): Json<Input>,
) -> ApiResult<Json<Value>> {
    c.admin.check(&h)?;
    validate(&c, &v, &v.token)?;
    let _guard = c.mutation.lock().await;
    let e = Entry {
        id: uuid::Uuid::new_v4().to_string(),
        name: v.name,
        local_addr: v.local_addr,
        token: v.token,
        enabled: v.enabled,
    };
    c.admin.db.save(&e.id, &e, "create")?;
    c.entries.lock().unwrap().insert(e.id.clone(), e.clone());
    tracing::info!(tunnel_id = %e.id, local_addr = %e.local_addr, enabled = e.enabled, "本地隧道配置已创建");
    c.start(e.clone());
    Ok(Json(json!({"id":e.id})))
}
async fn update(
    State(c): State<Arc<Client>>,
    Path(id): Path<String>,
    h: HeaderMap,
    Json(v): Json<Input>,
) -> ApiResult<Json<Value>> {
    c.admin.check(&h)?;
    let _guard = c.mutation.lock().await;
    let old = c
        .entries
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| bad("连接不存在"))?;
    let token = if v.token.is_empty() {
        old.token
    } else {
        v.token.clone()
    };
    // Expired credentials must still be stoppable; starting requires a usable token.
    if v.enabled {
        validate(&c, &v, &token)?;
    }
    let e = Entry {
        id: id.clone(),
        name: v.name,
        local_addr: v.local_addr,
        token,
        enabled: v.enabled,
    };
    c.admin.db.save(&id, &e, "update")?;
    c.entries.lock().unwrap().insert(id.clone(), e.clone());
    c.stop(&id).await;
    tracing::info!(tunnel_id = %id, local_addr = %e.local_addr, enabled = e.enabled, "本地隧道配置已更新");
    c.start(e);
    Ok(Json(json!({"ok":true})))
}
async fn remove(
    State(c): State<Arc<Client>>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> ApiResult<Json<Value>> {
    c.admin.check(&h)?;
    let _guard = c.mutation.lock().await;
    c.admin.db.remove(&id)?;
    c.entries.lock().unwrap().remove(&id);
    c.stop(&id).await;
    tracing::info!(tunnel_id = %id, "本地隧道配置已删除");
    Ok(Json(json!({"ok":true})))
}
