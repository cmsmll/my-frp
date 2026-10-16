use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU16, Ordering},
    time::Duration,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[path = "../examples/stream_service.rs"]
mod stream_service;

const PASSWORD: &str = "integration-test-password-2026";
static NEXT_PORT: AtomicU16 = AtomicU16::new(20000);
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn port() -> u16 {
    loop {
        let candidate = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        assert!(candidate <= 40000, "integration test port range exhausted");
        if std::net::TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            return candidate;
        }
    }
}
fn launch(binary: &str, args: &[String]) -> Process {
    launch_in_dir(binary, args, None)
}
fn launch_in_dir(binary: &str, args: &[String], dir: Option<&Path>) -> Process {
    launch_with_password(binary, args, dir, Some(PASSWORD))
}
fn launch_with_password(
    binary: &str,
    args: &[String],
    dir: Option<&Path>,
    password: Option<&str>,
) -> Process {
    let mut cmd = Command::new(binary);
    cmd.args(args)
        .env("RUST_LOG", "my_frp=info")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(password) = password {
        cmd.env("MYFRP_ADMIN_PASSWORD", password);
    } else {
        cmd.env_remove("MYFRP_ADMIN_PASSWORD");
    }
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    Process(cmd.spawn().unwrap())
}
struct Api {
    http: reqwest::Client,
    url: String,
    token: String,
    must_change_password: bool,
}
impl Api {
    async fn login(url: String, cert: Option<&str>) -> Self {
        Self::login_password(url, cert, PASSWORD).await
    }
    async fn login_password(url: String, cert: Option<&str>, password: &str) -> Self {
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5));
        if let Some(cert) = cert {
            builder = builder
                .add_root_certificate(reqwest::Certificate::from_pem(cert.as_bytes()).unwrap());
        }
        let http = builder.build().unwrap();
        let mut last_error = String::new();
        let ready = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match http
                    .get(format!("{url}/api/meta"))
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                {
                    Ok(response) if response.status().is_success() => return,
                    Ok(response) => last_error = format!("HTTP {}", response.status()),
                    Err(error) => last_error = format!("{error:?}"),
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        assert!(
            ready.is_ok(),
            "server unavailable during startup: {url}: {last_error}"
        );
        let r = http
            .post(format!("{url}/api/login"))
            .json(&json!({"password":password}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        Self {
            http,
            url,
            token: body["token"].as_str().unwrap().into(),
            must_change_password: body["must_change_password"].as_bool().unwrap(),
        }
    }
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let mut r = self
            .http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(&self.token);
        if let Some(v) = body {
            r = r.json(&v);
        }
        let r = r.send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "{status}: {text}");
        serde_json::from_str(&text).unwrap()
    }
    async fn get(&self) -> Value {
        self.call(reqwest::Method::GET, "/api/tunnels", None).await
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
    async fn put(&self, id: &str, body: Value) -> Value {
        self.call(
            reqwest::Method::PUT,
            &format!("/api/tunnels/{id}"),
            Some(body),
        )
        .await
    }
    async fn wait_online(&self, id: &str) {
        for _ in 0..120 {
            let v = self.get().await;
            if v["items"].as_array().unwrap().iter().any(|e| {
                e["config"]["id"] == id && (e["online"] == true || e["status"]["state"] == "在线")
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("not online: {}", self.get().await);
    }
}
fn server_args(dir: &TempDir, admin: u16, transport: u16) -> Vec<String> {
    let path = dir.path().join("server-config.json");
    let config = json!({
        "data_dir": ".",
        "admin_listen": format!("127.0.0.1:{admin}"),
        "tunnel_listen": format!("127.0.0.1:{transport}"),
        "public_url": format!("ws://127.0.0.1:{transport}"),
        "admin_password": PASSWORD,
        "allow_insecure": true,
        "port_min": 1024,
        "port_max": 65535
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    vec![path.to_str().unwrap().into()]
}
fn client_args(dir: &TempDir, admin: u16) -> Vec<String> {
    vec![
        "--data-dir".into(),
        dir.path().to_str().unwrap().into(),
        "--admin-listen".into(),
        format!("127.0.0.1:{admin}"),
        "--allow-insecure".into(),
    ]
}
fn tunnel(p: u16) -> Value {
    json!({"name":"integration tunnel","bind_ip":"127.0.0.1","port":p,"max_connections":10,"ttl_days":1,"enabled":true})
}
fn entry(token: &str, local: u16) -> Value {
    json!({"name":"local echo","local_addr":format!("127.0.0.1:{local}"),"token":token,"enabled":true})
}
async fn echo() -> (u16, tokio::task::JoinHandle<()>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = l.local_addr().unwrap().port();
    let h = tokio::spawn(async move {
        loop {
            let (tcp, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut r, mut w) = tcp.into_split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
                let _ = w.shutdown().await;
            });
        }
    });
    (p, h)
}
async fn roundtrip(port: u16, size: usize) {
    tokio::time::timeout(Duration::from_secs(20), async {
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (mut r, mut w) = tcp.into_split();
        let data: Vec<u8> = (0..size).map(|n| (n % 251) as u8).collect();
        let expected = data.clone();
        let sending = async move {
            w.write_all(&data).await.unwrap();
            w.shutdown().await.unwrap();
        };
        let receiving = async move {
            let mut got = Vec::new();
            r.read_to_end(&mut got).await.unwrap();
            assert_eq!(got, expected);
        };
        tokio::join!(sending, receiving);
    })
    .await
    .expect("TCP roundtrip timed out");
}
async fn wait_closed(tcp: &mut TcpStream) {
    let mut b = [0; 1];
    let r = tokio::time::timeout(Duration::from_secs(5), tcp.read(&mut b))
        .await
        .expect("connection was not terminated");
    assert!(matches!(r, Ok(0) | Err(_)));
}

struct LocalStreamService(tokio::task::JoinHandle<()>);
impl Drop for LocalStreamService {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn local_stream_service() -> (u16, LocalStreamService) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        axum::serve(listener, stream_service::app()).await.unwrap();
    });
    (local, LocalStreamService(task))
}

async fn websocket_stream_probe(public: u16, label: &str, binary_size: usize) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{
        Message,
        protocol::{CloseFrame, frame::coding::CloseCode},
    };

    tokio::time::timeout(Duration::from_secs(15), async {
        let (mut ws, response) =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{public}/ws"))
                .await
                .unwrap();
        assert_eq!(response.status(), 101, "business WebSocket upgrade failed");
        assert_eq!(response.headers()["upgrade"], "websocket");

        let text = format!("{label}: 中文穿透 / café / 🛰️\nsecond line");
        ws.send(Message::Text(text.clone().into())).await.unwrap();
        assert_eq!(
            ws.next().await.unwrap().unwrap(),
            Message::Text(text.into()),
            "text must remain on its own WebSocket connection"
        );

        // This business frame crosses many 32 KiB chunks in the tunnel bridge.
        let salt = label.bytes().fold(0u8, u8::wrapping_add);
        let data: Vec<u8> = (0..binary_size).map(|n| (n % 251) as u8 ^ salt).collect();
        ws.send(Message::Binary(data.clone().into())).await.unwrap();
        assert_eq!(
            ws.next().await.unwrap().unwrap(),
            Message::Binary(data.into()),
            "large binary frame was truncated, corrupted, or crossed connections"
        );

        let ping = format!("business-ping-{label}").into_bytes();
        ws.send(Message::Ping(ping.clone().into())).await.unwrap();
        assert_eq!(
            ws.next().await.unwrap().unwrap(),
            Message::Pong(ping.into()),
            "business ping/pong must pass through the tunnel"
        );

        // Ensure a ping did not consume or corrupt the next application message.
        let after_ping = format!("after-ping-{label}");
        ws.send(Message::Text(after_ping.clone().into()))
            .await
            .unwrap();
        assert_eq!(
            ws.next().await.unwrap().unwrap(),
            Message::Text(after_ping.into())
        );
        let close = CloseFrame {
            code: CloseCode::Normal,
            reason: format!("done-{label}").into(),
        };
        ws.send(Message::Close(Some(close.clone()))).await.unwrap();
        assert_eq!(
            ws.next().await.unwrap().unwrap(),
            Message::Close(Some(close)),
            "normal WebSocket close handshake was lost"
        );
    })
    .await
    .expect("business WebSocket probe timed out");
}

