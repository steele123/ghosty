use std::{
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, CONTENT_TYPE},
    Url,
};
use serde_json::Value;

use crate::models::{LogCategory, LogEntry, LogLevel};

pub const LOCALHOST_DOMAIN: &str = "deceive-localhost.molenzwiebel.xyz";
const CONFIG_URL: &str = "https://clientconfig.rpg.riotgames.com";
const CONFIG_HOST: &str = "clientconfig.rpg.riotgames.com";
const GEO_PAS_URL: &str = "https://riot-geo.pas.si.riotgames.com/pas/v1/service/chat";
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);
const CONFIG_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HEADER_BYTES: usize = 64 * 1024;
const JSON_CONTENT_TYPE: &str = "application/json";
const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

#[derive(Debug, Clone)]
pub struct PatchedChatServer {
    pub host: String,
    pub port: u16,
    pub affinity: Option<String>,
}

pub fn start(
    chat_port: u16,
    running: Arc<AtomicBool>,
    patched_tx: Sender<PatchedChatServer>,
    log_tx: Sender<LogEntry>,
) -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("Unable to bind config proxy")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let client = config_client()?;

    thread::spawn(move || {
        log_config(
            &log_tx,
            format!("Config proxy listening on 127.0.0.1:{port}"),
        );

        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let client = client.clone();
                    let tx = patched_tx.clone();
                    let logs = log_tx.clone();
                    let running = running.clone();
                    thread::spawn(move || {
                        if let Err(error) =
                            handle_request(stream, &client, chat_port, running, tx, logs.clone())
                        {
                            log_error(&logs, format!("Config proxy request failed: {error:#}"));
                        }
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(30));
                }
                Err(error) => log_error(&log_tx, format!("Config proxy accept failed: {error}")),
            }
        }
    });

    Ok(port)
}

fn config_client() -> Result<Client> {
    Client::builder()
        .timeout(CONFIG_REQUEST_TIMEOUT)
        .build()
        .context("Unable to build reqwest client")
}

fn handle_request(
    mut stream: TcpStream,
    client: &Client,
    chat_port: u16,
    running: Arc<AtomicBool>,
    patched_tx: Sender<PatchedChatServer>,
    log_tx: Sender<LogEntry>,
) -> Result<()> {
    stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT))?;
    let bytes = match read_http_headers(&mut stream) {
        Ok(bytes) => bytes,
        Err(HeaderReadError::TooLarge) => {
            write_text_response(
                &mut stream,
                431,
                "Request Header Fields Too Large",
                "Config proxy request headers are too large",
            )?;
            return Ok(());
        }
        Err(HeaderReadError::TimedOut) => {
            log_error(&log_tx, "Config proxy request timed out".to_string());
            write_text_response(&mut stream, 408, "Request Timeout", "Request Timeout")?;
            return Ok(());
        }
        Err(HeaderReadError::Incomplete) => {
            log_error(
                &log_tx,
                "Rejected incomplete config proxy request".to_string(),
            );
            write_text_response(&mut stream, 400, "Bad Request", "Bad Request")?;
            return Ok(());
        }
        Err(HeaderReadError::Io(error)) => return Err(error.into()),
    };

    let request = match request_text(&bytes) {
        Ok(request) => request,
        Err(error) => {
            log_error(&log_tx, format!("Rejected config proxy request: {error:#}"));
            write_text_response(&mut stream, 400, "Bad Request", "Bad Request")?;
            return Ok(());
        }
    };
    let mut lines = request.lines();
    let first_line = lines.next().ok_or_else(|| anyhow!("Empty HTTP request"))?;
    let path = match request_path(first_line) {
        Ok(path) => path,
        Err(error) => {
            log_error(&log_tx, format!("Rejected config proxy request: {error:#}"));
            write_text_response(&mut stream, 400, "Bad Request", "Bad Request")?;
            return Ok(());
        }
    };

    let headers = parse_headers(lines);
    let target = format!("{CONFIG_URL}{path}");
    log_config(&log_tx, format!("Patching client config request: {path}"));

    let mut req = client.get(target).header(
        "User-Agent",
        headers
            .get("user-agent")
            .map(String::as_str)
            .unwrap_or("Ghosty"),
    );

    if let Some(value) = headers.get("authorization") {
        req = req.header("Authorization", value);
    }
    if let Some(value) = headers.get("x-riot-entitlements-jwt") {
        req = req.header("X-Riot-Entitlements-JWT", value);
    }

    let result = req.send()?;
    let status = result.status();
    let upstream_content_type = response_content_type(result.headers());
    let mut body = result.bytes()?.to_vec();
    let mut content_type = upstream_content_type
        .as_deref()
        .unwrap_or(JSON_CONTENT_TYPE);

    if status.is_success() {
        if let Ok(mut json) = serde_json::from_slice::<Value>(&body) {
            if let Some(server) = rewrite_config(
                &mut json,
                chat_port,
                client,
                headers.get("authorization"),
                &log_tx,
            ) {
                send_patched_server_if_running(&running, &patched_tx, server);
            }
            body = serde_json::to_vec(&json)?;
            content_type = JSON_CONTENT_TYPE;
        }
    }

    write_response(
        &mut stream,
        status.as_u16(),
        response_reason(status),
        content_type,
        &body,
    )?;
    Ok(())
}

