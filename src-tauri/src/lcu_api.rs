use std::{process::Command, time::Duration};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::{
    blocking::{Client, RequestBuilder},
    header,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LcuApiResponse {
    pub method: String,
    pub endpoint: String,
    pub url: String,
    pub port: u16,
    pub status: u16,
    pub ok: bool,
    pub body: Option<Value>,
    pub text: String,
}

#[derive(Debug, Clone)]
struct LcuAuthInfo {
    port: u16,
    password: String,
}

pub fn call_endpoint(method: &str, endpoint: &str, body: Option<Value>) -> Result<LcuApiResponse> {
    let method = normalize_method(method)?;
    let endpoint = normalize_endpoint(endpoint)?;
    let auth = find_auth_info()?;
    let url = format!("https://127.0.0.1:{}{endpoint}", auth.port);
    let client = build_client(&auth)?;
    let request = request_builder(&client, &method, &url, body)?;
    let response = request
        .send()
        .with_context(|| format!("Unable to call League Client API at {url}"))?;
    let status = response.status();
    let text = response
        .text()
        .context("Unable to read League Client API response")?;
    let body = serde_json::from_str(&text).ok();

    Ok(LcuApiResponse {
        method,
        endpoint,
        url,
        port: auth.port,
        status: status.as_u16(),
        ok: status.is_success(),
        body,
        text,
    })
}

#[cfg(not(test))]
pub fn gameflow_phase() -> Result<String> {
    let response = call_endpoint("GET", "/lol-gameflow/v1/gameflow-phase", None)?;
    if let Some(Value::String(phase)) = response.body {
        return Ok(phase);
    }
    Ok(response.text.trim_matches('"').to_string())
}

#[cfg(not(test))]
pub fn accept_ready_check() -> Result<LcuApiResponse> {
    call_endpoint(
        "POST",
        "/lol-matchmaking/v1/ready-check/accept",
        Some(serde_json::json!({})),
    )
}

fn build_client(auth: &LcuAuthInfo) -> Result<Client> {
    let mut headers = header::HeaderMap::new();
    let encoded = STANDARD.encode(format!("riot:{}", auth.password));
    let value = header::HeaderValue::from_str(&format!("Basic {encoded}"))
        .context("Unable to build League Client authorization header")?;
    headers.insert(header::AUTHORIZATION, value);

    Client::builder()
        .danger_accept_invalid_certs(true)
        .default_headers(headers)
        .timeout(Duration::from_secs(6))
        .build()
        .context("Unable to build League Client API client")
}

fn request_builder(
    client: &Client,
    method: &str,
    url: &str,
    body: Option<Value>,
) -> Result<RequestBuilder> {
    let request = match method {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "PATCH" => client.patch(url),
        "DELETE" => client.delete(url),
        _ => return Err(anyhow!("Unsupported League Client API method: {method}")),
    };

    Ok(match body {
        Some(body) if method != "GET" && method != "DELETE" => request.json(&body),
        _ => request,
    })
}

fn find_auth_info() -> Result<LcuAuthInfo> {
    league_client_command_lines()?
        .into_iter()
        .find_map(|line| auth_info_from_command_line(&line))
        .ok_or_else(|| {
            anyhow!(
                "Unable to find LeagueClientUx.exe with --app-port and --remoting-auth-token. Start League Client, then try again."
            )
        })
}

fn league_client_command_lines() -> Result<Vec<String>> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-CimInstance Win32_Process -Filter \"Name = 'LeagueClientUx.exe'\").CommandLine | ConvertTo-Json -Compress",
        ])
        .output()
        .context("Unable to query LeagueClientUx.exe command line")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "Unable to query LeagueClientUx.exe command line: {}",
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Ok(Vec::new());
    }
    parse_command_line_json(&stdout)
}

fn parse_command_line_json(output: &str) -> Result<Vec<String>> {
    let value: Value =
        serde_json::from_str(output).context("Unable to parse LeagueClientUx command line JSON")?;
    Ok(match value {
        Value::String(line) => vec![line],
        Value::Array(lines) => lines
            .into_iter()
            .filter_map(|line| line.as_str().map(ToOwned::to_owned))
            .collect(),
        Value::Null => Vec::new(),
        _ => {
            return Err(anyhow!(
                "Unexpected LeagueClientUx command line query output: {output}"
            ))
        }
    })
}

