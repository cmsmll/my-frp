//! Persist application events without taking the SQLite mutex on tracing threads.
use crate::common::{Admin, ApiResult, Db, bad, now};
use anyhow::Result;
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fmt,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    time::Duration,
};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

const MAX_RECORDS: i64 = 10_000;
const QUEUE_CAPACITY: usize = 2_048;
static SINKS: OnceLock<Mutex<Vec<Sink>>> = OnceLock::new();

struct Sink {
    db: Weak<Db>,
    role: &'static str,
    tx: SyncSender<Record>,
    dropped: Arc<AtomicU64>,
}

#[derive(Clone, Debug)]
struct Record {
    at: u64,
    level: String,
    component: String,
    tunnel_id: Option<String>,
    message: String,
}

pub fn init() {
    // try_init also makes embedding and repeated initialization in tests safe.
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "my_frp=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(RuntimeLayer)
        .try_init();
}

pub fn attach(db: Arc<Db>, role: &'static str) -> Result<()> {
    {
        let connection = db.0.lock().unwrap();
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS runtime_logs(
                id INTEGER PRIMARY KEY,
                at INTEGER NOT NULL,
                level TEXT NOT NULL,
                component TEXT NOT NULL,
                tunnel_id TEXT,
                message TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS runtime_logs_tunnel ON runtime_logs(tunnel_id,id);",
        )?;
        trim(&connection)?;
    }
    let mut sinks = SINKS.get_or_init(Default::default).lock().unwrap();
    sinks.retain(|sink| sink.db.strong_count() > 0);
    if sinks
        .iter()
        .any(|sink| sink.db.ptr_eq(&Arc::downgrade(&db)))
    {
        return Ok(());
    }
    let (tx, rx) = mpsc::sync_channel::<Record>(QUEUE_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    let writer_dropped = dropped.clone();
    let weak_db = Arc::downgrade(&db);
    std::thread::Builder::new()
        .name(format!("myfrp-{role}-logs"))
        .spawn(move || {
            loop {
                let first = match rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(record) => record,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if weak_db.strong_count() == 0 {
                            break;
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let Some(db) = weak_db.upgrade() else {
                    break;
                };
                let mut records = vec![first];
                let deadline = std::time::Instant::now() + Duration::from_millis(100);
                while records.len() < 64 {
                    match rx
                        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    {
                        Ok(record) => records.push(record),
                        Err(_) => break,
                    }
                }
                let lost = writer_dropped.swap(0, Ordering::Relaxed);
                if lost > 0 {
                    records.push(Record {
                        at: now(),
                        level: "WARN".into(),
                        component: "my_frp::logging".into(),
                        tunnel_id: None,
                        message: format!("日志队列已满，已丢弃 {lost} 条日志"),
                    });
                }
                // Never trace from this worker: an SQLite error must not create a log loop.
                if let Err(error) = persist(&db, &records) {
                    eprintln!("运行日志写入失败: {error}");
                }
            }
        })?;
    tx.try_send(Record {
        at: now(),
        level: "INFO".into(),
        component: format!("my_frp::{role}"),
        tunnel_id: None,
        message: "运行日志已启用，最多保留最近 10000 条".into(),
    })?;
    sinks.push(Sink {
        db: Arc::downgrade(&db),
        role,
        tx,
        dropped,
    });
    Ok(())
}

fn trim(connection: &rusqlite::Connection) -> rusqlite::Result<usize> {
    connection.execute(
        "DELETE FROM runtime_logs WHERE id <= (
            SELECT id FROM runtime_logs ORDER BY id DESC LIMIT 1 OFFSET ?
        )",
        [MAX_RECORDS],
    )
}

fn persist(db: &Db, records: &[Record]) -> Result<()> {
    let mut connection = db.0.lock().unwrap();
    let transaction = connection.transaction()?;
    for record in records {
        transaction.execute(
            "INSERT INTO runtime_logs(at,level,component,tunnel_id,message) VALUES(?,?,?,?,?)",
            params![
                record.at as i64,
                record.level,
                record.component,
                record.tunnel_id,
                record.message
            ],
        )?;
    }
    trim(&transaction)?;
    transaction.commit()?;
    Ok(())
}

struct RuntimeLayer;
impl<S: Subscriber> Layer<S> for RuntimeLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let metadata = event.metadata();
        if !metadata.target().starts_with("my_frp") {
            return;
        }
        let Some(sinks) = SINKS.get() else {
            return;
        };
        let mut visitor = EventFields::default();
        event.record(&mut visitor);
        let record = Record {
            at: now(),
            level: metadata.level().to_string(),
            component: metadata.target().to_owned(),
            tunnel_id: visitor.tunnel_id,
            message: redact(&visitor.parts.join(" · ")),
        };
        let sinks = sinks.lock().unwrap();
        for sink in sinks.iter() {
            // A process normally owns one database. Keep server/client events separate
            // when both roles are embedded in an integration harness.
            if (metadata.target().starts_with("my_frp::server") && sink.role != "server")
                || (metadata.target().starts_with("my_frp::client") && sink.role != "client")
            {
                continue;
            }
            if let Err(mpsc::TrySendError::Full(_)) = sink.tx.try_send(record.clone()) {
                sink.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Default)]
struct EventFields {
    tunnel_id: Option<String>,
    parts: Vec<String>,
}
impl EventFields {
    fn push(&mut self, field: &Field, value: String) {
        let name = field.name();
        if sensitive_field(name) {
            self.parts.push(format!("{name}=[REDACTED]"));
        } else if name == "tunnel_id" {
            self.tunnel_id = Some(redact(value.trim_matches('"')));
        } else if name == "message" {
            self.parts.insert(0, redact(&value));
        } else {
            self.parts.push(format!("{name}={}", redact(&value)));
        }
    }
}
impl Visit for EventFields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.push(field, format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value.to_owned());
    }
}

