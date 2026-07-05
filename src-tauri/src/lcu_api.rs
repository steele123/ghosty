use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::{
    blocking::{Client, RequestBuilder},
    header,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sysinfo::System;

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

static LCU_AUTH_CACHE: OnceLock<Mutex<Option<LcuAuthInfo>>> = OnceLock::new();

pub fn call_endpoint(method: &str, endpoint: &str, body: Option<Value>) -> Result<LcuApiResponse> {
    let method = normalize_method(method)?;
    let endpoint = normalize_endpoint(endpoint)?;
    let auth = cached_auth_info()?;
    match call_endpoint_with_auth(&method, &endpoint, body.clone(), &auth) {
        Ok(response) if !auth_response_needs_refresh(&response) => Ok(response),
        Ok(_) | Err(_) => {
            clear_auth_cache();
            let auth = refresh_auth_info()?;
            call_endpoint_with_auth(&method, &endpoint, body, &auth)
        }
    }
}

fn call_endpoint_with_auth(
    method: &str,
    endpoint: &str,
    body: Option<Value>,
    auth: &LcuAuthInfo,
) -> Result<LcuApiResponse> {
    let url = format!("https://127.0.0.1:{}{endpoint}", auth.port);
    let client = build_client(auth)?;
    let request = request_builder(&client, method, &url, body)?;
    let response = request
        .send()
        .with_context(|| format!("Unable to call League Client API at {url}"))?;
    let status = response.status();
    let text = response
        .text()
        .context("Unable to read League Client API response")?;
    let body = serde_json::from_str(&text).ok();

    Ok(LcuApiResponse {
        method: method.to_string(),
        endpoint: endpoint.to_string(),
        url,
        port: auth.port,
        status: status.as_u16(),
        ok: status.is_success(),
        body,
        text,
    })
}

fn auth_response_needs_refresh(response: &LcuApiResponse) -> bool {
    matches!(response.status, 401 | 403)
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

#[cfg_attr(test, allow(dead_code))]
pub fn current_summoner_opgg_link() -> Result<String> {
    let summoner = call_endpoint("GET", "/lol-summoner/v1/current-summoner", None)?;
    let body = summoner
        .body
        .as_ref()
        .ok_or_else(|| anyhow!("League Client did not return current summoner JSON"))?;
    let (game_name, tag_line) = current_summoner_riot_id(body)?;
    let region = current_opgg_region().unwrap_or_else(|_| "na".to_string());
    Ok(build_opgg_summoner_link(&region, &game_name, &tag_line))
}

#[cfg_attr(test, allow(dead_code))]
pub fn current_summoner_display_name() -> Result<String> {
    let summoner = call_endpoint("GET", "/lol-summoner/v1/current-summoner", None)?;
    let body = summoner
        .body
        .as_ref()
        .ok_or_else(|| anyhow!("League Client did not return current summoner JSON"))?;
    let (game_name, tag_line) = current_summoner_riot_id(body)?;
    Ok(format!("{game_name}#{tag_line}"))
}

#[cfg_attr(test, allow(dead_code))]
pub fn current_lobby_opgg_multisearch_link() -> Result<String> {
    let region = current_opgg_region().unwrap_or_else(|_| "na".to_string());
    let summoners = current_lobby_riot_ids()?;
    if summoners.is_empty() {
        return Err(anyhow!(
            "Unable to find lobby members. Join a League lobby or champ select, then try again."
        ));
    }
    Ok(build_opgg_multisearch_link(&region, &summoners))
}

#[cfg_attr(test, allow(dead_code))]
pub fn current_friends_summary() -> Result<String> {
    let response = call_endpoint("GET", "/lol-chat/v1/friends", None)?;
    let friends = response
        .body
        .as_ref()
        .map(friends_from_value)
        .unwrap_or_default();
    Ok(format_friends_summary(&friends))
}

#[cfg_attr(test, allow(dead_code))]
fn current_opgg_region() -> Result<String> {
    let response = call_endpoint("GET", "/riotclient/region-locale", None)?;
    let region = response
        .body
        .as_ref()
        .and_then(|body| string_field(body, "region").or_else(|| string_field(body, "webRegion")))
        .ok_or_else(|| anyhow!("League Client did not return a region"))?;
    Ok(opgg_region_slug(region))
}

fn current_summoner_riot_id(body: &Value) -> Result<(String, String)> {
    riot_id_from_value(body).ok_or_else(|| {
        anyhow!("Unable to read your Riot ID from League Client. Open your profile once, then try again.")
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FriendSummaryEntry {
    name: String,
    availability: String,
    product: String,
}

fn friends_from_value(body: &Value) -> Vec<FriendSummaryEntry> {
    let friends = body
        .as_array()
        .or_else(|| body.get("friends").and_then(Value::as_array))
        .into_iter()
        .flatten();
    friends.filter_map(friend_from_value).collect()
}

fn friend_from_value(friend: &Value) -> Option<FriendSummaryEntry> {
    let name = friend_display_name(friend)?;
    let availability = string_field(friend, "availability")
        .or_else(|| string_field(friend, "show"))
        .unwrap_or("unknown")
        .trim();
    let product = string_field(friend, "product")
        .or_else(|| string_field(friend, "productName"))
        .or_else(|| string_field(friend, "game"))
        .unwrap_or("unknown")
        .trim();
    Some(FriendSummaryEntry {
        name,
        availability: normalize_friend_availability(availability),
        product: normalize_friend_product(product),
    })
}

fn friend_display_name(friend: &Value) -> Option<String> {
    if let Some((game_name, tag_line)) = riot_id_from_value(friend) {
        return Some(format!("{game_name}#{tag_line}"));
    }
    string_field(friend, "displayName")
        .or_else(|| string_field(friend, "name"))
        .or_else(|| string_field(friend, "summonerName"))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_friend_availability(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "chat" | "online" => "online",
        "away" => "away",
        "dnd" | "busy" => "dnd",
        "mobile" => "mobile",
        "offline" => "offline",
        "unknown" | "" => "unknown",
        other => other,
    }
    .to_string()
}

fn normalize_friend_product(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "league_of_legends" | "lol" => "League",
        "valorant" => "VALORANT",
        "riot_mobile" | "mobile" => "Riot Mobile",
        "bacon" | "lor" => "Runeterra",
        "keystone" | "riot_client" => "Riot Client",
        "unknown" | "" => "Unknown",
        other => other,
    }
    .to_string()
}

fn format_friends_summary(friends: &[FriendSummaryEntry]) -> String {
    if friends.is_empty() {
        return "Friends: 0 found. League Client did not return any friends.".to_string();
    }

    let mut availability_counts = BTreeMap::<String, usize>::new();
    let mut product_counts = BTreeMap::<String, usize>::new();
    for friend in friends {
        *availability_counts
            .entry(friend.availability.clone())
            .or_default() += 1;
        *product_counts.entry(friend.product.clone()).or_default() += 1;
    }

    let status_order = ["online", "dnd", "away", "mobile", "offline", "unknown"];
    let status_summary = ordered_counts(&availability_counts, &status_order);
    let product_summary = product_counts
        .iter()
        .map(|(product, count)| format!("{product} {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Friends: {} total. Statuses: {}. Products: {}.",
        friends.len(),
        status_summary,
        product_summary
    )
}

fn ordered_counts(counts: &BTreeMap<String, usize>, order: &[&str]) -> String {
    let mut parts = Vec::new();
    for key in order {
        if let Some(count) = counts.get(*key) {
            parts.push(format!("{key} {count}"));
        }
    }
    for (key, count) in counts {
        if !order.iter().any(|ordered| ordered == key) {
            parts.push(format!("{key} {count}"));
        }
    }
    parts.join(", ")
}

#[cfg_attr(test, allow(dead_code))]
fn current_lobby_riot_ids() -> Result<Vec<(String, String)>> {
    let chat_participants = call_endpoint("GET", "/chat/v5/participants", None)
        .ok()
        .and_then(|response| response.body)
        .map(|body| riot_ids_from_chat_participants(&body))
        .unwrap_or_default();
    if !chat_participants.is_empty() {
        return Ok(chat_participants);
    }

    let lobby = call_endpoint("GET", "/lol-lobby/v2/lobby", None)?;
    Ok(lobby
        .body
        .as_ref()
        .map(riot_ids_from_lobby)
        .unwrap_or_default())
}

fn riot_ids_from_chat_participants(body: &Value) -> Vec<(String, String)> {
    let Some(participants) = body.get("participants").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut ids = participants
        .iter()
        .filter(|participant| {
            string_field(participant, "cid")
                .map(|cid| cid.contains("champ-select"))
                .unwrap_or(false)
        })
        .filter_map(riot_id_from_value)
        .collect::<Vec<_>>();
    dedupe_riot_ids(&mut ids);
    ids
}

#[cfg_attr(test, allow(dead_code))]
fn riot_ids_from_lobby(body: &Value) -> Vec<(String, String)> {
    let Some(members) = body.get("members").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut ids = members
        .iter()
        .filter_map(riot_id_from_value)
        .collect::<Vec<_>>();
    dedupe_riot_ids(&mut ids);
    ids
}

fn riot_id_from_value(body: &Value) -> Option<(String, String)> {
    if let (Some(game_name), Some(tag_line)) = (
        string_field(body, "gameName").or_else(|| string_field(body, "game_name")),
        string_field(body, "tagLine")
            .or_else(|| string_field(body, "tagline"))
            .or_else(|| string_field(body, "gameTag"))
            .or_else(|| string_field(body, "game_tag")),
    ) {
        if !game_name.trim().is_empty() && !tag_line.trim().is_empty() {
            return Some((game_name.trim().to_string(), tag_line.trim().to_string()));
        }
    }

    string_field(body, "displayName")
        .or_else(|| string_field(body, "summonerName"))
        .or_else(|| string_field(body, "name"))
        .and_then(split_riot_id)
}

fn dedupe_riot_ids(ids: &mut Vec<(String, String)>) {
    let mut seen = Vec::<String>::new();
    ids.retain(|(game_name, tag_line)| {
        let key = format!(
            "{}#{}",
            game_name.to_ascii_lowercase(),
            tag_line.to_ascii_lowercase()
        );
        if seen.iter().any(|seen| seen == &key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
}

fn string_field<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key)?.as_str()
}

fn split_riot_id(value: &str) -> Option<(String, String)> {
    let (game_name, tag_line) = value.rsplit_once('#')?;
    let game_name = game_name.trim();
    let tag_line = tag_line.trim();
    (!game_name.is_empty() && !tag_line.is_empty())
        .then(|| (game_name.to_string(), tag_line.to_string()))
}

fn build_opgg_summoner_link(region: &str, game_name: &str, tag_line: &str) -> String {
    format!(
        "https://www.op.gg/summoners/{}/{}-{}",
        opgg_region_slug(region),
        percent_encode_path_segment(game_name.trim()),
        percent_encode_path_segment(tag_line.trim())
    )
}

fn build_opgg_multisearch_link(region: &str, summoners: &[(String, String)]) -> String {
    let summoners = summoners
        .iter()
        .map(|(game_name, tag_line)| format!("{}#{}", game_name.trim(), tag_line.trim()))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "https://op.gg/lol/multisearch/{}?summoners={}",
        opgg_region_slug(region),
        percent_encode_query_component(&summoners)
    )
}

fn opgg_region_slug(region: &str) -> String {
    let normalized = region.trim().to_ascii_uppercase();
    match normalized.as_str() {
        "BR" | "BR1" => "br",
        "EUN" | "EUN1" | "EUNE" => "eune",
        "EUW" | "EUW1" => "euw",
        "JP" | "JP1" => "jp",
        "KR" => "kr",
        "LA1" | "LAN" => "lan",
        "LA2" | "LAS" => "las",
        "NA" | "NA1" => "na",
        "OC" | "OC1" | "OCE" => "oce",
        "PBE" | "PBE1" => "pbe",
        "RU" => "ru",
        "TR" | "TR1" => "tr",
        other => return other.to_ascii_lowercase(),
    }
    .to_string()
}

fn percent_encode_path_segment(value: &str) -> String {
    value.bytes().fold(String::new(), |mut output, byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
        output
    })
}

fn percent_encode_query_component(value: &str) -> String {
    value.bytes().fold(String::new(), |mut output, byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
        output
    })
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

fn cached_auth_info() -> Result<LcuAuthInfo> {
    if let Some(auth) = auth_cache()
        .lock()
        .map_err(|e| anyhow!("Unable to read League Client auth cache: {e}"))?
        .clone()
    {
        return Ok(auth);
    }
    refresh_auth_info()
}

fn refresh_auth_info() -> Result<LcuAuthInfo> {
    let auth = find_auth_info()?;
    *auth_cache()
        .lock()
        .map_err(|e| anyhow!("Unable to update League Client auth cache: {e}"))? = Some(auth.clone());
    Ok(auth)
}

fn clear_auth_cache() {
    if let Ok(mut cache) = auth_cache().lock() {
        *cache = None;
    }
}

fn auth_cache() -> &'static Mutex<Option<LcuAuthInfo>> {
    LCU_AUTH_CACHE.get_or_init(|| Mutex::new(None))
}

fn league_client_command_lines() -> Result<Vec<String>> {
    Ok(System::new_all()
        .processes()
        .values()
        .filter(|process| {
            process
                .name()
                .to_string_lossy()
                .eq_ignore_ascii_case("LeagueClientUx.exe")
        })
        .map(|process| {
            process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|line| !line.trim().is_empty())
        .collect())
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
        auth_info_from_command_line, build_opgg_multisearch_link, build_opgg_summoner_link,
        current_summoner_riot_id, format_friends_summary, friends_from_value, normalize_endpoint,
        normalize_method, opgg_region_slug, parse_arg_value, riot_ids_from_chat_participants,
    };
    use serde_json::json;

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
    fn reads_current_summoner_riot_id_from_game_name_and_tag_line() {
        let value = json!({
            "gameName": "Ghosty User",
            "tagLine": "NA1",
            "displayName": "ignored"
        });

        let (game_name, tag_line) = current_summoner_riot_id(&value).unwrap();

        assert_eq!(game_name, "Ghosty User");
        assert_eq!(tag_line, "NA1");
    }

    #[test]
    fn reads_current_summoner_riot_id_from_display_name_fallback() {
        let value = json!({
            "displayName": "Ghosty#EUW"
        });

        let (game_name, tag_line) = current_summoner_riot_id(&value).unwrap();

        assert_eq!(game_name, "Ghosty");
        assert_eq!(tag_line, "EUW");
    }

    #[test]
    fn builds_opgg_link_with_region_mapping_and_path_encoding() {
        assert_eq!(opgg_region_slug("NA1"), "na");
        assert_eq!(opgg_region_slug("EUW1"), "euw");
        assert_eq!(
            build_opgg_summoner_link("NA1", "Ghosty User", "N#1"),
            "https://www.op.gg/summoners/na/Ghosty%20User-N%231"
        );
    }

    #[test]
    fn reads_champ_select_riot_ids_from_chat_participants() {
        let value = json!({
            "participants": [
                {
                    "cid": "lol-champ-select-1",
                    "game_name": "Ghosty",
                    "game_tag": "NA1"
                },
                {
                    "cid": "lol-champ-select-1",
                    "gameName": "Duo User",
                    "tagLine": "NA2"
                },
                {
                    "cid": "other-chat",
                    "game_name": "Ignored",
                    "game_tag": "NA3"
                }
            ]
        });

        let ids = riot_ids_from_chat_participants(&value);

        assert_eq!(
            ids,
            vec![
                ("Ghosty".to_string(), "NA1".to_string()),
                ("Duo User".to_string(), "NA2".to_string())
            ]
        );
    }

    #[test]
    fn builds_opgg_multisearch_link() {
        let ids = vec![
            ("Ghosty".to_string(), "NA1".to_string()),
            ("Duo User".to_string(), "N#2".to_string()),
        ];

        assert_eq!(
            build_opgg_multisearch_link("NA1", &ids),
            "https://op.gg/lol/multisearch/na?summoners=Ghosty%23NA1%2CDuo%20User%23N%232"
        );
    }

    #[test]
    fn summarizes_lol_chat_friends() {
        let value = json!([
            {
                "gameName": "Ghosty",
                "tagLine": "NA1",
                "availability": "chat",
                "product": "league_of_legends"
            },
            {
                "displayName": "Mobile Pal#NA2",
                "availability": "mobile",
                "product": "riot_mobile"
            },
            {
                "name": "Offline Friend",
                "availability": "offline"
            }
        ]);

        let friends = friends_from_value(&value);
        let summary = format_friends_summary(&friends);

        assert_eq!(
            summary,
            "Friends: 3 total. Statuses: online 1, mobile 1, offline 1. Products: League 1, Riot Mobile 1, Unknown 1."
        );
    }

    #[test]
    fn summarizes_empty_friends_list() {
        assert_eq!(
            format_friends_summary(&[]),
            "Friends: 0 found. League Client did not return any friends."
        );
    }
}