struct SseEvent {
    id: u64,
    kind: String,
    data: Value,
}

fn drain_sse_events(pending: &mut Vec<u8>) -> Vec<SseEvent> {
    let mut events = Vec::new();
    loop {
        let boundary = pending
            .windows(2)
            .position(|bytes| bytes == b"\n\n")
            .map(|end| (end, 2))
            .or_else(|| {
                pending
                    .windows(4)
                    .position(|bytes| bytes == b"\r\n\r\n")
                    .map(|end| (end, 4))
            });
        let Some((end, delimiter)) = boundary else {
            break;
        };
        let block = String::from_utf8(pending.drain(..end + delimiter).collect()).unwrap();
        let mut id = None;
        let mut kind = String::new();
        let mut data = Vec::new();
        for line in block.lines() {
            if line.starts_with(':') || line.is_empty() {
                continue;
            }
            if let Some((field, value)) = line.split_once(':') {
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "id" => id = Some(value.parse().unwrap()),
                    "event" => kind = value.into(),
                    "data" => data.push(value),
                    _ => {}
                }
            }
        }
        if !data.is_empty() {
            events.push(SseEvent {
                id: id.expect("SSE event is missing its resumable ID"),
                kind,
                data: serde_json::from_str(&data.join("\n")).unwrap(),
            });
        }
    }
    events
}

