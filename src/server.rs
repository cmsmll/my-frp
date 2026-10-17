use crate::common::*;
use anyhow::{Result, bail};
use axum::{
    Json, Router,
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::Response,
    routing::{get, post},
};
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use futures_util::{SinkExt, StreamExt};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

mod config;
pub use config::Config;
#[derive(Clone, Serialize, Deserialize)]
pub struct Tunnel {
    pub id: String,
    pub name: String,
    pub bind_ip: IpAddr,
    pub port: u16,
    pub enabled: bool,
    pub max_connections: usize,
    pub version: u64,
    pub expires_at: u64,
    pub jti: String,
}
#[derive(Deserialize)]
struct Input {
    name: String,
    bind_ip: IpAddr,
    port: u16,
    enabled: bool,
    max_connections: usize,
    #[serde(default = "default_days")]
    ttl_days: u64,
}
fn default_days() -> u64 {
    30
}
struct Session {
    id: String,
    tx: mpsc::Sender<Control>,
    cancel: CancellationToken,
}
struct Pending {
    session_id: String,
    cancel: CancellationToken,
    expires: u64,
    tx: oneshot::Sender<WebSocket>,
}
struct Runtime {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
    status: Arc<Mutex<Status>>,
}
pub struct Server {
    admin: Arc<Admin>,
    options: Config,
    issuer: String,
    signing: EncodingKey,
    verifying: DecodingKey,
    tunnels: Mutex<HashMap<String, Tunnel>>,
    sessions: Mutex<HashMap<String, Session>>,
    pending: Mutex<HashMap<String, Pending>>,
    runtimes: Mutex<HashMap<String, Runtime>>,
    mutation: tokio::sync::Mutex<()>,
    shutdown: CancellationToken,
}
impl Server {
    fn token(&self, t: &Tunnel) -> Result<String> {
        Ok(jsonwebtoken::encode(
            &Header::new(Algorithm::EdDSA),
            &Claims {
                iss: self.issuer.clone(),
                aud: "my-frp-client".into(),
                sub: t.id.clone(),
                jti: t.jti.clone(),
                iat: now(),
                nbf: now(),
                exp: t.expires_at,
                endpoint: self.options.public_url.clone(),
                version: t.version,
            },
            &self.signing,
        )?)
    }
    fn verify(&self, token: &str) -> ApiResult<Claims> {
        let mut v = Validation::new(Algorithm::EdDSA);
        v.set_audience(&["my-frp-client"]);
        v.set_issuer(&[&self.issuer]);
        v.leeway = 0;
        v.validate_nbf = true;
        let c = jsonwebtoken::decode::<Claims>(token, &self.verifying, &v)
            .map_err(|_| unauthorized())?
            .claims;
        let ts = self.tunnels.lock().unwrap();
        let t = ts.get(&c.sub).ok_or_else(unauthorized)?;
        if !t.enabled || c.version != t.version || c.jti != t.jti || c.exp <= now() {
            return Err(unauthorized());
        }
        Ok(c)
    }
    fn disconnect(&self, id: &str) {
        if let Some(s) = self.sessions.lock().unwrap().remove(id) {
            s.cancel.cancel();
            self.pending
                .lock()
                .unwrap()
                .retain(|_, p| p.session_id != s.id);
        }
    }
    async fn stop(&self, id: &str) {
        self.disconnect(id);
        let r = self.runtimes.lock().unwrap().remove(id);
        if let Some(r) = r {
            r.cancel.cancel();
            let _ = r.handle.await;
            tracing::info!(tunnel_id = id, "隧道监听已停止");
        }
    }
    fn start(self: &Arc<Self>, t: Tunnel, listener: TcpListener) {
        tracing::info!(tunnel_id = %t.id, listen = %SocketAddr::new(t.bind_ip, t.port), "隧道开始监听");
        let cancel = self.shutdown.child_token();
        let status = Arc::new(Mutex::new(Status {
            state: "监听中".into(),
            ..Default::default()
        }));
        let a = self.clone();
        let c = cancel.clone();
        let s = status.clone();
        let id = t.id.clone();
        let handle = tokio::spawn(async move {
            let limit = Arc::new(Semaphore::new(t.max_connections));
            loop {
                let accepted = tokio::select! {_=c.cancelled()=>break,r=listener.accept()=>r};
                let (tcp, _) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(tunnel_id = %t.id, error = %e, "隧道监听失败");
                        let mut st = s.lock().unwrap();
                        st.state = "监听失败".into();
                        st.error = Some(e.to_string());
                        break;
                    }
                };
                let Ok(permit) = limit.clone().try_acquire_owned() else {
                    continue;
                };
                let session = a
                    .sessions
                    .lock()
                    .unwrap()
                    .get(&t.id)
                    .map(|x| (x.id.clone(), x.tx.clone(), x.cancel.clone()));
                let Some((sid, tx, session_cancel)) = session else {
                    continue;
                };
                if session_cancel.is_cancelled() {
                    continue;
                }
                let ticket = random();
                let (send, recv) = oneshot::channel();
                a.pending.lock().unwrap().insert(
                    ticket.clone(),
                    Pending {
                        session_id: sid,
                        cancel: session_cancel.clone(),
                        expires: now() + 15,
                        tx: send,
                    },
                );
                if tx
                    .try_send(Control::Open {
                        ticket: ticket.clone(),
                    })
                    .is_err()
                {
                    a.pending.lock().unwrap().remove(&ticket);
                    continue;
                }
                let a2 = a.clone();
                let cc = c.clone();
                let ss = s.clone();
                let tunnel_id = t.id.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let transfer = async {
                        let ws = tokio::time::timeout(Duration::from_secs(15), recv).await??;
                        crate::bridge::bridge(tcp, ws, ss.clone()).await
                    };
                    tokio::select! {_=cc.cancelled()=>{},_=session_cancel.cancelled()=>{},r=transfer=>{if let Err(e)=r {tracing::warn!(tunnel_id=%tunnel_id,error=%e,"隧道数据转发失败或等待数据连接超时");ss.lock().unwrap().error=Some(e.to_string());}}}
                    a2.pending.lock().unwrap().remove(&ticket);
                });
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
}
pub async fn run(o: Config) -> Result<()> {
    if o.port_min == 0 || o.port_min > o.port_max {
        bail!("端口范围无效");
    }
    let probe = Claims {
        iss: String::new(),
        aud: "my-frp-client".into(),
        sub: String::new(),
        jti: String::new(),
        iat: now(),
        nbf: now(),
        exp: now() + 60,
        endpoint: o.public_url.clone(),
        version: 1,
    };
    use base64::Engine;
    discover(
        &format!(
            "x.{}.x",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&probe)?)
        ),
        o.allow_insecure,
    )?;
    let tls = o.tls_cert.is_some() && o.tls_key.is_some();
    if o.tls_cert.is_some() != o.tls_key.is_some() {
        bail!("证书和私钥必须同时配置");
    }
    if !tls && !o.allow_insecure {
        bail!(
            "请在服务端 JSON 中配置 tls_cert 和 tls_key；本地调试可设置 allow_insecure 为 true、public_url 为 ws://127.0.0.1:10088"
        );
    }
    if tls != o.public_url.starts_with("wss://") {
        bail!("public_url 的 ws/wss 必须与 TLS 配置匹配");
    }
    if !o.admin_listen.ip().is_loopback() && !tls {
        bail!("非本机管理界面必须启用 TLS");
    }
    let db = Db::open(&o.data_dir)?;
    let admin = Admin::new(
        db.clone(),
        o.admin_password.clone(),
        o.reset_password,
        "server",
    )?;
    let key_path = o.data_dir.join("signing.pem");
    let key = if key_path.exists() {
        ed25519_dalek::SigningKey::from_pkcs8_pem(&std::fs::read_to_string(&key_path)?)?
    } else {
        let key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let pem = key.to_pkcs8_pem(Default::default())?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write;
        opts.open(&key_path)?.write_all(pem.as_bytes())?;
        key
    };
    let signing = EncodingKey::from_ed_der(key.to_pkcs8_der()?.as_bytes());
    let verifying = DecodingKey::from_ed_der(key.verifying_key().as_bytes());
    let issuer = match db.setting("issuer")? {
        Some(i) => i,
        None => {
            let i = format!("my-frp:{}", random());
            db.set("issuer", &i)?;
            i
        }
    };
    let tunnels: Vec<Tunnel> = db.all()?;
    let shutdown = CancellationToken::new();
    let s = Arc::new(Server {
        admin: admin.clone(),
        options: o,
        issuer,
        signing,
        verifying,
        tunnels: Mutex::new(tunnels.iter().map(|t| (t.id.clone(), t.clone())).collect()),
        sessions: Mutex::new(HashMap::new()),
        pending: Mutex::new(HashMap::new()),
        runtimes: Mutex::new(HashMap::new()),
        mutation: tokio::sync::Mutex::new(()),
        shutdown: shutdown.clone(),
    });
    for t in tunnels.into_iter().filter(|t| t.enabled) {
        match TcpListener::bind(SocketAddr::new(t.bind_ip, t.port)).await {
            Ok(l) => s.start(t, l),
            Err(e) => {
                tracing::error!(tunnel_id = %t.id, port = t.port, error = %e, "恢复隧道监听失败");
                let status = Arc::new(Mutex::new(Status {
                    state: "监听失败".into(),
                    error: Some(e.to_string()),
                    ..Default::default()
                }));
                s.runtimes.lock().unwrap().insert(
                    t.id,
                    Runtime {
                        cancel: shutdown.child_token(),
                        handle: tokio::spawn(async {}),
                        status,
                    },
                );
            }
        }
    }
    let admin_app = admin_routes(admin).merge(
        Router::new()
            .route("/api/tunnels", get(list).post(create))
            .route(
                "/api/tunnels/{id}",
                axum::routing::put(update).delete(remove),
            )
            .route("/api/tunnels/{id}/rotate", post(rotate))
            .with_state(s.clone()),
    );
    let tunnel_app = Router::new()
        .route("/control", get(control))
        .route("/data", get(data))
        .with_state(s.clone());
    let cancel = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel.cancel();
    });
    tracing::info!(admin=%s.options.admin_listen,tunnel=%s.options.tunnel_listen,"服务端启动");
    let result = tokio::try_join!(
        serve(
            s.options.admin_listen,
            admin_app,
            s.options.tls_cert.clone(),
            s.options.tls_key.clone(),
            shutdown.clone()
        ),
        serve(
            s.options.tunnel_listen,
            tunnel_app,
            s.options.tls_cert.clone(),
            s.options.tls_key.clone(),
            shutdown.clone()
        )
    );
    shutdown.cancel();
    if let Err(error) = &result {
        tracing::error!(error = %error, "服务端停止：监听服务异常");
    } else {
        tracing::info!("服务端已停止");
    }
    result?;
    Ok(())
}
async fn list(State(s): State<Arc<Server>>, h: HeaderMap) -> ApiResult<Json<Value>> {
    s.admin.check(&h)?;
    let ts = s.tunnels.lock().unwrap();
    let rs = s.runtimes.lock().unwrap();
    let sessions = s.sessions.lock().unwrap();
    let mut items:Vec<_>=ts.values().map(|t|{let st=rs.get(&t.id).map(|r|r.status.lock().unwrap().clone()).unwrap_or(Status{state:"已禁用".into(),..Default::default()});json!({"config":t,"status":st,"online":sessions.get(&t.id).is_some_and(|x|!x.cancel.is_cancelled()),"expired":t.expires_at<=now()})}).collect();
    items.sort_by_key(|v| v["config"]["name"].as_str().unwrap_or_default().to_owned());
    Ok(Json(
        json!({"items":items,"public_url":s.options.public_url,"port_min":s.options.port_min,"port_max":s.options.port_max}),
    ))
}
fn validate(s: &Server, v: &Input) -> ApiResult<()> {
    if v.name.trim().is_empty() || v.name.len() > 100 {
        return Err(bad("名称长度应为 1–100 字节"));
    }
    if v.port < s.options.port_min || v.port > s.options.port_max {
        return Err(bad(format!(
            "端口必须在 {}–{} 之间",
            s.options.port_min, s.options.port_max
        )));
    }
    if v.max_connections == 0 || v.max_connections > 10000 || v.ttl_days == 0 || v.ttl_days > 3650 {
        return Err(bad("连接数需为 1–10000，有效期需为 1–3650 天"));
    }
    Ok(())
}
async fn create(
    State(s): State<Arc<Server>>,
    h: HeaderMap,
    Json(v): Json<Input>,
) -> ApiResult<Json<Value>> {
    s.admin.check(&h)?;
    validate(&s, &v)?;
    let _guard = s.mutation.lock().await;
    if s.tunnels.lock().unwrap().values().any(|t| t.port == v.port) {
        return Err(bad("公网端口已被其他隧道配置占用"));
    }
    let t = Tunnel {
        id: uuid::Uuid::new_v4().to_string(),
        name: v.name,
        bind_ip: v.bind_ip,
        port: v.port,
        enabled: v.enabled,
        max_connections: v.max_connections,
        version: 1,
        expires_at: now() + v.ttl_days * 86400,
        jti: random(),
    };
    let listener = if t.enabled {
        Some(
            TcpListener::bind(SocketAddr::new(t.bind_ip, t.port))
                .await
                .map_err(|e| {
                    tracing::warn!(tunnel_id = %t.id, port = t.port, error = %e, "创建隧道时端口绑定失败");
                    bad(format!("端口绑定失败: {e}"))
                })?,
        )
    } else {
        None
    };
    let token = s.token(&t)?;
    s.admin.db.save(&t.id, &t, "create")?;
    s.tunnels.lock().unwrap().insert(t.id.clone(), t.clone());
    tracing::info!(tunnel_id = %t.id, port = t.port, enabled = t.enabled, "隧道已创建");
    if let Some(l) = listener {
        s.start(t.clone(), l);
    }
    Ok(Json(json!({"id":t.id,"token":token})))
}
async fn update(
    State(s): State<Arc<Server>>,
    Path(id): Path<String>,
    h: HeaderMap,
    Json(v): Json<Input>,
) -> ApiResult<Json<Value>> {
    s.admin.check(&h)?;
    validate(&s, &v)?;
    let _guard = s.mutation.lock().await;
    let old = s
        .tunnels
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| bad("隧道不存在"))?;
    if s.tunnels
        .lock()
        .unwrap()
        .values()
        .any(|t| t.id != id && t.port == v.port)
    {
        return Err(bad("公网端口已被其他隧道配置占用"));
    }
    // Pre-bind changed addresses so a failed update never removes the existing listener.
    let changed = old.port != v.port || old.bind_ip != v.bind_ip;
    let listener = if v.enabled && (changed || !old.enabled) {
        Some(
            TcpListener::bind(SocketAddr::new(v.bind_ip, v.port))
                .await
                .map_err(|e| {
                    tracing::warn!(tunnel_id = %id, port = v.port, error = %e, "更新隧道时端口绑定失败");
                    bad(format!("端口绑定失败: {e}"))
                })?,
        )
    } else {
        None
    };
    let mut t = old.clone();
    t.name = v.name;
    t.bind_ip = v.bind_ip;
    t.port = v.port;
    t.enabled = v.enabled;
    t.max_connections = v.max_connections;
    s.admin.db.save(&id, &t, "update")?;
    s.tunnels.lock().unwrap().insert(id.clone(), t.clone());
    s.stop(&id).await;
    tracing::info!(tunnel_id = %id, port = t.port, enabled = t.enabled, "隧道配置已更新");
    if old.enabled && !t.enabled {
        tracing::info!(tunnel_id = %id, "隧道已禁用，现有连接已关闭");
    }
    if t.enabled {
        let l = match listener {
            Some(l) => l,
            None => TcpListener::bind(SocketAddr::new(t.bind_ip, t.port))
                .await
                .map_err(|e| {
                    tracing::error!(tunnel_id = %id, error = %e, "隧道配置已保存，但监听失败");
                    bad(format!("配置已保存，但监听失败: {e}"))
                })?,
        };
        s.start(t, l);
    }
    Ok(Json(json!({"ok":true})))
}
async fn remove(
    State(s): State<Arc<Server>>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> ApiResult<Json<Value>> {
    s.admin.check(&h)?;
    let _guard = s.mutation.lock().await;
    s.admin.db.remove(&id)?;
    s.tunnels.lock().unwrap().remove(&id);
    s.stop(&id).await;
    tracing::info!(tunnel_id = %id, "隧道已删除");
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct Rotate {
    ttl_days: u64,
}
async fn rotate(
    State(s): State<Arc<Server>>,
    Path(id): Path<String>,
    h: HeaderMap,
    Json(v): Json<Rotate>,
) -> ApiResult<Json<Value>> {
    s.admin.check(&h)?;
    if v.ttl_days == 0 || v.ttl_days > 3650 {
        return Err(bad("有效期需为 1–3650 天"));
    }
    let _guard = s.mutation.lock().await;
    let mut t = s
        .tunnels
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or_else(|| bad("隧道不存在"))?;
    t.version += 1;
    t.jti = random();
    t.expires_at = now() + v.ttl_days * 86400;
    let token = s.token(&t)?;
    s.admin.db.save(&id, &t, "rotate")?;
    s.tunnels.lock().unwrap().insert(id.clone(), t);
    s.disconnect(&id);
    tracing::info!(tunnel_id = %id, ttl_days = v.ttl_days, "隧道凭证已刷新，旧会话已关闭");
    Ok(Json(json!({"token":token})))
}
async fn control(
    State(s): State<Arc<Server>>,
    h: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let c = s.verify(bearer(&h)?).inspect_err(|_| {
        tracing::warn!("客户端连接被拒绝：凭证无效、已过期或隧道已禁用");
    })?;
    Ok(ws
        .max_message_size(16384)
        .max_frame_size(16384)
        .on_upgrade(move |socket| control_session(s, c, socket)))
}
async fn control_session(s: Arc<Server>, c: Claims, socket: WebSocket) {
    let (tx, mut rx) = mpsc::channel(128);
    let cancel = s.shutdown.child_token();
    let sid = random();
    let port;
    {
        let _mutation = s.mutation.lock().await;
        let tunnels = s.tunnels.lock().unwrap();
        let Some(t) = tunnels.get(&c.sub) else {
            return;
        };
        if !t.enabled || t.version != c.version || t.jti != c.jti || c.exp <= now() {
            tracing::warn!(tunnel_id = %c.sub, "客户端连接被拒绝：凭证状态已变化");
            return;
        }
        port = t.port;
        let mut sessions = s.sessions.lock().unwrap();
        if sessions.contains_key(&c.sub) {
            tracing::warn!(tunnel_id = %c.sub, "客户端连接被拒绝：隧道已有在线客户端");
            return;
        }
        sessions.insert(
            c.sub.clone(),
            Session {
                id: sid.clone(),
                tx,
                cancel: cancel.clone(),
            },
        );
    }
    tracing::info!(tunnel_id = %c.sub, public_port = port, "客户端控制连接已上线");
    let (mut sink, mut source) = socket.split();
    let io = async {
        sink.send(Message::Text(
            serde_json::to_string(&Control::Ready {
                protocol: 1,
                public_port: port,
            })?
            .into(),
        ))
        .await?;
        let mut beat = tokio::time::interval(Duration::from_secs(10));
        let mut seen = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _=beat.tick()=>{if seen.elapsed()>Duration::from_secs(35){bail!("客户端心跳超时");}sink.send(Message::Text(serde_json::to_string(&Control::Ping)?.into())).await?;},
                message=rx.recv()=>{let Some(message)=message else{break;};sink.send(Message::Text(serde_json::to_string(&message)?.into())).await?;},
                incoming=source.next()=>{match incoming{Some(Ok(Message::Text(t)))=>match serde_json::from_str::<Control>(&t)?{
                    Control::Pong=>{seen=tokio::time::Instant::now();if let Some(r)=s.runtimes.lock().unwrap().get(&c.sub){r.status.lock().unwrap().last_seen=Some(now());}},
                    Control::Failed{ticket}=>{let mut p=s.pending.lock().unwrap();if p.get(&ticket).is_some_and(|p|p.session_id==sid){p.remove(&ticket);}},
                    _=>bail!("控制消息无效"),
                },Some(Ok(Message::Ping(_)|Message::Pong(_)))=>{},_=>break}}
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! {_=cancel.cancelled()=>{},_=tokio::time::sleep(Duration::from_secs(c.exp.saturating_sub(now())))=>{tracing::warn!(tunnel_id=%c.sub,"凭证已过期，正在关闭客户端会话");},r=io=>{if let Err(e)=r{tracing::warn!(tunnel_id=%c.sub,error=%e,"客户端控制连接异常");}}}
    cancel.cancel();
    let mut sessions = s.sessions.lock().unwrap();
    if sessions.get(&c.sub).is_some_and(|x| x.id == sid) {
        sessions.remove(&c.sub);
    }
    drop(sessions);
    s.pending.lock().unwrap().retain(|_, p| p.session_id != sid);
    tracing::info!(tunnel_id = %c.sub, "客户端控制连接已离线");
}
async fn data(
    State(s): State<Arc<Server>>,
    h: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let ticket = bearer(&h)?;
    let pending = s
        .pending
        .lock()
        .unwrap()
        .remove(ticket)
        .ok_or_else(unauthorized)?;
    if pending.expires <= now() || pending.cancel.is_cancelled() {
        return Err(unauthorized());
    }
    Ok(ws
        .max_message_size(65536)
        .max_frame_size(65536)
        .on_upgrade(move |socket| async move {
            if !pending.cancel.is_cancelled() {
                let _ = pending.tx.send(socket);
            }
        }))
}
