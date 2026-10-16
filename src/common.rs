use anyhow::{Result, bail};
use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;
use tower_http::set_header::SetResponseHeaderLayer;
use uuid::Uuid;

pub fn init() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    crate::logging::init();
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn random() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
pub type ApiResult<T> = std::result::Result<T, ApiError>;
pub struct ApiError(pub StatusCode, pub String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let must_change_password =
            self.0 == StatusCode::FORBIDDEN && self.1 == PASSWORD_CHANGE_REQUIRED;
        (
            self.0,
            Json(json!({"error": self.1,"must_change_password":must_change_password})),
        )
            .into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error=%e,"operation failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "操作失败，请检查程序日志".into(),
        )
    }
}
pub fn bad(s: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, s.into())
}
pub fn unauthorized() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "认证失败或凭证已失效".into())
}
pub fn bearer(h: &HeaderMap) -> ApiResult<&str> {
    h.get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)
}

pub struct Db(pub Mutex<Connection>);
impl Db {
    pub fn open(dir: &Path) -> Result<Arc<Self>> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let c = Connection::open(dir.join("state.sqlite"))?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS entries(id TEXT PRIMARY KEY,body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS audit(id INTEGER PRIMARY KEY,at INTEGER NOT NULL,action TEXT NOT NULL,target TEXT NOT NULL);
            PRAGMA user_version=1;")?;
        Ok(Arc::new(Self(Mutex::new(c))))
    }
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .0
            .lock()
            .unwrap()
            .query_row("SELECT value FROM settings WHERE key=?", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT OR REPLACE INTO settings VALUES(?,?)",
            params![key, value],
        )?;
        Ok(())
    }
    pub fn all<T: serde::de::DeserializeOwned>(&self) -> Result<Vec<T>> {
        let c = self.0.lock().unwrap();
        let mut q = c.prepare("SELECT body FROM entries ORDER BY rowid")?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn save<T: Serialize>(&self, id: &str, v: &T, action: &str) -> Result<()> {
        let body = serde_json::to_string(v)?;
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO entries VALUES(?,?)",
            params![id, body],
        )?;
        tx.execute(
            "INSERT INTO audit(at,action,target) VALUES(?,?,?)",
            params![now() as i64, action, id],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn remove(&self, id: &str) -> Result<()> {
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM entries WHERE id=?", [id])?;
        tx.execute(
            "INSERT INTO audit(at,action,target) VALUES(?,?,?)",
            params![now() as i64, "delete", id],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn audit(&self, action: &str, target: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT INTO audit(at,action,target) VALUES(?,?,?)",
            params![now() as i64, action, target],
        )?;
        Ok(())
    }
}

pub struct Admin {
    pub db: Arc<Db>,
    sessions: Mutex<HashMap<String, u64>>,
    login_attempts: Mutex<Vec<u64>>,
    password_attempts: Mutex<Vec<u64>>,
    auth_mutation: tokio::sync::Mutex<()>,
    must_change_password: AtomicBool,
    pub role: &'static str,
}
impl Admin {
    pub fn new(
        db: Arc<Db>,
        password: Option<String>,
        reset: bool,
        role: &'static str,
    ) -> Result<Arc<Self>> {
        if reset || db.setting("password")?.is_none() {
            let password = password.unwrap_or_else(|| "admin".into());
            if password != "admin" && password.chars().count() < 12 {
                bail!("管理密码至少需要 12 个字符");
            }
            let hash = Argon2::default()
                .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .to_string();
            store_password(
                &db,
                &hash,
                password == "admin",
                if reset {
                    "password_reset"
                } else {
                    "password_initialize"
                },
            )?;
        }
        let must_change_password = match db.setting("must_change_password")? {
            Some(value) => value == "1",
            None => {
                let hash = db.setting("password")?.unwrap_or_default();
                let is_default = verify_password(&hash, "admin");
                db.set("must_change_password", if is_default { "1" } else { "0" })?;
                is_default
            }
        };
        crate::logging::attach(db.clone(), role)?;
        Ok(Arc::new(Self {
            db,
            sessions: Mutex::new(HashMap::new()),
            login_attempts: Mutex::new(Vec::new()),
            password_attempts: Mutex::new(Vec::new()),
            auth_mutation: tokio::sync::Mutex::new(()),
            must_change_password: AtomicBool::new(must_change_password),
            role,
        }))
    }
    fn check_session(&self, h: &HeaderMap) -> ApiResult<()> {
        let key = bearer(h)?;
        let mut s = self.sessions.lock().unwrap();
        s.retain(|_, exp| *exp > now());
        if s.contains_key(key) {
            Ok(())
        } else {
            Err(unauthorized())
        }
    }
    pub fn check(&self, h: &HeaderMap) -> ApiResult<()> {
        self.check_session(h)?;
        if self.must_change_password.load(Ordering::Acquire) {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                PASSWORD_CHANGE_REQUIRED.into(),
            ));
        }
        Ok(())
    }
}
const PASSWORD_CHANGE_REQUIRED: &str = "请先修改默认管理密码";
fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash).ok().is_some_and(|h| {
        Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok()
    })
}
fn store_password(db: &Db, hash: &str, must_change: bool, action: &str) -> Result<()> {
    let mut connection = db.0.lock().unwrap();
    let tx = connection.transaction()?;
    tx.execute(
        "INSERT OR REPLACE INTO settings VALUES('password',?)",
        [hash],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO settings VALUES('must_change_password',?)",
        [if must_change { "1" } else { "0" }],
    )?;
    tx.execute(
        "INSERT INTO audit(at,action,target) VALUES(?,?,?)",
        params![now() as i64, action, "admin"],
    )?;
    tx.commit()?;
    Ok(())
}
fn limit_attempts(attempts: &Mutex<Vec<u64>>) -> ApiResult<()> {
    let mut attempts = attempts.lock().unwrap();
    attempts.retain(|t| now().saturating_sub(*t) < 60);
    if attempts.len() >= 10 {
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "尝试次数过多，请一分钟后重试".into(),
        ));
    }
    attempts.push(now());
    Ok(())
}
#[derive(Deserialize)]
struct Login {
    password: String,
}
async fn login(State(a): State<Arc<Admin>>, Json(v): Json<Login>) -> ApiResult<Json<Value>> {
    limit_attempts(&a.login_attempts)?;
    let _guard = a.auth_mutation.lock().await;
    let hash = a.db.setting("password")?.unwrap_or_default();
    let valid = tokio::task::spawn_blocking(move || verify_password(&hash, &v.password))
        .await
        .map_err(anyhow::Error::from)?;
    if !valid {
        tracing::warn!(role = a.role, "管理员登录失败");
        return Err(unauthorized());
    }
    let token = random();
    a.sessions
        .lock()
        .unwrap()
        .insert(token.clone(), now() + 8 * 3600);
    a.db.audit("login", "admin")?;
    tracing::info!(role = a.role, "管理员登录成功");
    Ok(Json(
        json!({"token":token,"must_change_password":a.must_change_password.load(Ordering::Acquire)}),
    ))
}
async fn logout(State(a): State<Arc<Admin>>, h: HeaderMap) -> ApiResult<Json<Value>> {
    a.check_session(&h)?;
    a.sessions.lock().unwrap().remove(bearer(&h)?);
    a.db.audit("logout", "admin")?;
    tracing::info!(role = a.role, "管理员退出登录");
    Ok(Json(json!({"ok":true})))
}
async fn account(State(a): State<Arc<Admin>>, h: HeaderMap) -> ApiResult<Json<Value>> {
    a.check_session(&h)?;
    Ok(Json(
        json!({"must_change_password":a.must_change_password.load(Ordering::Acquire)}),
    ))
}
#[derive(Deserialize)]
struct PasswordChange {
    current_password: String,
    new_password: String,
    confirm_password: String,
}
async fn change_password(
    State(a): State<Arc<Admin>>,
    h: HeaderMap,
    Json(v): Json<PasswordChange>,
) -> ApiResult<Json<Value>> {
    let _guard = a.auth_mutation.lock().await;
    a.check_session(&h)?;
    limit_attempts(&a.password_attempts)?;
    if v.new_password.chars().count() < 12 {
        return Err(bad("新密码至少需要 12 个字符"));
    }
    if v.new_password != v.confirm_password {
        return Err(bad("两次输入的新密码不一致"));
    }
    if v.current_password == v.new_password {
        return Err(bad("新密码不能与当前密码相同"));
    }
    let hash = a.db.setting("password")?.unwrap_or_default();
    let next_hash = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        if !verify_password(&hash, &v.current_password) {
            return Ok(None);
        }
        Ok(Some(
            Argon2::default()
                .hash_password(v.new_password.as_bytes(), &SaltString::generate(&mut OsRng))
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .to_string(),
        ))
    })
    .await
    .map_err(anyhow::Error::from)??;
    let Some(next_hash) = next_hash else {
        tracing::warn!(role = a.role, "修改管理密码失败：当前密码错误");
        return Err(bad("当前密码不正确"));
    };
    store_password(&a.db, &next_hash, false, "change_password")?;
    a.must_change_password.store(false, Ordering::Release);
    a.sessions.lock().unwrap().clear();
    tracing::info!(role = a.role, "管理密码已修改，所有管理会话已失效");
    Ok(Json(json!({"ok":true,"reauthenticate":true})))
}
async fn audit(State(a): State<Arc<Admin>>, h: HeaderMap) -> ApiResult<Json<Value>> {
    a.check(&h)?;
    let c = a.db.0.lock().unwrap();
    let mut q = c
        .prepare("SELECT at,action,target FROM audit ORDER BY id DESC LIMIT 100")
        .map_err(anyhow::Error::from)?;
    let rows=q.query_map([],|r|Ok(json!({"at":r.get::<_,i64>(0)?,"action":r.get::<_,String>(1)?,"target":r.get::<_,String>(2)?}))).map_err(anyhow::Error::from)?;
    Ok(Json(json!(
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(anyhow::Error::from)?
    )))
}
async fn meta(State(a): State<Arc<Admin>>) -> Json<Value> {
    Json(json!({"role":a.role,"version":env!("CARGO_PKG_VERSION")}))
}
pub fn admin_routes(a: Arc<Admin>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../ui/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../ui/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../ui/style.css"),
                )
            }),
        )
        .route("/api/meta", get(meta))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/account", get(account))
        .route("/api/password", post(change_password))
        .route("/api/audit", get(audit))
        .route("/api/logs", get(crate::logging::list))
        .route("/api/logs/export", get(crate::logging::export))
        .with_state(a)
}
pub fn secure(app: Router) -> Router {
    app.layer(DefaultBodyLimit::max(32*1024))
        .layer(SetResponseHeaderLayer::overriding(header::CACHE_CONTROL,header::HeaderValue::from_static("no-store")))
        .layer(SetResponseHeaderLayer::overriding(header::X_CONTENT_TYPE_OPTIONS,header::HeaderValue::from_static("nosniff")))
        .layer(SetResponseHeaderLayer::overriding(header::CONTENT_SECURITY_POLICY,header::HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'")))
}
pub async fn serve(
    addr: SocketAddr,
    app: Router,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    cancel: CancellationToken,
) -> Result<()> {
    match (cert, key) {
        (Some(cert), Some(key)) => {
            let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?;
            let handle = axum_server::Handle::new();
            let h = handle.clone();
            tokio::spawn(async move {
                cancel.cancelled().await;
                h.graceful_shutdown(Some(std::time::Duration::from_secs(3)));
            });
            axum_server::bind_rustls(addr, config)
                .handle(handle)
                .serve(secure(app).into_make_service())
                .await?;
        }
        (None, None) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, secure(app))
                .with_graceful_shutdown(cancel.cancelled_owned())
                .await?;
        }
        _ => bail!("证书与私钥必须同时提供"),
    };
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub jti: String,
    pub iat: u64,
    pub nbf: u64,
    pub exp: u64,
    pub endpoint: String,
    pub version: u64,
}
// Decoding is only for discovery/display. Authentication happens at the server.
pub fn unverified_claims(token: &str) -> Result<Claims> {
    use base64::Engine;
    if token.len() > 8192 {
        bail!("凭证过长");
    }
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        bail!("JWT 格式无效");
    }
    Ok(serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1])?,
    )?)
}
pub fn discover(token: &str, allow_insecure: bool) -> Result<Claims> {
    let c = unverified_claims(token)?;
    let u = url::Url::parse(&c.endpoint)?;
    if !(u.scheme() == "wss" || allow_insecure && u.scheme() == "ws")
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || u.path() != "/"
    {
        bail!("服务端地址无效，必须是 wss://主机:端口（本地调试可显式允许 ws）");
    }
    if c.aud != "my-frp-client" || c.exp <= now() {
        bail!("凭证无效或已过期");
    }
    Ok(c)
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    Ready { protocol: u8, public_port: u16 },
    Ping,
    Pong,
    Open { ticket: String },
    Failed { ticket: String },
}

#[derive(Default, Clone, Serialize)]
pub struct Status {
    pub state: String,
    pub error: Option<String>,
    pub local: String,
    pub public_port: Option<u16>,
    pub active: u64,
    pub sent: u64,
    pub received: u64,
    pub last_seen: Option<u64>,
}