fn auth_info_from_command_line(command_line: &str) -> Option<LcuAuthInfo> {
    let port = parse_arg_value(command_line, "--app-port=")?
        .parse::<u16>()
        .ok()?;
    let password = parse_arg_value(command_line, "--remoting-auth-token=")
        .or_else(|| parse_arg_value(command_line, "--riotclient-auth-token="))?;
    Some(LcuAuthInfo { port, password })
}

fn parse_arg_value(command_line: &str, prefix: &str) -> Option<String> {
    let start = command_line.find(prefix)? + prefix.len();
    let rest = &command_line[start..];
    let rest = rest.trim_start_matches('"');
    let value: String = rest
        .chars()
        .take_while(|ch| !ch.is_whitespace() && *ch != '"')
        .collect();
    (!value.is_empty()).then_some(value)
}

fn normalize_method(method: &str) -> Result<String> {
    let method = method.trim().to_ascii_uppercase();
    match method.as_str() {
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" => Ok(method),
        _ => Err(anyhow!("Method must be GET, POST, PUT, PATCH, or DELETE.")),
    }
}

fn normalize_endpoint(endpoint: &str) -> Result<String> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(anyhow!("Choose a League Client API endpoint first."));
    }
    if endpoint.starts_with("http://")
        || endpoint.starts_with("https://")
        || endpoint.starts_with("//")
        || endpoint.contains('\\')
        || endpoint.contains("..")
        || endpoint.contains('#')
    {
        return Err(anyhow!(
            "Only relative League Client API endpoints are allowed."
        ));
    }

    let endpoint = if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{endpoint}")
    };
    if endpoint.starts_with("/lol-")
        || endpoint.starts_with("/riotclient/")
        || endpoint.starts_with("/rso-auth/")
        || endpoint.starts_with("/entitlements/")
        || endpoint.starts_with("/chat/")
    {
        Ok(endpoint)
    } else {
        Err(anyhow!(
            "Endpoint must be a relative League Client API path, like /lol-summoner/v1/current-summoner."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        auth_info_from_command_line, normalize_endpoint, normalize_method, parse_arg_value,
        parse_command_line_json,
    };

    #[test]
    fn parses_lcu_auth_from_command_line() {
        let line = r#""C:\Riot Games\League of Legends\LeagueClientUx.exe" "--app-port=53122" "--remoting-auth-token=secret-token" --no-proxy-server"#;
        let auth = auth_info_from_command_line(line).expect("auth should parse");
        assert_eq!(auth.port, 53122);
        assert_eq!(auth.password, "secret-token");
    }

    #[test]
    fn parses_unquoted_arg_values() {
        assert_eq!(
            parse_arg_value("--app-port=12345 --remoting-auth-token=abc", "--app-port="),
            Some("12345".to_string())
        );
    }

    #[test]
    fn normalizes_lcu_endpoints_and_methods() {
        assert_eq!(normalize_method("patch").unwrap(), "PATCH");
        assert_eq!(
            normalize_endpoint("lol-summoner/v1/current-summoner").unwrap(),
            "/lol-summoner/v1/current-summoner"
        );
        assert_eq!(
            normalize_endpoint("/lol-chat/v1/friends?foo=bar").unwrap(),
            "/lol-chat/v1/friends?foo=bar"
        );
    }

    #[test]
    fn rejects_non_lcu_targets() {
        assert!(normalize_endpoint("https://127.0.0.1:2999/Help").is_err());
        assert!(normalize_endpoint("/GetLiveclientdataAllgamedata").is_err());
        assert!(normalize_endpoint("../lol-chat/v1/friends").is_err());
        assert!(normalize_endpoint("/lol-chat/v1/friends#fragment").is_err());
        assert!(normalize_method("TRACE").is_err());
    }

    #[test]
    fn parses_powershell_json_shapes() {
        assert_eq!(
            parse_command_line_json(r#""one command line""#).unwrap(),
            vec!["one command line"]
        );
        assert_eq!(
            parse_command_line_json(r#"["one","two"]"#).unwrap(),
            vec!["one", "two"]
        );
        assert!(parse_command_line_json("null").unwrap().is_empty());
    }
}