fn sensitive_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "token",
        "credential",
        "authorization",
        "secret",
        "private_key",
        "signing_key",
        "ticket",
    ]
    .iter()
    .any(|part| name.contains(part))
}

// Sanitize error text too: websocket handshake errors may include complete HTTP headers.
fn redact(value: &str) -> String {
    if value.contains("-----BEGIN") && value.contains("PRIVATE KEY-----") {
        return "[REDACTED: private key]".into();
    }
    let lowercase = value.to_ascii_lowercase();
    let keys = [
        "admin_password",
        "authorization",
        "private_key",
        "signing_key",
        "credentials",
        "credential",
        "password",
        "passwd",
        "token",
        "secret",
        "ticket",
    ];
    let mut clean = String::new();
    let mut index = 0;
    while index < value.len() {
        let boundary = index == 0
            || !value.as_bytes()[index - 1].is_ascii_alphanumeric()
                && value.as_bytes()[index - 1] != b'_';
        let key = boundary
            .then(|| {
                keys.iter().find(|key| {
                    lowercase[index..].starts_with(**key)
                        && lowercase
                            .as_bytes()
                            .get(index + key.len())
                            .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
                })
            })
            .flatten();
        if let Some(key) = key {
            let mut start = index + key.len();
            if matches!(value.as_bytes().get(start), Some(b'"' | b'\'')) {
                start += 1;
            }
            while value
                .as_bytes()
                .get(start)
                .is_some_and(u8::is_ascii_whitespace)
            {
                start += 1;
            }
            if matches!(value.as_bytes().get(start), Some(b':' | b'=')) {
                start += 1;
                while value
                    .as_bytes()
                    .get(start)
                    .is_some_and(u8::is_ascii_whitespace)
                {
                    start += 1;
                }
                let mut end = start;
                if let Some(quote @ (b'"' | b'\'')) = value.as_bytes().get(start) {
                    end += 1;
                    while end < value.len() && value.as_bytes()[end] != *quote {
                        if value.as_bytes()[end] == b'\\' {
                            end += 1;
                        }
                        end += 1;
                    }
                    end = (end + 1).min(value.len());
                } else {
                    // Include "Bearer <value>" when a header is rendered without quotes.
                    if lowercase[start..].starts_with("bearer ") {
                        end += 7;
                    }
                    while end < value.len()
                        && !matches!(
                            value.as_bytes()[end],
                            b' ' | b'\t' | b'\r' | b'\n' | b',' | b';' | b'}' | b')'
                        )
                    {
                        end += 1;
                    }
                }
                clean.push_str(&value[index..start]);
                clean.push_str("[REDACTED]");
                index = end;
                continue;
            }
        }
        if boundary && lowercase[index..].starts_with("bearer ") {
            clean.push_str("Bearer [REDACTED]");
            index += 7;
            while index < value.len()
                && !matches!(
                    value.as_bytes()[index],
                    b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'\'' | b',' | b'}' | b')'
                )
            {
                index += 1;
            }
            continue;
        }
        let ch = value[index..].chars().next().unwrap();
        clean.push(ch);
        index += ch.len_utf8();
    }
    let mut output = String::new();
    let mut token = String::new();
    let flush = |output: &mut String, token: &mut String| {
        let parts: Vec<_> = token.trim_matches('.').split('.').collect();
        if parts.len() == 3
            && parts[0].starts_with("eyJ")
            && parts.iter().all(|part| !part.is_empty())
        {
            output.push_str("[REDACTED JWT]");
        } else {
            output.push_str(token);
        }
        token.clear();
    };
    for ch in clean.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            token.push(ch);
        } else {
            flush(&mut output, &mut token);
            output.push(ch);
        }
    }
    flush(&mut output, &mut token);
    // Limit record size and neutralize control characters so exports remain readable.
    output
        .chars()
        .take(4096)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

#[derive(Default, Deserialize)]
pub struct Filters {
    level: Option<String>,
    q: Option<String>,
    limit: Option<usize>,
    tunnel_id: Option<String>,
}

#[derive(Serialize)]
struct Item {
    id: i64,
    at: i64,
    level: String,
    component: String,
    tunnel_id: Option<String>,
    message: String,
}