fn send_patched_server_if_running(
    running: &Arc<AtomicBool>,
    patched_tx: &Sender<PatchedChatServer>,
    server: PatchedChatServer,
) {
    if running.load(Ordering::Relaxed) {
        let _ = patched_tx.send(server);
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let response = response_bytes(status, reason, content_type, body);
    stream.write_all(&response)?;
    Ok(())
}

fn write_text_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> Result<()> {
    let response = response_bytes(status, reason, TEXT_CONTENT_TYPE, body.as_bytes());
    stream.write_all(&response)?;
    Ok(())
}

fn response_content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

#[derive(Debug)]
enum HeaderReadError {
    TimedOut,
    TooLarge,
    Incomplete,
    Io(std::io::Error),
}

fn read_http_headers(reader: &mut impl Read) -> std::result::Result<Vec<u8>, HeaderReadError> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 2048];
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(count) => count,
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(HeaderReadError::TimedOut);
            }
            Err(error) => return Err(HeaderReadError::Io(error)),
        };
        if count == 0 {
            return Err(HeaderReadError::Incomplete);
        }
        bytes.extend_from_slice(&chunk[..count]);
        if bytes.len() > MAX_HEADER_BYTES {
            return Err(HeaderReadError::TooLarge);
        }
        if let Some(end) = header_end(&bytes) {
            bytes.truncate(end);
            return Ok(bytes);
        }
    }
}

fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn request_text(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).context("Config proxy request headers are not valid UTF-8")
}

fn response_bytes(status: u16, reason: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn response_reason(status: reqwest::StatusCode) -> &'static str {
    status.canonical_reason().unwrap_or("Unknown")
}

fn request_path(first_line: &str) -> Result<String> {
    let mut parts = first_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow!("Malformed HTTP request line"))?;
    let target = parts
        .next()
        .ok_or_else(|| anyhow!("Malformed HTTP request line"))?;
    let version = parts
        .next()
        .ok_or_else(|| anyhow!("Malformed HTTP request line"))?;

    if parts.next().is_some() || !valid_http_version(version) {
        return Err(anyhow!("Malformed HTTP request line"));
    }
    if !method.eq_ignore_ascii_case("GET") {
        return Err(anyhow!("Unsupported config proxy method: {method}"));
    }
    if target.contains('#') {
        return Err(anyhow!(
            "Unsupported config proxy request target with fragment: {target}"
        ));
    }
    if target.starts_with('/') {
        if target.starts_with("//") {
            return Err(anyhow!(
                "Unsupported config proxy scheme-relative target: {target}"
            ));
        }
        return allowed_config_path(target);
    }

    if !target.starts_with("http://") && !target.starts_with("https://") {
        return Err(anyhow!("Unsupported config proxy request target: {target}"));
    }

    absolute_request_path(target)
}

fn valid_http_version(version: &str) -> bool {
    matches!(version, "HTTP/1.0" | "HTTP/1.1" | "HTTP/2" | "HTTP/2.0")
}