async fn sse_stream_probe(public: u16, tag: &str, last_id: Option<u64>) {
    const COUNT: u64 = 5;
    const INTERVAL_MS: u64 = 350;
    tokio::time::timeout(Duration::from_secs(12), async {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let mut request = http.get(format!("http://127.0.0.1:{public}/sse")).query(&[
            ("count", COUNT.to_string()),
            ("interval_ms", INTERVAL_MS.to_string()),
            ("tag", tag.to_owned()),
        ]);
        if let Some(last_id) = last_id {
            request = request.header("Last-Event-ID", last_id.to_string());
        }
        let mut response = request.send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/event-stream")
        );
        let mut pending = Vec::new();
        let mut received = Vec::new();
        let mut arrival_times = Vec::new();
        while let Some(bytes) = response.chunk().await.unwrap() {
            pending.extend_from_slice(&bytes);
            for event in drain_sse_events(&mut pending) {
                assert!(received.len() < COUNT as usize, "SSE stream exceeded count");
                let expected_id = last_id.unwrap_or(0) + received.len() as u64 + 1;
                assert_eq!(event.id, expected_id);
                assert_eq!(event.kind, "tick");
                assert_eq!(event.data["seq"], expected_id);
                assert_eq!(event.data["tag"], tag, "concurrent SSE streams mixed data");
                arrival_times.push(std::time::Instant::now());
                received.push(event);
            }
        }
        assert_eq!(
            received.len(),
            COUNT as usize,
            "finite SSE stream was truncated"
        );
        assert!(pending.is_empty(), "SSE stream ended inside an event");
        // A proxy which buffers the complete response yields all events together.
        // Allow scheduling jitter while requiring observable progressive delivery.
        let spread = arrival_times
            .last()
            .unwrap()
            .duration_since(arrival_times[0]);
        assert!(
            spread >= Duration::from_millis(INTERVAL_MS * (COUNT - 1) / 2),
            "SSE events were buffered rather than streamed: spread={spread:?}"
        );
    })
    .await
    .expect("finite SSE streaming probe timed out");
}