fn read(db: &Db, filters: &Filters) -> ApiResult<Vec<Item>> {
    let level = filters
        .level
        .as_deref()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let level = if level == "ALL" { String::new() } else { level };
    if !["", "TRACE", "DEBUG", "INFO", "WARN", "ERROR"].contains(&level.as_str()) {
        return Err(bad("日志级别无效"));
    }
    let query = filters.q.as_deref().unwrap_or_default().trim();
    let tunnel = filters.tunnel_id.as_deref().unwrap_or_default().trim();
    if query.len() > 256 || tunnel.len() > 100 {
        return Err(bad("日志筛选条件过长"));
    }
    let limit = filters.limit.unwrap_or(200).clamp(1, 500) as i64;
    let connection = db.0.lock().unwrap();
    let mut statement = connection
        .prepare(
            "SELECT id,at,level,component,tunnel_id,message FROM runtime_logs
            WHERE (?1='' OR level=?1)
            AND (?2='' OR instr(lower(message),lower(?2))>0 OR instr(lower(component),lower(?2))>0
                OR instr(lower(COALESCE(tunnel_id,'')),lower(?2))>0)
            AND (?3='' OR tunnel_id=?3)
            ORDER BY id DESC LIMIT ?4",
        )
        .map_err(anyhow::Error::from)?;
    let rows = statement
        .query_map(params![level, query, tunnel, limit], |row| {
            Ok(Item {
                id: row.get(0)?,
                at: row.get(1)?,
                level: row.get(2)?,
                component: row.get(3)?,
                tunnel_id: row.get(4)?,
                message: row.get(5)?,
            })
        })
        .map_err(anyhow::Error::from)?;
    Ok(rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(anyhow::Error::from)?)
}

pub async fn list(
    State(admin): State<Arc<Admin>>,
    headers: HeaderMap,
    Query(filters): Query<Filters>,
) -> ApiResult<Json<Value>> {
    admin.check(&headers)?;
    let items = read(&admin.db, &filters)?;
    Ok(Json(
        json!({"items":items,"retention":MAX_RECORDS,"limit":filters.limit.unwrap_or(200).clamp(1,500)}),
    ))
}

pub async fn export(
    State(admin): State<Arc<Admin>>,
    headers: HeaderMap,
    Query(filters): Query<Filters>,
) -> ApiResult<Response> {
    admin.check(&headers)?;
    let items = read(&admin.db, &filters)?;
    let mut text = String::from("my-frp 运行日志（时间为 Unix 秒，最新记录在前）\n");
    for item in items {
        use std::fmt::Write;
        let _ = writeln!(
            text,
            "{} [{}] {} {} {}",
            item.at,
            item.level,
            item.component,
            item.tunnel_id.as_deref().unwrap_or("-"),
            item.message
        );
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"my-frp-logs.txt\"",
            ),
        ],
        text,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_credentials_in_fields_and_error_messages() {
        for value in [
            "Authorization: Bearer abc123",
            "request failed: {\"authorization\": \"Bearer abc123\"}",
            "password=abc123 endpoint=127.0.0.1",
            "token: 'abc123'",
            "凭证 eyJhbGciOiJFZERTQSJ9.eyJzdWIiOiIxIn0.abc123 无效",
            "ticket=abc123",
        ] {
            let clean = redact(value);
            assert!(!clean.contains("abc123"), "{clean}");
        }
        assert!(redact("连接失败，目标 127.0.0.1:8080").contains("127.0.0.1:8080"));
        assert!(redact("error:-----BEGIN PRIVATE KEY-----abc123").contains("REDACTED"));
        assert!(sensitive_field("refresh_token"));
    }

    #[test]
    fn retention_keeps_latest_records_and_filters_match() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        db.0.lock().unwrap().execute_batch(
            "CREATE TABLE runtime_logs(id INTEGER PRIMARY KEY,at INTEGER NOT NULL,level TEXT NOT NULL,component TEXT NOT NULL,tunnel_id TEXT,message TEXT NOT NULL);"
        ).unwrap();
        let records: Vec<_> = (0..MAX_RECORDS + 5)
            .map(|id| Record {
                at: id as u64,
                level: "INFO".into(),
                component: "test".into(),
                tunnel_id: Some("a".into()),
                message: format!("event {id}"),
            })
            .collect();
        persist(&db, &records).unwrap();
        let connection = db.0.lock().unwrap();
        let (count, first): (i64, i64) = connection
            .query_row("SELECT count(*),min(at) FROM runtime_logs", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!((count, first), (MAX_RECORDS, 5));
        drop(connection);
        let items = read(
            &db,
            &Filters {
                q: Some("event 10004".into()),
                tunnel_id: Some("a".into()),
                ..Default::default()
            },
        )
        .unwrap_or_else(|_| panic!("filter failed"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].at, 10004);
        assert!(
            read(
                &db,
                &Filters {
                    level: Some("ERROR".into()),
                    ..Default::default()
                }
            )
            .unwrap_or_default()
            .is_empty()
        );
    }
}