fn absolute_request_path(target: &str) -> Result<String> {
    let url = Url::parse(target)
        .with_context(|| format!("Malformed absolute config proxy request target: {target}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("Absolute config proxy request target has no host"))?;
    if !host.eq_ignore_ascii_case(CONFIG_HOST) {
        return Err(anyhow!("Unsupported config proxy request host: {host}"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow!(
            "Absolute config proxy request target cannot include userinfo"
        ));
    }
    if url.path() == "/" && url.query().is_none() {
        return Err(anyhow!("Absolute config proxy request target has no path"));
    }

    let mut path = url.path().to_string();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    allowed_config_path(&path)
}

fn allowed_config_path(path: &str) -> Result<String> {
    let route = path.split_once('?').map_or(path, |(route, _)| route);
    if matches!(route, "/api/v1/config/player" | "/api/v1/config/public") {
        Ok(path.to_string())
    } else {
        Err(anyhow!("Unsupported config proxy path: {route}"))
    }
}

fn rewrite_config(
    json: &mut Value,
    chat_port: u16,
    client: &Client,
    authorization: Option<&String>,
    log_tx: &Sender<LogEntry>,
) -> Option<PatchedChatServer> {
    let affinity = if json
        .get("chat.affinity.enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        match authorization {
            Some(auth) => match player_affinity(client, auth) {
                Ok(affinity) => {
                    log_config(log_tx, format!("Using Riot chat affinity: {affinity}"));
                    Some(affinity)
                }
                Err(error) => {
                    log_warn(
                        log_tx,
                        format!("Unable to resolve Riot chat affinity: {error:#}"),
                    );
                    None
                }
            },
            None => {
                log_warn(
                    log_tx,
                    "Riot chat affinity was enabled but no authorization header was present"
                        .to_string(),
                );
                None
            }
        }
    } else {
        None
    };

    rewrite_config_with_affinity(json, chat_port, affinity.as_deref())
}

fn rewrite_config_with_affinity(
    json: &mut Value,
    chat_port: u16,
    affinity: Option<&str>,
) -> Option<PatchedChatServer> {
    let original_host = json
        .get("chat.host")
        .and_then(chat_host_value)
        .map(ToOwned::to_owned);
    let original_port = json.get("chat.port").and_then(chat_port_value);
    let affinity_host = affinity_host(json, affinity, original_host.is_none());
    let server = affinity_host
        .or(original_host)
        .zip(original_port)
        .map(|(host, port)| PatchedChatServer {
            host,
            port,
            affinity: affinity.map(ToOwned::to_owned),
        })?;

    if json.get("chat.host").is_some() {
        json["chat.host"] = Value::String(LOCALHOST_DOMAIN.to_string());
    }
    if json.get("chat.port").is_some() {
        json["chat.port"] = Value::Number(chat_port.into());
    }
    patch_affinities(json);

    Some(server)
}

fn chat_port_value(value: &Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .or_else(|| value.as_str().and_then(|port| port.parse::<u16>().ok()))
        .filter(|port| *port != 0)
}

fn chat_host_value(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|host| !host.is_empty())
}

fn affinity_host(json: &Value, affinity: Option<&str>, allow_any_affinity: bool) -> Option<String> {
    let items = json.get("chat.affinities").and_then(Value::as_object)?;
    affinity
        .and_then(|affinity| items.get(affinity))
        .and_then(chat_host_value)
        .map(ToOwned::to_owned)
        .or_else(|| {
            allow_any_affinity
                .then(|| {
                    items
                        .values()
                        .find_map(chat_host_value)
                        .map(ToOwned::to_owned)
                })
                .flatten()
        })
}

fn patch_affinities(json: &mut Value) {
    let Some(items) = json
        .get_mut("chat.affinities")
        .and_then(Value::as_object_mut)
    else {
        return;
    };

    for value in items.values_mut() {
        if value.is_string() {
            *value = Value::String(LOCALHOST_DOMAIN.to_string());
        }
    }
}

fn player_affinity(client: &Client, authorization: &str) -> Result<String> {
    let token = client
        .get(GEO_PAS_URL)
        .header("Authorization", authorization)
        .send()?
        .text()?;
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("Geo PAS did not return a JWT"))?;
    let decoded = URL_SAFE_NO_PAD.decode(payload)?;
    let json: Value = serde_json::from_slice(&decoded)?;
    json.get("affinity")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("Geo PAS JWT did not contain an affinity"))
}

