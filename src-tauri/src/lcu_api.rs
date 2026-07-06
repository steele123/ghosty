use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
};

use anyhow::{anyhow, Context, Result};
use reqwest::{Method, StatusCode};
use rusty_lcu::{
    generated::{self, models},
    Credentials, CredentialsSource, EndpointParams, Error as RustyLcuError, LcuClient,
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

static LCU_CREDENTIALS_CACHE: OnceLock<Mutex<Option<Credentials>>> = OnceLock::new();

pub async fn call_endpoint(
    method: &str,
    endpoint: &str,
    body: Option<Value>,
) -> Result<LcuApiResponse> {
    let method = normalize_method(method)?;
    let endpoint = normalize_endpoint(endpoint)?;
    let credentials = cached_credentials().await?;
    match call_endpoint_with_credentials(&method, &endpoint, body.clone(), &credentials).await {
        Ok(response) if !credentials_response_needs_refresh(&response) => Ok(response),
        Ok(_) | Err(_) => {
            clear_credentials_cache();
            let credentials = refresh_credentials().await?;
            call_endpoint_with_credentials(&method, &endpoint, body, &credentials).await
        }
    }
}

async fn call_endpoint_with_credentials(
    method: &str,
    endpoint: &str,
    body: Option<Value>,
    credentials: &Credentials,
) -> Result<LcuApiResponse> {
    let url = format!("{}{}", credentials.base_url(), endpoint);
    let method_value = Method::from_bytes(method.as_bytes())
        .with_context(|| format!("Unsupported League Client API method: {method}"))?;
    let client = LcuClient::with_credentials(credentials.clone())
        .context("Unable to build League Client API client")?;
    let mut params = EndpointParams::new();
    if let Some(body) = body.filter(|_| method != "GET" && method != "DELETE") {
        params = params.body(body)?;
    }

    match client.request(method_value, endpoint, params).await {
        Ok(body) => Ok(lcu_response_from_value(
            method,
            endpoint,
            &url,
            credentials.port,
            StatusCode::OK,
            body,
        )),
        Err(RustyLcuError::Lcu { status, body }) => Ok(LcuApiResponse {
            method: method.to_string(),
            endpoint: endpoint.to_string(),
            url,
            port: credentials.port,
            status: status.as_u16(),
            ok: false,
            body: serde_json::from_str(&body).ok(),
            text: body,
        }),
        Err(error) => Err(anyhow!(
            "Unable to call League Client API at {url}: {error}"
        )),
    }
}

async fn generated_client() -> Result<LcuClient> {
    let credentials = cached_credentials().await?;
    LcuClient::with_credentials(credentials).context("Unable to build League Client API client")
}

fn lcu_response_from_value(
    method: &str,
    endpoint: &str,
    url: &str,
    port: u16,
    status: StatusCode,
    body: Value,
) -> LcuApiResponse {
    let text = if body.is_null() {
        String::new()
    } else {
        serde_json::to_string(&body).unwrap_or_default()
    };
    LcuApiResponse {
        method: method.to_string(),
        endpoint: endpoint.to_string(),
        url: url.to_string(),
        port,
        status: status.as_u16(),
        ok: status.is_success(),
        body: Some(body),
        text,
    }
}

fn credentials_response_needs_refresh(response: &LcuApiResponse) -> bool {
    matches!(response.status, 401 | 403)
}

#[cfg(not(test))]
pub async fn gameflow_phase() -> Result<String> {
    let response = call_endpoint("GET", "/lol-gameflow/v1/gameflow-phase", None).await?;
    if let Some(Value::String(phase)) = response.body {
        return Ok(phase);
    }
    Ok(response.text.trim_matches('"').to_string())
}

#[cfg(not(test))]
pub async fn accept_ready_check() -> Result<LcuApiResponse> {
    call_endpoint(
        "POST",
        "/lol-matchmaking/v1/ready-check/accept",
        Some(serde_json::json!({})),
    )
    .await
}

#[cfg_attr(test, allow(dead_code))]
pub async fn current_summoner_opgg_link() -> Result<String> {
    let summoner = call_endpoint("GET", "/lol-summoner/v1/current-summoner", None).await?;
    let body = summoner
        .body
        .as_ref()
        .ok_or_else(|| anyhow!("League Client did not return current summoner JSON"))?;
    let (game_name, tag_line) = current_summoner_riot_id(body)?;
    let region = current_opgg_region()
        .await
        .unwrap_or_else(|_| "na".to_string());
    Ok(build_opgg_summoner_link(&region, &game_name, &tag_line))
}

#[cfg_attr(test, allow(dead_code))]
pub async fn current_summoner_display_name() -> Result<String> {
    let summoner = call_endpoint("GET", "/lol-summoner/v1/current-summoner", None).await?;
    let body = summoner
        .body
        .as_ref()
        .ok_or_else(|| anyhow!("League Client did not return current summoner JSON"))?;
    let (game_name, tag_line) = current_summoner_riot_id(body)?;
    Ok(format!("{game_name}#{tag_line}"))
}

#[cfg_attr(test, allow(dead_code))]
pub async fn current_lobby_opgg_multisearch_link() -> Result<String> {
    let region = current_opgg_region()
        .await
        .unwrap_or_else(|_| "na".to_string());
    let summoners = current_lobby_riot_ids().await?;
    if summoners.is_empty() {
        return Err(anyhow!(
            "Unable to find lobby members. Join a League lobby or champ select, then try again."
        ));
    }
    Ok(build_opgg_multisearch_link(&region, &summoners))
}

#[cfg_attr(test, allow(dead_code))]
pub async fn current_friends_summary() -> Result<String> {
    let response = call_endpoint("GET", "/lol-chat/v1/friends", None).await?;
    let friends = response
        .body
        .as_ref()
        .map(friends_from_value)
        .unwrap_or_default();
    Ok(format_friends_summary(&friends))
}

#[cfg_attr(test, allow(dead_code))]
async fn current_opgg_region() -> Result<String> {
    let response = call_endpoint("GET", "/riotclient/region-locale", None).await?;
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
async fn current_lobby_riot_ids() -> Result<Vec<(String, String)>> {
    if let Ok(session) = current_champ_select_session().await {
        let mut champ_select_ids = riot_ids_from_typed_champ_select_session(&session);
        if champ_select_ids.is_empty() {
            let summoner_ids = summoner_ids_from_typed_champ_select_session(&session);
            champ_select_ids = resolve_summoner_ids(&summoner_ids).await;
        }
        if !champ_select_ids.is_empty() {
            return Ok(champ_select_ids);
        }
    }

    if let Ok(lobby) = current_lobby().await {
        let mut lobby_ids = riot_ids_from_typed_lobby(&lobby);
        if lobby_ids.is_empty() {
            let summoner_ids = summoner_ids_from_typed_lobby(&lobby);
            lobby_ids = resolve_summoner_ids(&summoner_ids).await;
        }
        if !lobby_ids.is_empty() {
            return Ok(lobby_ids);
        }
    }

    let chat_participants = current_chat_participants()
        .await
        .map(|participants| riot_ids_from_typed_chat_participants(&participants))
        .unwrap_or_default();
    if !chat_participants.is_empty() {
        return Ok(chat_participants);
    }

    Ok(Vec::new())
}

async fn current_champ_select_session() -> Result<models::TeamBuilderDirectChampSelectSession> {
    let client = generated_client().await?;
    generated::get_lol_champ_select_v1_session_typed(&client, EndpointParams::new())
        .await
        .context("Unable to read League champ select session")
}

async fn current_lobby() -> Result<models::LolLobbyLobbyDto> {
    let client = generated_client().await?;
    generated::get_lol_lobby_v2_lobby_typed(&client, EndpointParams::new())
        .await
        .context("Unable to read League lobby")
}

async fn current_chat_participants() -> Result<models::LolChatParticipantList> {
    let client = generated_client().await?;
    client
        .get_as("/chat/v5/participants")
        .await
        .context("Unable to read League chat participants")
}

async fn resolve_summoner_ids(summoner_ids: &[u64]) -> Vec<(String, String)> {
    let mut ids = Vec::new();
    let Ok(client) = generated_client().await else {
        return ids;
    };
    for summoner_id in summoner_ids {
        let params = EndpointParams::new().path("id", *summoner_id);
        if let Ok(summoner) =
            generated::get_lol_summoner_v1_summoners_by_id_typed(&client, params).await
        {
            ids.push((summoner.game_name, summoner.tag_line));
        }
    }
    dedupe_riot_ids(&mut ids);
    ids
}

fn riot_ids_from_typed_champ_select_session(
    session: &models::TeamBuilderDirectChampSelectSession,
) -> Vec<(String, String)> {
    let mut ids = session
        .my_team
        .iter()
        .filter_map(riot_id_from_typed_champ_select_player)
        .collect::<Vec<_>>();
    dedupe_riot_ids(&mut ids);
    ids
}

fn riot_id_from_typed_champ_select_player(
    player: &models::TeamBuilderDirectChampSelectPlayerSelection,
) -> Option<(String, String)> {
    (!player.game_name.trim().is_empty() && !player.tag_line.trim().is_empty()).then(|| {
        (
            player.game_name.trim().to_string(),
            player.tag_line.trim().to_string(),
        )
    })
}

fn summoner_ids_from_typed_champ_select_session(
    session: &models::TeamBuilderDirectChampSelectSession,
) -> Vec<u64> {
    let mut ids = session
        .my_team
        .iter()
        .map(|member| member.summoner_id)
        .filter(|id| *id > 0)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn riot_ids_from_typed_lobby(lobby: &models::LolLobbyLobbyDto) -> Vec<(String, String)> {
    let mut ids = lobby
        .members
        .iter()
        .filter_map(riot_id_from_typed_lobby_member)
        .collect::<Vec<_>>();
    dedupe_riot_ids(&mut ids);
    ids
}

fn riot_id_from_typed_lobby_member(
    member: &models::LolLobbyLobbyParticipantDto,
) -> Option<(String, String)> {
    split_riot_id(&member.summoner_name)
}

fn summoner_ids_from_typed_lobby(lobby: &models::LolLobbyLobbyDto) -> Vec<u64> {
    let mut ids = lobby
        .members
        .iter()
        .map(|member| member.summoner_id)
        .filter(|id| *id > 0)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn riot_ids_from_typed_chat_participants(
    participants: &models::LolChatParticipantList,
) -> Vec<(String, String)> {
    let mut ids = participants
        .participants
        .iter()
        .filter(|participant| participant.cid.contains("champ-select"))
        .filter_map(riot_id_from_typed_chat_participant)
        .collect::<Vec<_>>();
    dedupe_riot_ids(&mut ids);
    ids
}

fn riot_id_from_typed_chat_participant(
    participant: &models::LolChatParticipant,
) -> Option<(String, String)> {
    if !participant.game_name.trim().is_empty() && !participant.game_tag.trim().is_empty() {
        return Some((
            participant.game_name.trim().to_string(),
            participant.game_tag.trim().to_string(),
        ));
    }
    split_riot_id(&participant.name)
}

#[cfg(test)]
fn riot_ids_from_champ_select_session(body: &Value) -> Vec<(String, String)> {
    let mut ids = body
        .get("myTeam")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(riot_id_from_value)
        .collect::<Vec<_>>();
    dedupe_riot_ids(&mut ids);
    ids
}

#[cfg(test)]
fn summoner_ids_from_champ_select_session(body: &Value) -> Vec<u64> {
    summoner_ids_from_array_field(body, "myTeam")
}

#[cfg(test)]
fn summoner_ids_from_lobby(body: &Value) -> Vec<u64> {
    summoner_ids_from_array_field(body, "members")
}

#[cfg(test)]
fn summoner_ids_from_array_field(body: &Value, field: &str) -> Vec<u64> {
    let mut ids = body
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(summoner_id_from_value)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

#[cfg(test)]
fn summoner_id_from_value(body: &Value) -> Option<u64> {
    body.get("summonerId")
        .or_else(|| body.get("summoner_id"))
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
        })
}

fn riot_id_from_value(body: &Value) -> Option<(String, String)> {
    if let (Some(game_name), Some(tag_line)) = (
        string_field(body, "gameName")
            .or_else(|| string_field(body, "game_name"))
            .or_else(|| string_field(body, "riotIdGameName"))
            .or_else(|| string_field(body, "riot_id_game_name")),
        string_field(body, "tagLine")
            .or_else(|| string_field(body, "tagline"))
            .or_else(|| string_field(body, "gameTag"))
            .or_else(|| string_field(body, "game_tag"))
            .or_else(|| string_field(body, "riotIdTagLine"))
            .or_else(|| string_field(body, "riotIdTagline"))
            .or_else(|| string_field(body, "riot_id_tag_line"))
            .or_else(|| string_field(body, "riot_id_tagline")),
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
        "https://op.gg/lol/multisearch?summoners={}&region={}",
        percent_encode_query_component(&summoners),
        opgg_region_slug(region)
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

async fn cached_credentials() -> Result<Credentials> {
    if let Some(credentials) = credentials_cache()
        .lock()
        .map_err(|e| anyhow!("Unable to read League Client credentials cache: {e}"))?
        .clone()
    {
        return Ok(credentials);
    }
    refresh_credentials().await
}

async fn refresh_credentials() -> Result<Credentials> {
    let credentials = Credentials::discover(CredentialsSource::Auto)
        .await
        .map_err(|error| {
            anyhow!(
                "Unable to find League Client API credentials. Start League Client, then try again. ({error})"
            )
        })?;
    *credentials_cache()
        .lock()
        .map_err(|e| anyhow!("Unable to update League Client credentials cache: {e}"))? =
        Some(credentials.clone());
    Ok(credentials)
}

fn clear_credentials_cache() {
    if let Ok(mut cache) = credentials_cache().lock() {
        *cache = None;
    }
}

fn credentials_cache() -> &'static Mutex<Option<Credentials>> {
    LCU_CREDENTIALS_CACHE.get_or_init(|| Mutex::new(None))
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
        build_opgg_multisearch_link, build_opgg_summoner_link, current_summoner_riot_id,
        format_friends_summary, friends_from_value, lcu_response_from_value, normalize_endpoint,
        normalize_method, opgg_region_slug, riot_ids_from_champ_select_session,
        riot_ids_from_typed_chat_participants, summoner_ids_from_champ_select_session,
        summoner_ids_from_lobby,
    };
    use reqwest::StatusCode;
    use rusty_lcu::generated::models;
    use serde_json::json;

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
    fn response_wrapper_preserves_debug_fields() {
        let response = lcu_response_from_value(
            "GET",
            "/lol-test/v1/example",
            "https://127.0.0.1:1234/lol-test/v1/example",
            1234,
            StatusCode::OK,
            json!({ "ok": true }),
        );

        assert_eq!(response.method, "GET");
        assert_eq!(response.endpoint, "/lol-test/v1/example");
        assert_eq!(response.port, 1234);
        assert_eq!(response.status, 200);
        assert!(response.ok);
        assert_eq!(response.body, Some(json!({ "ok": true })));
        assert_eq!(response.text, r#"{"ok":true}"#);
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
        let value = models::LolChatParticipantList {
            participants: vec![
                chat_participant("lol-champ-select-1", "Ghosty", "NA1", "ignored"),
                chat_participant("lol-champ-select-1", "", "", "Duo User#NA2"),
                chat_participant("other-chat", "Ignored", "NA3", "Ignored#NA3"),
            ],
        };

        let ids = riot_ids_from_typed_chat_participants(&value);

        assert_eq!(
            ids,
            vec![
                ("Ghosty".to_string(), "NA1".to_string()),
                ("Duo User".to_string(), "NA2".to_string())
            ]
        );
    }

    fn chat_participant(
        cid: &str,
        game_name: &str,
        game_tag: &str,
        name: &str,
    ) -> models::LolChatParticipant {
        models::LolChatParticipant {
            cid: cid.to_string(),
            game_name: game_name.to_string(),
            game_tag: game_tag.to_string(),
            muted: false,
            name: name.to_string(),
            pid: String::new(),
            puuid: String::new(),
            region: "NA1".to_string(),
        }
    }

    #[test]
    fn reads_champ_select_riot_ids_from_session() {
        let value = json!({
            "myTeam": [
                {
                    "riotIdGameName": "Ghosty",
                    "riotIdTagLine": "NA1",
                    "summonerId": 123
                },
                {
                    "gameName": "Duo User",
                    "tagLine": "NA2",
                    "summonerId": 456
                }
            ],
            "theirTeam": [
                {
                    "riotIdGameName": "Enemy",
                    "riotIdTagLine": "NA3",
                    "summonerId": 789
                }
            ]
        });

        assert_eq!(
            riot_ids_from_champ_select_session(&value),
            vec![
                ("Ghosty".to_string(), "NA1".to_string()),
                ("Duo User".to_string(), "NA2".to_string())
            ]
        );
        assert_eq!(
            summoner_ids_from_champ_select_session(&value),
            vec![123, 456]
        );
    }

    #[test]
    fn reads_lobby_summoner_ids() {
        let value = json!({
            "members": [
                { "summonerId": 456 },
                { "summonerId": "123" },
                { "summonerId": 456 }
            ]
        });

        assert_eq!(summoner_ids_from_lobby(&value), vec![123, 456]);
    }

    #[test]
    fn builds_opgg_multisearch_link() {
        let ids = vec![
            ("Ghosty".to_string(), "NA1".to_string()),
            ("Duo User".to_string(), "N#2".to_string()),
        ];

        assert_eq!(
            build_opgg_multisearch_link("NA1", &ids),
            "https://op.gg/lol/multisearch?summoners=Ghosty%23NA1%2CDuo%20User%23N%232&region=na"
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