async fn websocket_and_sse_transport(tls: bool) {
    let sd = TempDir::new().unwrap();
    let cd = TempDir::new().unwrap();
    let (sa, st, ca, public) = (port(), port(), port(), port());
    let (local, _local_service) = local_stream_service().await;
    let args = server_args(&sd, sa, st);
    let mut client = client_args(&cd, ca);
    let cert = if tls {
        let certified =
            rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])
                .unwrap();
        let cert = certified.cert.pem();
        let cert_path = sd.path().join("cert.pem");
        std::fs::write(&cert_path, &cert).unwrap();
        std::fs::write(
            sd.path().join("tls-key.pem"),
            certified.signing_key.serialize_pem(),
        )
        .unwrap();
        let mut config: Value = serde_json::from_slice(&std::fs::read(&args[0]).unwrap()).unwrap();
        config["public_url"] = json!(format!("wss://localhost:{st}"));
        config["allow_insecure"] = json!(false);
        config["tls_cert"] = json!("cert.pem");
        config["tls_key"] = json!("tls-key.pem");
        std::fs::write(&args[0], serde_json::to_vec_pretty(&config).unwrap()).unwrap();
        client.retain(|arg| arg != "--allow-insecure");
        client.extend(["--ca-cert".into(), cert_path.to_str().unwrap().into()]);
        Some(cert)
    } else {
        None
    };
    let _server = launch(env!("CARGO_BIN_EXE_myfrps"), &args);
    let _client = launch(env!("CARGO_BIN_EXE_myfrpc"), &client);
    let admin_url = if tls {
        format!("https://localhost:{sa}")
    } else {
        format!("http://127.0.0.1:{sa}")
    };
    let s = Api::login(admin_url, cert.as_deref()).await;
    let c = Api::login(format!("http://127.0.0.1:{ca}"), None).await;
    let t = s.post("/api/tunnels", tunnel(public)).await;
    c.post("/api/tunnels", entry(t["token"].as_str().unwrap(), local))
        .await;
    s.wait_online(t["id"].as_str().unwrap()).await;

    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let health = http
        .get(format!("http://127.0.0.1:{public}/health"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    assert!(health.json::<Value>().await.unwrap().is_object());

    // All four streams share a public tunnel while retaining independent bytes,
    // event IDs, and lifecycles. The first binary frame is over 1 MiB.
    tokio::join!(
        websocket_stream_probe(public, "websocket-A", 1024 * 1024 + 123),
        websocket_stream_probe(public, "websocket-B", 32768 * 3 + 17),
        sse_stream_probe(public, "SSE-甲-🛰️", None),
        sse_stream_probe(public, "SSE-乙-resume", Some(41)),
    );
    // Closing WebSockets and completing SSE responses must leave the tunnel usable.
    websocket_stream_probe(public, "after-concurrent-streams", 32769).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_and_sse_over_verified_wss_transport() {
    websocket_and_sse_transport(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_and_sse_over_local_ws_transport() {
    websocket_and_sse_transport(false).await;
}

#[test]
fn server_rejects_missing_malformed_config_and_cli_flags() {
    let dir = TempDir::new().unwrap();
    let args = server_args(&dir, port(), port());
    let malformed = dir.path().join("malformed.json");
    std::fs::write(&malformed, "{\"admin_listen\":").unwrap();
    let rejected_args = [
        vec![dir.path().join("missing.json").to_str().unwrap().into()],
        vec![malformed.to_str().unwrap().into()],
        vec![args[0].clone(), "--allow-insecure".into()],
    ];
    for args in rejected_args {
        let mut server = launch(env!("CARGO_BIN_EXE_myfrps"), &args);
        let mut exited = false;
        for _ in 0..100 {
            if let Some(status) = server.0.try_wait().unwrap() {
                assert!(
                    !status.success(),
                    "invalid startup arguments succeeded: {args:?}"
                );
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            exited,
            "invalid startup arguments started a server: {args:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_loads_default_json_from_working_directory() {
    let dir = TempDir::new().unwrap();
    let (admin, transport) = (port(), port());
    let args = server_args(&dir, admin, transport);
    std::fs::rename(&args[0], dir.path().join("server.json")).unwrap();
    let _server = launch_in_dir(env!("CARGO_BIN_EXE_myfrps"), &[], Some(dir.path()));
    let api = Api::login(format!("http://127.0.0.1:{admin}"), None).await;
    assert!(api.get().await["items"].as_array().unwrap().is_empty());
    assert!(dir.path().join("signing.pem").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_password_change_sessions_and_persistent_filtered_logs() {
    const NEW_PASSWORD: &str = "new-integration-password-2026";
    for is_server in [true, false] {
        let dir = TempDir::new().unwrap();
        let admin = port();
        let (binary, args) = if is_server {
            let args = server_args(&dir, admin, port());
            let mut config: Value =
                serde_json::from_slice(&std::fs::read(&args[0]).unwrap()).unwrap();
            config.as_object_mut().unwrap().remove("admin_password");
            std::fs::write(&args[0], serde_json::to_vec_pretty(&config).unwrap()).unwrap();
            (env!("CARGO_BIN_EXE_myfrps"), args)
        } else {
            (env!("CARGO_BIN_EXE_myfrpc"), client_args(&dir, admin))
        };
        let mut process = launch_with_password(binary, &args, None, None);
        let url = format!("http://127.0.0.1:{admin}");
        let first = Api::login_password(url.clone(), None, "admin").await;
        let other = Api::login_password(url.clone(), None, "admin").await;
        assert!(first.must_change_password);
        assert_eq!(
            first.call(reqwest::Method::GET, "/api/account", None).await["must_change_password"],
            true
        );
        for path in [
            "/api/tunnels",
            "/api/audit",
            "/api/logs",
            "/api/logs/export",
        ] {
            let response = first
                .http
                .get(format!("{url}{path}"))
                .bearer_auth(&first.token)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 403, "default password allowed {path}");
            assert_eq!(
                response.json::<Value>().await.unwrap()["must_change_password"],
                true
            );
        }
        for body in [
            json!({"current_password":"incorrect-old-password","new_password":NEW_PASSWORD,"confirm_password":NEW_PASSWORD}),
            json!({"current_password":"admin","new_password":NEW_PASSWORD,"confirm_password":"mismatching-password"}),
            json!({"current_password":"admin","new_password":"short","confirm_password":"short"}),
        ] {
            let response = first
                .http
                .post(format!("{url}/api/password"))
                .bearer_auth(&first.token)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert!(!response.status().is_success(), "invalid password accepted");
        }
        let changed = first
            .post(
                "/api/password",
                json!({"current_password":"admin","new_password":NEW_PASSWORD,"confirm_password":NEW_PASSWORD}),
            )
            .await;
        assert_eq!(changed["reauthenticate"], true);
        for stale in [&first, &other] {
            assert_eq!(
                stale
                    .http
                    .get(format!("{url}/api/account"))
                    .bearer_auth(&stale.token)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                401,
                "password change kept an existing session alive"
            );
        }
        assert_eq!(
            first
                .http
                .post(format!("{url}/api/login"))
                .json(&json!({"password":"admin"}))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let api = Api::login_password(url.clone(), None, NEW_PASSWORD).await;
        assert!(!api.must_change_password);
        assert_eq!(
            api.call(reqwest::Method::GET, "/api/account", None).await["must_change_password"],
            false
        );
        assert!(api.get().await["items"].as_array().unwrap().is_empty());
        for path in ["/api/logs", "/api/logs/export"] {
            assert_eq!(
                api.http
                    .get(format!("{url}{path}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                401,
                "logs available without authentication"
            );
        }
        let logs = api.call(reqwest::Method::GET, "/api/logs", None).await;
        let items = logs["items"].as_array().unwrap();
        assert!(!items.is_empty(), "startup did not produce runtime logs");
        for item in items {
            assert!(item["id"].is_number());
            assert!(item["at"].is_number());
            assert!(item["component"].is_string());
            assert!(item["tunnel_id"].is_null() || item["tunnel_id"].is_string());
        }
        let persisted = items[0].clone();
        let level = persisted["level"].as_str().unwrap();
        let message = persisted["message"].as_str().unwrap();
        let filtered: Value = api
            .http
            .get(format!("{url}/api/logs"))
            .bearer_auth(&api.token)
            .query(&[("level", level), ("q", message), ("limit", "2")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let filtered = filtered["items"].as_array().unwrap();
        assert!(!filtered.is_empty());
        assert!(filtered.len() <= 2);
        assert!(
            filtered.iter().all(|item| item["level"] == level
                && item["message"].as_str().unwrap().contains(message))
        );
        let no_match: Value = api
            .http
            .get(format!("{url}/api/logs"))
            .bearer_auth(&api.token)
            .query(&[("q", "never-match-log-query-17592316")])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(no_match["items"].as_array().unwrap().is_empty());
        let export = api
            .http
            .get(format!("{url}/api/logs/export"))
            .bearer_auth(&api.token)
            .query(&[("level", level), ("q", message)])
            .send()
            .await
            .unwrap();
        assert!(export.status().is_success());
        assert!(export.headers().contains_key("content-disposition"));
        let text = export.text().await.unwrap();
        assert!(text.contains(message));
        let logs_text = logs.to_string();
        for secret in [
            NEW_PASSWORD,
            "incorrect-old-password",
            "mismatching-password",
            first.token.as_str(),
            other.token.as_str(),
            api.token.as_str(),
        ] {
            assert!(!logs_text.contains(secret), "secret included in logs");
            assert!(!text.contains(secret), "secret included in log export");
        }
        process.0.kill().unwrap();
        process.0.wait().unwrap();
        process = launch_with_password(binary, &args, None, None);
        let restarted = Api::login_password(url, None, NEW_PASSWORD).await;
        assert_eq!(
            restarted
                .call(reqwest::Method::GET, "/api/account", None)
                .await["must_change_password"],
            false,
            "restart reset the stored password"
        );
        let logs = restarted
            .call(reqwest::Method::GET, "/api/logs", None)
            .await;
        assert!(
            logs["items"].as_array().unwrap().iter().any(
                |item| item["id"] == persisted["id"] && item["message"] == persisted["message"]
            ),
            "restart lost persistent logs"
        );
        drop(process);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_multi_server_rotation_and_persistence() {
    let sd = TempDir::new().unwrap();
    let sd2 = TempDir::new().unwrap();
    let cd = TempDir::new().unwrap();
    let (sa, st, sa2, st2, ca, p1, p2) = (port(), port(), port(), port(), port(), port(), port());
    let sargs = server_args(&sd, sa, st);
    let mut server = launch(env!("CARGO_BIN_EXE_myfrps"), &sargs);
    let _server2 = launch(env!("CARGO_BIN_EXE_myfrps"), &server_args(&sd2, sa2, st2));
    let cargs = client_args(&cd, ca);
    let mut client = launch(env!("CARGO_BIN_EXE_myfrpc"), &cargs);
    let s = Api::login(format!("http://127.0.0.1:{sa}"), None).await;
    let s2 = Api::login(format!("http://127.0.0.1:{sa2}"), None).await;
    let c = Api::login(format!("http://127.0.0.1:{ca}"), None).await;
    assert_eq!(
        s.http
            .get(format!("{}/api/tunnels", s.url))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let page = s.http.get(&s.url).send().await.unwrap();
    assert!(page.headers().contains_key("content-security-policy"));
    assert!(page.text().await.unwrap().contains("隧道管理"));
    let (local, echo_task) = echo().await;
    let t1 = s.post("/api/tunnels", tunnel(p1)).await;
    let id = t1["id"].as_str().unwrap();
    let token = t1["token"].as_str().unwrap();
    let e1 = c.post("/api/tunnels", entry(token, local)).await;
    let cid = e1["id"].as_str().unwrap();
    s.wait_online(id).await;
    roundtrip(p1, 4 * 1024 * 1024).await;
    for api in [&s, &c] {
        let logs = api.call(reqwest::Method::GET, "/api/logs", None).await;
        assert!(
            !logs.to_string().contains(token),
            "runtime logs exposed JWT"
        );
        let export = api
            .http
            .get(format!("{}/api/logs/export", api.url))
            .bearer_auth(&api.token)
            .send()
            .await
            .unwrap();
        assert!(export.status().is_success());
        assert!(
            !export.text().await.unwrap().contains(token),
            "log export exposed JWT"
        );
    }
    for (api, tunnel_id) in [(&s, id), (&c, cid)] {
        let mut matched = false;
        for _ in 0..20 {
            let filtered: Value = api
                .http
                .get(format!("{}/api/logs", api.url))
                .bearer_auth(&api.token)
                .query(&[("tunnel_id", tunnel_id)])
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let items = filtered["items"].as_array().unwrap();
            assert!(items.iter().all(|item| item["tunnel_id"] == tunnel_id));
            if !items.is_empty() {
                matched = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(matched, "tunnel runtime events were not recorded");
    }
    assert!(
        !c.get().await.to_string().contains(token),
        "list must not expose credentials"
    );
    let t2 = s2.post("/api/tunnels", tunnel(p2)).await;
    c.post("/api/tunnels", entry(t2["token"].as_str().unwrap(), local))
        .await;
    s2.wait_online(t2["id"].as_str().unwrap()).await;
    tokio::join!(roundtrip(p1, 32769), roundtrip(p2, 65537));
    // A failed port change leaves the original tunnel running.
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let response = s
        .http
        .put(format!("{}/api/tunnels/{id}", s.url))
        .bearer_auth(&s.token)
        .json(&tunnel(occupied.local_addr().unwrap().port()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    roundtrip(p1, 100).await;
    let mut active = TcpStream::connect(("127.0.0.1", p1)).await.unwrap();
    active.write_all(b"ping").await.unwrap();
    let mut buf = [0; 4];
    active.read_exact(&mut buf).await.unwrap();
    let rotated = s
        .post(&format!("/api/tunnels/{id}/rotate"), json!({"ttl_days":2}))
        .await;
    wait_closed(&mut active).await;
    let mut request =
        tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(format!(
            "ws://127.0.0.1:{st}/control"
        ))
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    let rejected = tokio_tungstenite::connect_async(request).await;
    assert!(
        matches!(rejected,Err(tokio_tungstenite::tungstenite::Error::Http(ref r)) if r.status()==401)
    );
    c.put(cid, entry(rotated["token"].as_str().unwrap(), local))
        .await;
    s.wait_online(id).await;
    roundtrip(p1, 1000).await;
    let mut disabled = tunnel(p1);
    disabled["enabled"] = json!(false);
    s.put(id, disabled).await;
    assert!(TcpStream::connect(("127.0.0.1", p1)).await.is_err());
    roundtrip(p2, 1000).await;
    s.put(id, tunnel(p1)).await;
    // Explicitly restart the client entry after an authorization failure.
    c.put(cid, entry(rotated["token"].as_str().unwrap(), local))
        .await;
    s.wait_online(id).await;
    client.0.kill().unwrap();
    client.0.wait().unwrap();
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    server = launch(env!("CARGO_BIN_EXE_myfrps"), &sargs);
    client = launch(env!("CARGO_BIN_EXE_myfrpc"), &cargs);
    let s = Api::login(format!("http://127.0.0.1:{sa}"), None).await;
    let c = Api::login(format!("http://127.0.0.1:{ca}"), None).await;
    s.wait_online(id).await;
    roundtrip(p1, 5000).await;
    assert_eq!(c.get().await["items"].as_array().unwrap().len(), 2);
    s.call(reqwest::Method::DELETE, &format!("/api/tunnels/{id}"), None)
        .await;
    assert!(TcpStream::connect(("127.0.0.1", p1)).await.is_err());
    let logs = s.call(reqwest::Method::GET, "/api/audit", None).await;
    assert!(
        logs.as_array()
            .unwrap()
            .iter()
            .any(|v| v["action"] == "rotate")
    );
    drop((server, client));
    echo_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls_verified_transport() {
    let sd = TempDir::new().unwrap();
    let cd = TempDir::new().unwrap();
    let (sa, st, ca, p) = (port(), port(), port(), port());
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let cert = certified.cert.pem();
    let key = certified.signing_key.serialize_pem();
    let cert_path = sd.path().join("cert.pem");
    let key_path = sd.path().join("tls-key.pem");
    std::fs::write(&cert_path, &cert).unwrap();
    std::fs::write(&key_path, key).unwrap();
    let args = server_args(&sd, sa, st);
    let mut config: Value = serde_json::from_slice(&std::fs::read(&args[0]).unwrap()).unwrap();
    config["public_url"] = json!(format!("wss://localhost:{st}"));
    config["allow_insecure"] = json!(false);
    config["tls_cert"] = json!("cert.pem");
    config["tls_key"] = json!("tls-key.pem");
    std::fs::write(&args[0], serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let _server = launch(env!("CARGO_BIN_EXE_myfrps"), &args);
    let mut args = client_args(&cd, ca);
    args.retain(|s| s != "--allow-insecure");
    args.extend(["--ca-cert".into(), cert_path.to_str().unwrap().into()]);
    let _client = launch(env!("CARGO_BIN_EXE_myfrpc"), &args);
    let s = Api::login(format!("https://localhost:{sa}"), Some(&cert)).await;
    let c = Api::login(format!("http://127.0.0.1:{ca}"), None).await;
    let (local, echo_task) = echo().await;
    let t = s.post("/api/tunnels", tunnel(p)).await;
    c.post("/api/tunnels", entry(t["token"].as_str().unwrap(), local))
        .await;
    s.wait_online(t["id"].as_str().unwrap()).await;
    roundtrip(p, 1024 * 1024).await;
    echo_task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authentication_ticket_replay_and_live_expiry() {
    use base64::Engine;
    use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
    use futures_util::{SinkExt, StreamExt};
    use my_frp::common::{Claims, Control, now};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

    let sd = TempDir::new().unwrap();
    let (sa, st, public) = (port(), port(), port());
    let _server = launch(env!("CARGO_BIN_EXE_myfrps"), &server_args(&sd, sa, st));
    let s = Api::login(format!("http://127.0.0.1:{sa}"), None).await;
    let t = s.post("/api/tunnels", tunnel(public)).await;
    let token = t["token"].as_str().unwrap();
    let mut claims: Claims = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    let key = ed25519_dalek::SigningKey::from_pkcs8_pem(
        &std::fs::read_to_string(sd.path().join("signing.pem")).unwrap(),
    )
    .unwrap();
    let signing = jsonwebtoken::EncodingKey::from_ed_der(key.to_pkcs8_der().unwrap().as_bytes());
    let sign = |c: &Claims| {
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA),
            c,
            &signing,
        )
        .unwrap()
    };
    let request = |path: &str, secret: &str| {
        let mut r = format!("ws://127.0.0.1:{st}/{path}")
            .into_client_request()
            .unwrap();
        r.headers_mut()
            .insert("Authorization", format!("Bearer {secret}").parse().unwrap());
        r
    };
    for invalid in ["expired", "audience", "future", "version", "signature"] {
        let mut c = claims.clone();
        match invalid {
            "expired" => c.exp = now() - 1,
            "audience" => c.aud = "wrong-audience".into(),
            "future" => c.nbf = now() + 3600,
            "version" => c.version += 1,
            _ => {}
        }
        let mut jwt = sign(&c);
        if invalid == "signature" {
            let index = jwt.rfind('.').unwrap() + 1;
            let replacement = if &jwt[index..index + 1] == "A" {
                "B"
            } else {
                "A"
            };
            jwt.replace_range(index..index + 1, replacement);
        }
        let response = tokio_tungstenite::connect_async(request("control", &jwt)).await;
        assert!(
            matches!(response, Err(tokio_tungstenite::tungstenite::Error::Http(ref r)) if r.status() == 401),
            "accepted {invalid}"
        );
    }
    claims.exp = now() + 5;
    let (mut control, _) = tokio_tungstenite::connect_async(request("control", &sign(&claims)))
        .await
        .unwrap();
    assert!(matches!(
        control.next().await.unwrap().unwrap(),
        Message::Text(_)
    ));
    let mut public_socket = TcpStream::connect(("127.0.0.1", public)).await.unwrap();
    let ticket = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Message::Text(text) = control.next().await.unwrap().unwrap() {
                match serde_json::from_str::<Control>(&text).unwrap() {
                    Control::Open { ticket } => break ticket,
                    Control::Ping => control
                        .send(Message::Text(
                            serde_json::to_string(&Control::Pong).unwrap().into(),
                        ))
                        .await
                        .unwrap(),
                    _ => {}
                }
            }
        }
    })
    .await
    .unwrap();
    let (mut data, _) = tokio_tungstenite::connect_async(request("data", &ticket))
        .await
        .unwrap();
    let replay = tokio_tungstenite::connect_async(request("data", &ticket)).await;
    assert!(
        matches!(replay, Err(tokio_tungstenite::tungstenite::Error::Http(ref r)) if r.status() == 401)
    );
    data.send(Message::Binary(b"verified".to_vec().into()))
        .await
        .unwrap();
    let mut buf = [0; 8];
    public_socket.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"verified");
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut b = [0];
        let r = public_socket.read(&mut b).await;
        assert!(matches!(r, Ok(0) | Err(_)));
    })
    .await
    .expect("expired session kept its TCP connection alive");
}