fn parse_headers<'a>(lines: impl Iterator<Item = &'a str>) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    for (key, value) in lines.filter_map(|line| line.split_once(':')) {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        headers
            .entry(key.trim().to_ascii_lowercase())
            .or_insert_with(|| value.to_string());
    }
    headers
}

fn log_config(tx: &Sender<LogEntry>, message: String) {
    let _ = tx.send(LogEntry {
        timestamp: chrono::Utc::now().format("%H:%M:%S").to_string(),
        level: LogLevel::Info,
        category: LogCategory::Config,
        message,
    });
}

fn log_error(tx: &Sender<LogEntry>, message: String) {
    let _ = tx.send(LogEntry {
        timestamp: chrono::Utc::now().format("%H:%M:%S").to_string(),
        level: LogLevel::Error,
        category: LogCategory::Error,
        message,
    });
}

fn log_warn(tx: &Sender<LogEntry>, message: String) {
    let _ = tx.send(LogEntry {
        timestamp: chrono::Utc::now().format("%H:%M:%S").to_string(),
        level: LogLevel::Warn,
        category: LogCategory::Config,
        message,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    use serde_json::json;

    #[test]
    fn log_error_marks_error_category_and_level() {
        let (tx, rx) = mpsc::channel();

        log_error(&tx, "Config proxy failed".to_string());
        let line = rx.recv().expect("log line should be sent");

        assert_eq!(line.category, LogCategory::Error);
        assert_eq!(line.level, LogLevel::Error);
        assert_eq!(line.message, "Config proxy failed");
    }

    #[test]
    fn log_config_marks_config_info() {
        let (tx, rx) = mpsc::channel();

        log_config(&tx, "Patching config".to_string());
        let line = rx.recv().expect("log line should be sent");

        assert_eq!(line.category, LogCategory::Config);
        assert_eq!(line.level, LogLevel::Info);
        assert_eq!(line.message, "Patching config");
    }

    #[test]
    fn request_path_accepts_origin_form_target() {
        assert_eq!(
            request_path("GET /api/v1/config/player?region=NA HTTP/1.1")
                .expect("request path should parse"),
            "/api/v1/config/player?region=NA"
        );
    }

    #[test]
    fn request_path_accepts_public_config_with_namespace_query() {
        assert_eq!(
            request_path(
                "GET /api/v1/config/public?region=NA&namespace=keystone.self_update HTTP/1.1"
            )
            .expect("request path should parse"),
            "/api/v1/config/public?region=NA&namespace=keystone.self_update"
        );
    }

    #[test]
    fn request_path_accepts_absolute_form_target() {
        assert_eq!(
            request_path(
                "GET http://clientconfig.rpg.riotgames.com/api/v1/config/public?region=NA HTTP/1.1"
            )
            .expect("request path should parse"),
            "/api/v1/config/public?region=NA"
        );
    }

    #[test]
    fn request_path_accepts_absolute_form_target_with_port_and_mixed_case_host() {
        assert_eq!(
            request_path(
                "GET https://ClientConfig.RPG.RiotGames.com:443/api/v1/config/public?region=NA HTTP/1.1"
            )
            .expect("request path should parse"),
            "/api/v1/config/public?region=NA"
        );
    }

    #[test]
    fn request_path_rejects_unsupported_method() {
        assert!(request_path("POST /api/v1/config/player HTTP/1.1").is_err());
    }

    #[test]
    fn request_path_rejects_invalid_http_version() {
        assert!(request_path("GET /api/v1/config/player HTTP/not-a-version").is_err());
        assert!(request_path("GET /api/v1/config/player FTP/1.0").is_err());
    }

    #[test]
    fn request_path_rejects_scheme_relative_origin_target() {
        assert!(request_path("GET //example.com/api/v1/config/player HTTP/1.1").is_err());
    }

    #[test]
    fn request_path_rejects_targets_with_fragments() {
        assert!(request_path("GET /api/v1/config/player?region=NA#fragment HTTP/1.1").is_err());
        assert!(request_path(
            "GET https://clientconfig.rpg.riotgames.com/api/v1/config/public#fragment HTTP/1.1"
        )
        .is_err());
    }

    #[test]
    fn request_path_rejects_malformed_absolute_target() {
        assert!(request_path("GET http://clientconfig.rpg.riotgames.com HTTP/1.1").is_err());
    }

    #[test]
    fn request_path_rejects_unknown_target_form() {
        assert!(request_path("GET clientconfig.rpg.riotgames.com/api/v1/config HTTP/1.1").is_err());
    }

    #[test]
    fn request_path_rejects_unrelated_origin_form_path() {
        assert!(request_path("GET /favicon.ico HTTP/1.1").is_err());
        assert!(request_path("GET /api/v1/other/player HTTP/1.1").is_err());
    }

    #[test]
    fn request_path_rejects_absolute_form_target_for_wrong_host() {
        assert!(request_path("GET https://example.com/api/v1/config/public HTTP/1.1").is_err());
    }

    #[test]
    fn request_path_rejects_absolute_form_target_with_userinfo() {
        assert!(request_path(
            "GET https://user@clientconfig.rpg.riotgames.com/api/v1/config/public HTTP/1.1"
        )
        .is_err());
    }

    #[test]
    fn response_text_includes_status_headers_and_body_length() {
        let response = String::from_utf8(response_bytes(
            400,
            "Bad Request",
            TEXT_CONTENT_TYPE,
            b"Bad Request",
        ))
        .expect("response should be utf8");

        assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(response.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        assert!(response.contains("Content-Length: 11\r\n"));
        assert!(response.ends_with("\r\n\r\nBad Request"));
    }

    #[test]
    fn response_json_includes_json_content_type() {
        let response = String::from_utf8(response_bytes(200, "OK", JSON_CONTENT_TYPE, b"{}"))
            .expect("response should be utf8");

        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type: application/json\r\n"));
        assert!(response.contains("Content-Length: 2\r\n"));
        assert!(response.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn response_bytes_preserves_custom_content_type() {
        let response = String::from_utf8(response_bytes(
            502,
            "Bad Gateway",
            "text/html; charset=utf-8",
            b"<h1>bad</h1>",
        ))
        .expect("response should be utf8");

        assert!(response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(response.contains("Content-Type: text/html; charset=utf-8\r\n"));
        assert!(response.contains("Content-Length: 12\r\n"));
        assert!(response.ends_with("\r\n\r\n<h1>bad</h1>"));
    }

    #[test]
    fn response_content_type_reads_valid_header() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "text/plain".parse().expect("valid header"));

        assert_eq!(
            response_content_type(&headers).as_deref(),
            Some("text/plain")
        );
    }

    #[test]
    fn response_content_type_ignores_missing_header() {
        assert!(response_content_type(&HeaderMap::new()).is_none());
    }

    #[test]
    fn response_bytes_uses_body_byte_length() {
        let body = "é".as_bytes();
        let response = response_bytes(200, "OK", JSON_CONTENT_TYPE, body);
        let header = String::from_utf8(
            response
                .split(|byte| *byte == b'\n')
                .take(4)
                .flat_map(|line| line.iter().copied().chain([b'\n']))
                .collect(),
        )
        .expect("headers should be utf8");

        assert!(header.contains("Content-Length: 2\r\n"));
        assert!(response.ends_with(body));
    }

    #[test]
    fn response_bytes_preserves_non_utf8_body() {
        let body = &[0xff, 0xfe, 0x00, b'a'];
        let response = response_bytes(502, "Bad Gateway", "application/octet-stream", body);

        assert!(response
            .windows(b"Content-Type: application/octet-stream\r\n".len())
            .any(|window| window == b"Content-Type: application/octet-stream\r\n"));
        assert!(response
            .windows(b"Content-Length: 4\r\n".len())
            .any(|window| window == b"Content-Length: 4\r\n"));
        assert!(response.ends_with(body));
    }

    #[test]
    fn response_reason_uses_canonical_reason_when_available() {
        assert_eq!(
            response_reason(reqwest::StatusCode::BAD_REQUEST),
            "Bad Request"
        );
    }

    #[test]
    fn response_reason_uses_unknown_for_nonstandard_status() {
        let status = reqwest::StatusCode::from_u16(599).expect("599 should be a valid status");

        assert_eq!(response_reason(status), "Unknown");
    }

    #[test]
    fn read_http_headers_stops_after_header_terminator() {
        let mut input = std::io::Cursor::new(b"GET / HTTP/1.1\r\nHost: local\r\n\r\nignored");

        let headers = read_http_headers(&mut input).expect("headers should read");

        assert_eq!(headers, b"GET / HTTP/1.1\r\nHost: local\r\n\r\n");
    }

    #[test]
    fn read_http_headers_rejects_eof_without_header_terminator() {
        let mut input = std::io::Cursor::new(b"GET / HTTP/1.1\r\nHost: local");

        let error = read_http_headers(&mut input).expect_err("partial headers should fail");

        assert!(matches!(error, HeaderReadError::Incomplete));
    }

    #[test]
    fn read_http_headers_rejects_empty_eof() {
        let mut input = std::io::Cursor::new(Vec::<u8>::new());

        let error = read_http_headers(&mut input).expect_err("empty request should fail");

        assert!(matches!(error, HeaderReadError::Incomplete));
    }

    #[test]
    fn request_text_accepts_valid_utf8_headers() {
        let request = request_text(b"GET / HTTP/1.1\r\nUser-Agent: Riot\r\n\r\n")
            .expect("valid request text should parse");

        assert!(request.contains("User-Agent: Riot"));
    }

    #[test]
    fn request_text_rejects_invalid_utf8_headers() {
        let error = request_text(b"GET / HTTP/1.1\r\nAuthorization: \xFF\r\n\r\n")
            .expect_err("invalid request text should fail");

        assert!(error.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn read_http_headers_rejects_oversized_headers() {
        let mut input = std::io::Cursor::new(vec![b'a'; MAX_HEADER_BYTES + 1]);

        let error = read_http_headers(&mut input).expect_err("headers should be too large");

        assert!(matches!(error, HeaderReadError::TooLarge));
    }

    #[test]
    fn parse_headers_normalizes_names_and_trims_values() {
        let headers = parse_headers(
            [
                "User-Agent:  RiotClient  ",
                "X-Riot-Entitlements-JWT: token",
            ]
            .into_iter(),
        );

        assert_eq!(
            headers.get("user-agent").map(String::as_str),
            Some("RiotClient")
        );
        assert_eq!(
            headers.get("x-riot-entitlements-jwt").map(String::as_str),
            Some("token")
        );
    }

    #[test]
    fn parse_headers_ignores_blank_values() {
        let headers = parse_headers(
            [
                "Authorization:",
                "X-Riot-Entitlements-JWT:   ",
                "User-Agent: Riot",
            ]
            .into_iter(),
        );

        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("x-riot-entitlements-jwt"));
        assert_eq!(headers.get("user-agent").map(String::as_str), Some("Riot"));
    }

    #[test]
    fn parse_headers_preserves_first_duplicate_value() {
        let headers = parse_headers(
            [
                "Authorization: Bearer real-token",
                "authorization: Bearer replacement",
                "Authorization:",
            ]
            .into_iter(),
        );

        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer real-token")
        );
    }

    #[test]
    fn parse_headers_uses_first_nonblank_duplicate_value() {
        let headers = parse_headers(
            [
                "Authorization:",
                "authorization:   ",
                "Authorization: Bearer real-token",
                "authorization: Bearer replacement",
            ]
            .into_iter(),
        );

        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer real-token")
        );
    }

    #[test]
    fn send_patched_server_skips_stopped_runtime() {
        let running = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();

        send_patched_server_if_running(
            &running,
            &tx,
            PatchedChatServer {
                host: "na2.chat.si.riotgames.com".to_string(),
                port: 5223,
                affinity: None,
            },
        );

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn send_patched_server_forwards_active_runtime() {
        let running = Arc::new(AtomicBool::new(true));
        let (tx, rx) = mpsc::channel();

        send_patched_server_if_running(
            &running,
            &tx,
            PatchedChatServer {
                host: "na2.chat.si.riotgames.com".to_string(),
                port: 5223,
                affinity: None,
            },
        );

        let server = rx.recv().expect("server should be sent");
        assert_eq!(server.host, "na2.chat.si.riotgames.com");
        assert_eq!(server.port, 5223);
    }

    #[test]
    fn rewrite_config_patches_chat_host_and_port() {
        let mut config = json!({
            "chat.host": "na2.chat.si.riotgames.com",
            "chat.port": 5223
        });

        let server = rewrite_config_with_affinity(&mut config, 49232, None)
            .expect("server should be detected");

        assert_eq!(server.host, "na2.chat.si.riotgames.com");
        assert_eq!(server.port, 5223);
        assert_eq!(config["chat.host"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.port"], 49232);
    }

    #[test]
    fn rewrite_config_accepts_string_chat_port() {
        let mut config = json!({
            "chat.host": "na2.chat.si.riotgames.com",
            "chat.port": "5223"
        });

        let server = rewrite_config_with_affinity(&mut config, 49232, None)
            .expect("server should be detected");

        assert_eq!(server.host, "na2.chat.si.riotgames.com");
        assert_eq!(server.port, 5223);
        assert_eq!(config["chat.host"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.port"], 49232);
    }

    #[test]
    fn rewrite_config_trims_chat_host_values() {
        let mut config = json!({
            "chat.host": "  na2.chat.si.riotgames.com  ",
            "chat.port": 5223
        });

        let server = rewrite_config_with_affinity(&mut config, 49232, None)
            .expect("server should be detected");

        assert_eq!(server.host, "na2.chat.si.riotgames.com");
        assert_eq!(server.port, 5223);
        assert_eq!(config["chat.host"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.port"], 49232);
    }

    #[test]
    fn rewrite_config_rejects_invalid_chat_port_values() {
        for port in [
            json!("not-a-port"),
            json!("0"),
            json!("70000"),
            json!(0_u64),
            json!(70000_u64),
        ] {
            let mut config = json!({
                "chat.host": "na2.chat.si.riotgames.com",
                "chat.port": port.clone()
            });
            let original = config.clone();

            assert!(rewrite_config_with_affinity(&mut config, 49232, None).is_none());
            assert_eq!(config, original);
        }
    }

    #[test]
    fn rewrite_config_rejects_blank_chat_host_values() {
        for host in [json!(""), json!("   ")] {
            let mut config = json!({
                "chat.host": host,
                "chat.port": 5223
            });
            let original = config.clone();

            assert!(rewrite_config_with_affinity(&mut config, 49232, None).is_none());
            assert_eq!(config, original);
        }
    }

    #[test]
    fn rewrite_config_leaves_config_unmodified_without_valid_server() {
        let mut config = json!({
            "chat.host": "   ",
            "chat.port": "0",
            "chat.affinities": {
                "na": "",
                "pbe": 2
            }
        });
        let original = config.clone();

        assert!(rewrite_config_with_affinity(&mut config, 49232, None).is_none());
        assert_eq!(config, original);
    }

    #[test]
    fn rewrite_config_prefers_configured_affinity_host() {
        let mut config = json!({
            "chat.host": "fallback.chat.si.riotgames.com",
            "chat.port": 5223,
            "chat.affinities": {
                "na": "na2.chat.si.riotgames.com",
                "pbe": "pbe1.chat.si.riotgames.com"
            }
        });

        let server = rewrite_config_with_affinity(&mut config, 49232, Some("na"))
            .expect("server should be detected");

        assert_eq!(server.host, "na2.chat.si.riotgames.com");
        assert_eq!(server.port, 5223);
        assert_eq!(config["chat.host"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.affinities"]["na"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.affinities"]["pbe"], LOCALHOST_DOMAIN);
    }

    #[test]
    fn rewrite_config_uses_chat_host_when_affinity_is_not_in_config() {
        let mut config = json!({
            "chat.host": "fallback.chat.si.riotgames.com",
            "chat.port": 5223,
            "chat.affinities": {
                "na": "na2.chat.si.riotgames.com",
                "pbe": "pbe1.chat.si.riotgames.com"
            }
        });

        let server = rewrite_config_with_affinity(&mut config, 49232, Some("missing"))
            .expect("server should be detected");

        assert_eq!(server.host, "fallback.chat.si.riotgames.com");
        assert_eq!(server.port, 5223);
        assert_eq!(config["chat.host"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.affinities"]["na"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.affinities"]["pbe"], LOCALHOST_DOMAIN);
    }

    #[test]
    fn rewrite_config_uses_config_host_even_when_affinity_name_differs() {
        let mut config = json!({
            "chat.host": "na2.chat.si.riotgames.com",
            "chat.port": 5223,
            "chat.affinities": {
                "na1": "na2.chat.si.riotgames.com"
            }
        });

        let server = rewrite_config_with_affinity(&mut config, 49232, Some("na1"))
            .expect("server should be detected");

        assert_eq!(server.host, "na2.chat.si.riotgames.com");
        assert_eq!(server.affinity.as_deref(), Some("na1"));
        assert_eq!(server.port, 5223);
        assert_eq!(config["chat.host"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.affinities"]["na1"], LOCALHOST_DOMAIN);
    }

    #[test]
    fn rewrite_config_falls_back_to_chat_host_for_invalid_affinity_name() {
        let mut config = json!({
            "chat.host": "fallback.chat.si.riotgames.com",
            "chat.port": 5223,
            "chat.affinities": {
                "na": "na2.chat.si.riotgames.com"
            }
        });

        let server = rewrite_config_with_affinity(&mut config, 49232, Some("bad.host"))
            .expect("server should be detected");

        assert_eq!(server.host, "fallback.chat.si.riotgames.com");
        assert_eq!(server.port, 5223);
        assert_eq!(config["chat.host"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.affinities"]["na"], LOCALHOST_DOMAIN);
    }

    #[test]
    fn rewrite_config_preserves_non_string_affinity_metadata() {
        let mut config = json!({
            "chat.host": "fallback.chat.si.riotgames.com",
            "chat.port": 5223,
            "chat.affinities": {
                "na": "na2.chat.si.riotgames.com",
                "enabled": true,
                "priority": 2,
                "metadata": { "source": "riot" }
            }
        });

        let server = rewrite_config_with_affinity(&mut config, 49232, Some("na"))
            .expect("server should be detected");

        assert_eq!(server.host, "na2.chat.si.riotgames.com");
        assert_eq!(config["chat.affinities"]["na"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.affinities"]["enabled"], true);
        assert_eq!(config["chat.affinities"]["priority"], 2);
        assert_eq!(
            config["chat.affinities"]["metadata"],
            json!({ "source": "riot" })
        );
    }

    #[test]
    fn rewrite_config_uses_any_affinity_when_chat_host_is_missing() {
        let mut config = json!({
            "chat.port": 5223,
            "chat.affinities": {
                "na": "na2.chat.si.riotgames.com"
            }
        });

        let server = rewrite_config_with_affinity(&mut config, 49232, None)
            .expect("server should be detected from affinities");

        assert_eq!(server.host, "na2.chat.si.riotgames.com");
        assert_eq!(server.port, 5223);
        assert_eq!(config.get("chat.host"), None);
        assert_eq!(config["chat.port"], 49232);
        assert_eq!(config["chat.affinities"]["na"], LOCALHOST_DOMAIN);
    }

    #[test]
    fn rewrite_config_skips_blank_affinity_hosts() {
        let mut config = json!({
            "chat.port": 5223,
            "chat.affinities": {
                "na": "",
                "pbe": "   ",
                "latam": "la1.chat.si.riotgames.com"
            }
        });

        let server = rewrite_config_with_affinity(&mut config, 49232, None)
            .expect("server should be detected from first nonblank affinity");

        assert_eq!(server.host, "la1.chat.si.riotgames.com");
        assert_eq!(server.port, 5223);
        assert_eq!(config["chat.affinities"]["na"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.affinities"]["pbe"], LOCALHOST_DOMAIN);
        assert_eq!(config["chat.affinities"]["latam"], LOCALHOST_DOMAIN);
    }

    #[test]
    fn rewrite_config_trims_affinity_hosts() {
        let mut config = json!({
            "chat.port": 5223,
            "chat.affinities": {
                "na": "  na2.chat.si.riotgames.com  "
            }
        });

        let server = rewrite_config_with_affinity(&mut config, 49232, None)
            .expect("server should be detected from affinity");

        assert_eq!(server.host, "na2.chat.si.riotgames.com");
        assert_eq!(server.port, 5223);
        assert_eq!(config["chat.affinities"]["na"], LOCALHOST_DOMAIN);
    }

    #[test]
    fn rewrite_config_keeps_unknown_config_unmodified() {
        let mut config = json!({
            "some.other.key": true
        });

        assert!(rewrite_config_with_affinity(&mut config, 49232, None).is_none());
        assert_eq!(config, json!({ "some.other.key": true }));
    }
}
