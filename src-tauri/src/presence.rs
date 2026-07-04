use std::io::Cursor;

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::Value;
use xmltree::{Element, EmitterConfig, ParserConfig, XMLNode};

use crate::models::PresenceStatus;

const HELPER_PUUID: &str = "41c322a1-b328-495b-a004-5ccd3e45eae8";
const HELPER_NAME: &str = "Ghosty Active!";
const HELPER_PROFILE_ICON: &str = "784";
pub const ROSTER_NAMESPACE: &str = "jabber:iq:riotgames:roster";

pub enum PresenceRewrite {
    Forward(String),
    Drop,
}

pub fn rewrite_presence_fragment(
    content: &str,
    enabled: bool,
    target_status: PresenceStatus,
    connect_to_muc: bool,
    valorant_version: &mut Option<String>,
) -> Result<Option<PresenceRewrite>> {
    if !enabled || !content.contains("<presence") {
        return Ok(None);
    }

    let wrapped = format!("<xml>{content}</xml>");
    let Ok(mut root) = Element::parse_with_config(
        Cursor::new(wrapped.as_bytes()),
        ParserConfig::new().whitespace_to_characters(true),
    ) else {
        return Ok(None);
    };
    let mut rewritten = Vec::new();
    let mut element_count = 0;
    let mut presence_count = 0;

    for node in root.children.iter_mut() {
        match node {
            XMLNode::Element(element) => {
                element_count += 1;
                if element.name != "presence" {
                    rewritten.push(serialize_element(element)?);
                    continue;
                }

                presence_count += 1;
                if element.attributes.contains_key("to") {
                    if connect_to_muc {
                        rewritten.push(serialize_element(element)?);
                    } else {
                        rewritten.push(String::new());
                    }
                    continue;
                }

                rewrite_presence(element, target_status, valorant_version);
                rewritten.push(serialize_element(element)?);
            }
            XMLNode::Text(text) | XMLNode::CData(text) => rewritten.push(text.clone()),
            _ => {}
        }
    }

    if element_count == 0 || presence_count == 0 {
        return Ok(None);
    }

    let rewritten = rewritten.join("");
    if rewritten.is_empty() {
        Ok(Some(PresenceRewrite::Drop))
    } else {
        Ok(Some(PresenceRewrite::Forward(rewritten)))
    }
}

pub fn rewrite_unaddressed_presence_only_fragment(
    content: &str,
    target_status: PresenceStatus,
    valorant_version: &mut Option<String>,
) -> Result<Option<Vec<u8>>> {
    if !content.contains("<presence") {
        return Ok(None);
    }

    let wrapped = format!("<xml>{content}</xml>");
    let Ok(mut root) = Element::parse_with_config(
        Cursor::new(wrapped.as_bytes()),
        ParserConfig::new().whitespace_to_characters(true),
    ) else {
        return Ok(None);
    };

    let mut rewritten = Vec::new();
    for node in root.children.iter_mut() {
        let XMLNode::Element(element) = node else {
            continue;
        };
        if element.name != "presence" || element.attributes.contains_key("to") {
            continue;
        }

        rewrite_presence(element, target_status, valorant_version);
        rewritten.push(serialize_element(element)?);
    }

    if rewritten.is_empty() {
        return Ok(None);
    }

    Ok(Some(rewritten.join("").into_bytes()))
}

pub fn helper_jid_for_chat_identity(host: &str, affinity: Option<&str>) -> String {
    let affinity = affinity
        .map(str::trim)
        .filter(|affinity| !affinity.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            host.split('.')
                .next()
                .filter(|affinity| !affinity.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "eu1".to_string());
    format!("{HELPER_PUUID}@{affinity}.pvp.net")
}

#[cfg(test)]
pub fn insert_helper_friend(content: &str, helper_jid: &str) -> Option<String> {
    if content.contains(helper_jid) {
        return None;
    }

    let open_at = roster_query_insert_at(content)?;
    let mut updated = content.to_string();
    updated.insert_str(open_at, &helper_roster_item(helper_jid));
    Some(updated)
}

pub fn helper_roster_push(helper_jid: &str) -> String {
    let now = chrono::Utc::now().timestamp_millis();
    format!(
        "<iq type='set' id='ghosty-roster-{now}'><query xmlns='{ROSTER_NAMESPACE}'>{}</query></iq>",
        helper_roster_item(helper_jid)
    )
}

pub fn contains_roster_marker(content: &str) -> bool {
    content.contains(ROSTER_NAMESPACE)
}

pub fn contains_unaddressed_presence_fragment(content: &str) -> bool {
    if !content.contains("<presence") {
        return false;
    }

    let wrapped = format!("<xml>{content}</xml>");
    let Ok(root) = Element::parse_with_config(
        Cursor::new(wrapped.as_bytes()),
        ParserConfig::new().whitespace_to_characters(true),
    ) else {
        return false;
    };

    root.children.iter().any(|node| {
        matches!(
            node,
            XMLNode::Element(element)
                if element.name == "presence" && !element.attributes.contains_key("to")
        )
    })
}

pub fn roster_item_count(content: &str) -> usize {
    if !contains_roster_marker(content) {
        return 0;
    }

    content.match_indices("<item").count()
}

#[cfg(test)]
fn roster_query_insert_at(content: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(relative_query_at) = content[search_from..].find("<query") {
        let query_at = search_from + relative_query_at;
        let tag_end = query_at + content[query_at..].find('>')?;
        let tag = &content[query_at..=tag_end];
        if tag.contains(ROSTER_NAMESPACE) {
            return content[tag_end + 1..]
                .find("</query>")
                .map(|relative_close_at| tag_end + 1 + relative_close_at);
        }
        search_from = tag_end + 1;
    }
    None
}

pub fn helper_presence(helper_jid: &str, valorant_version: Option<&str>) -> String {
    let now = chrono::Utc::now().timestamp_millis();
    let league_presence = helper_league_presence();
    let valorant_presence = helper_valorant_presence(valorant_version.unwrap_or("unknown"));
    format!(
        "<presence from='{helper_jid}/RC-Ghosty' id='ghosty-{now}'>\
         <games>\
         <keystone><st>chat</st><s.t>{now}</s.t><s.p>keystone</s.p><pty/></keystone>\
         <league_of_legends><st>chat</st><s.t>{now}</s.t><s.r>NA1</s.r><s.p>league_of_legends</s.p><s.c>live</s.c><p>{league_presence}</p><pty/></league_of_legends>\
         <valorant><st>chat</st><s.t>{now}</s.t><s.p>valorant</s.p><s.r>PC</s.r><p>{valorant_presence}</p><pty/></valorant>\
         <bacon><st>chat</st><s.t>{now}</s.t><s.l>bacon_availability_online</s.l><s.p>bacon</s.p></bacon>\
         </games><show>chat</show><platform>riot</platform><status/></presence>"
    )
}

fn helper_league_presence() -> String {
    let payload = format!(
        r#"{{
            "championId": "",
            "gameQueueType": "",
            "gameStatus": "outOfGame",
            "level": "1",
            "profileIcon": "{HELPER_PROFILE_ICON}",
            "puuid": "{HELPER_PUUID}",
            "queueId": null
        }}"#
    );
    let encoded_json_string = serde_json::to_string(&payload)
        .expect("helper League presence payload should serialize as JSON string");
    STANDARD.encode(encoded_json_string)
}

fn helper_valorant_presence(version: &str) -> String {
    let json = format!(
        r#"{{
            "isValid": true,
            "isIdle": false,
            "queueId": "competitive",
            "provisioningFlow": "Invalid",
            "partyId": "00000000-0000-0000-0000-000000000000",
            "partySize": 1,
            "maxPartySize": 5,
            "premierPresenceData": {{
                "rosterId": "",
                "rosterName": "Ghosty is active. Ignore any version mismatch warnings.",
                "rosterTag": "Ghosty Active!",
                "rosterType": "VCT",
                "division": 0,
                "score": 0,
                "plating": 0,
                "showAura": false,
                "showTag": true,
                "showPlating": false
            }},
            "matchPresenceData": {{
                "sessionLoopState": "MENUS",
                "provisioningFlow": "Invalid",
                "matchMap": "",
                "queueId": "competitive"
            }},
            "partyPresenceData": {{
                "partyId": "00000000-0000-0000-0000-000000000000",
                "isPartyOwner": true,
                "partyState": "DEFAULT",
                "partyAccessibility": "CLOSED",
                "partyLFM": false,
                "partyClientVersion": "{version}",
                "partyVersion": 1768830115681,
                "partySize": 1,
                "queueEntryTime": "0001.01.01-00.00.00",
                "isPartyCrossPlayEnabled": false,
                "isPlayerCrossPlayEnabled": false,
                "partyPrecisePlatformTypes": 1,
                "customGameName": "Ghosty Active!",
                "customGameTeam": "",
                "maxPartySize": 5,
                "tournamentId": "",
                "rosterId": "",
                "partyOwnerSessionLoopState": "MENUS",
                "partyOwnerMatchMap": "",
                "partyOwnerProvisioningFlow": "Invalid",
                "partyOwnerMatchScoreAllyTeam": 0,
                "partyOwnerMatchScoreEnemyTeam": 0
            }},
            "playerPresenceData": {{
                "playerCardId": "893deca1-4123-9c1f-2985-aa9de74cb512",
                "playerTitleId": "e3ca05a4-4e44-9afe-3791-7d96ca8f71fa",
                "accountLevel": 999,
                "competitiveTier": 0,
                "leaderboardPosition": 0
            }}
        }}"#
    );
    STANDARD.encode(json)
}

pub fn contains_helper_message(content: &str, helper_jid: &str) -> bool {
    content.contains("<message")
        && (content.contains(&format!("to='{helper_jid}"))
            || content.contains(&format!("to=\"{helper_jid}")))
}

pub fn helper_chat_message(helper_jid: &str, message: &str) -> String {
    let stamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
    format!(
        "<message from='{helper_jid}/RC-Ghosty' stamp='{stamp}' id='ghosty-{stamp}' type='chat'><body>{}</body></message>",
        escape_xml(message)
    )
}

fn rewrite_presence(
    presence: &mut Element,
    target_status: PresenceStatus,
    valorant_version: &mut Option<String>,
) {
    let target = target_status.as_xmpp();

    if target_status != PresenceStatus::Chat || league_st(presence).as_deref() != Some("dnd") {
        replace_child_text(presence, "show", target);
        if let Some(st) = child_path_mut(presence, &["games", "league_of_legends", "st"]) {
            st.children = vec![XMLNode::Text(target.to_string())];
        }
    }

    if target_status == PresenceStatus::Chat {
        return;
    }

    remove_child(presence, "status");

    if let Some(league) = child_path_mut(presence, &["games", "league_of_legends"]) {
        remove_child(league, "p");
        remove_child(league, "m");
    }

    if valorant_version.is_none() {
        *valorant_version = valorant_client_version(presence);
    }

    if let Some(games) = child_path_mut(presence, &["games"]) {
        for game in ["bacon", "lion", "keystone", "riot_client", "valorant"] {
            remove_child(games, game);
        }
    }
}

fn helper_roster_item(helper_jid: &str) -> String {
    format!(
        "<item jid='{helper_jid}' name='&#9;{HELPER_NAME}' subscription='both' puuid='{HELPER_PUUID}'>\
         <group priority='9999'>Ghosty</group>\
         <state>online</state>\
         <id name='&#9;{HELPER_NAME}' tagline='...'/>\
         <lol name='&#9;{HELPER_NAME}'/>\
         <platforms><riot name='&#9;Ghosty Active' tagline='...'/></platforms>\
         </item>"
    )
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn league_st(presence: &mut Element) -> Option<String> {
    child_path_mut(presence, &["games", "league_of_legends", "st"]).and_then(element_text)
}

fn valorant_client_version(presence: &mut Element) -> Option<String> {
    let encoded = child_path_mut(presence, &["games", "valorant", "p"]).and_then(element_text)?;
    let decoded = STANDARD.decode(encoded).ok()?;
    let json: Value = serde_json::from_slice(&decoded).ok()?;
    json.pointer("/partyPresenceData/partyClientVersion")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn child_path_mut<'a>(element: &'a mut Element, path: &[&str]) -> Option<&'a mut Element> {
    let mut current = element;
    for name in path {
        current = current.get_mut_child(*name)?;
    }
    Some(current)
}

fn remove_child(element: &mut Element, name: &str) {
    element.children.retain(|node| match node {
        XMLNode::Element(child) => child.name != name,
        _ => true,
    });
}

fn replace_child_text(element: &mut Element, child_name: &str, value: &str) {
    if let Some(child) = element.get_mut_child(child_name) {
        child.children = vec![XMLNode::Text(value.to_string())];
    }
}

fn element_text(element: &mut Element) -> Option<String> {
    element.children.iter().find_map(|node| match node {
        XMLNode::Text(text) => Some(text.clone()),
        _ => None,
    })
}

fn serialize_element(element: &Element) -> Result<String> {
    let mut out = Vec::new();
    element.write_with_config(
        &mut out,
        EmitterConfig::new()
            .write_document_declaration(false)
            .perform_indent(false),
    )?;
    String::from_utf8(out).map_err(|e| anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_HELPER_JID: &str = "41c322a1-b328-495b-a004-5ccd3e45eae8@na2.pvp.net";

    #[test]
    fn derives_helper_jid_from_chat_identity() {
        assert_eq!(
            helper_jid_for_chat_identity("na2.chat.si.riotgames.com", None),
            TEST_HELPER_JID
        );
        assert_eq!(
            helper_jid_for_chat_identity("na2.chat.si.riotgames.com", Some("na1")),
            "41c322a1-b328-495b-a004-5ccd3e45eae8@na1.pvp.net"
        );
        assert_eq!(
            helper_jid_for_chat_identity("", None),
            "41c322a1-b328-495b-a004-5ccd3e45eae8@eu1.pvp.net"
        );
    }

    #[test]
    fn appends_helper_friend_to_riot_roster() {
        let roster_open = "<query xmlns='jabber:iq:riotgames:roster'>";
        let input = format!(
            "<iq type='result'>{}<item jid='friend@na2.pvp.net'/></query></iq>",
            roster_open
        );

        let updated = insert_helper_friend(&input, TEST_HELPER_JID)
            .expect("helper friend should be inserted");

        let helper_at = updated
            .find(TEST_HELPER_JID)
            .expect("helper jid should be present");
        let friend_at = updated
            .find("friend@na2.pvp.net")
            .expect("original friend should remain");
        assert!(friend_at < helper_at);
        assert!(updated.contains("Ghosty Active!"));
    }

    #[test]
    fn counts_roster_items_only_inside_roster_payloads() {
        assert_eq!(
            roster_item_count(
                "<iq><query xmlns='jabber:iq:riotgames:roster'><item jid='a'/><item jid='b'/></query></iq>"
            ),
            2
        );
        assert_eq!(
            roster_item_count("<message><item jid='not-roster'/></message>"),
            0
        );
    }

    #[test]
    fn helper_roster_item_uses_client_visible_identity_fields() {
        let item = helper_roster_item(TEST_HELPER_JID);

        assert!(item.contains("name='&#9;Ghosty Active!'"));
        assert!(item.contains("<group priority='9999'>Ghosty</group>"));
        assert!(item.contains("<state>online</state>"));
        assert!(item.contains("<lol name='&#9;Ghosty Active!'/>"));
        assert!(
            item.contains("<platforms><riot name='&#9;Ghosty Active' tagline='...'/></platforms>")
        );
    }

    #[test]
    fn helper_roster_push_wraps_helper_item_without_replacing_initial_roster() {
        let push = helper_roster_push(TEST_HELPER_JID);

        assert!(push.contains("type='set'"));
        assert!(push.contains("jabber:iq:riotgames:roster"));
        assert!(push.contains(TEST_HELPER_JID));
        assert!(push.contains("Ghosty Active!"));
    }

    #[test]
    fn inserts_helper_friend_after_roster_query_with_double_quotes_and_extra_attributes() {
        let input = "<iq type='result'><query ver='2' xmlns=\"jabber:iq:riotgames:roster\"><item jid='friend@na2.pvp.net'/></query></iq>";

        let updated =
            insert_helper_friend(input, TEST_HELPER_JID).expect("helper friend should be inserted");

        let helper_at = updated
            .find(TEST_HELPER_JID)
            .expect("helper jid should be present");
        let friend_at = updated
            .find("friend@na2.pvp.net")
            .expect("original friend should remain");
        assert!(friend_at < helper_at);
        assert!(updated.contains("Ghosty Active!"));
    }

    #[test]
    fn ignores_unrelated_query_before_roster_query() {
        let input = "<iq><query xmlns='jabber:iq:riotgames:auth'/><query xmlns=\"jabber:iq:riotgames:roster\"><item jid='friend@na2.pvp.net'/></query></iq>";

        let updated =
            insert_helper_friend(input, TEST_HELPER_JID).expect("helper friend should be inserted");

        assert!(updated.contains("<query xmlns='jabber:iq:riotgames:auth'/>"));
        assert!(updated.contains(TEST_HELPER_JID));
    }

    #[test]
    fn does_not_insert_helper_friend_twice() {
        let input = format!(
            "<iq type='result'><query xmlns='jabber:iq:riotgames:roster'>{}</query></iq>",
            helper_roster_item(TEST_HELPER_JID)
        );

        assert!(insert_helper_friend(&input, TEST_HELPER_JID).is_none());
    }

    #[test]
    fn detects_unaddressed_presence_in_mixed_fragments() {
        assert!(contains_unaddressed_presence_fragment(
            "<message/><presence><show>chat</show></presence>"
        ));
        assert!(!contains_unaddressed_presence_fragment(
            "<presence to='room@conference.pvp.net'><show>chat</show></presence>"
        ));
        assert!(!contains_unaddressed_presence_fragment(
            "<presence><show>chat"
        ));
    }

    #[test]
    fn rewrites_only_unaddressed_presence_for_warmup_followup() {
        let mut valorant_version = None;
        let rewritten = rewrite_unaddressed_presence_only_fragment(
            "<iq type='get' id='1'/><presence id='presence_5'><show>chat</show><status>hello</status><games><keystone><st>chat</st></keystone></games></presence><presence to='room@conference.pvp.net'><show>chat</show></presence>",
            PresenceStatus::Mobile,
            &mut valorant_version,
        )
        .expect("rewrite should not fail")
        .expect("presence should be rewritten");
        let rewritten = String::from_utf8(rewritten).expect("presence should stay utf8");

        assert!(rewritten.contains("<presence id=\"presence_5\">"));
        assert!(rewritten.contains("<show>mobile</show>"));
        assert!(!rewritten.contains("<iq"));
        assert!(!rewritten.contains("conference.pvp.net"));
        assert!(!rewritten.contains("<status>hello</status>"));
    }

    #[test]
    fn helper_friend_is_inserted_into_roster_results_without_roster_set_push() {
        let input = "<iq type='result' id='roster-1'><query xmlns='jabber:iq:riotgames:roster'><item jid='friend@na2.pvp.net'/></query></iq>";

        let updated =
            insert_helper_friend(input, TEST_HELPER_JID).expect("helper friend should be inserted");

        assert!(updated.contains("type='result'"));
        assert!(!updated.contains("type='set'"));
        assert!(updated.contains(TEST_HELPER_JID));
        assert!(updated.contains("friend@na2.pvp.net"));
        assert!(
            updated.find("friend@na2.pvp.net").expect("friend remains")
                < updated.find(TEST_HELPER_JID).expect("helper inserted")
        );
    }

    #[test]
    fn helper_presence_includes_game_payloads_and_valorant_version() {
        let presence = helper_presence(TEST_HELPER_JID, Some("release-10.11"));

        assert!(presence.contains("<league_of_legends>"));
        assert!(presence.contains("<valorant>"));
        assert!(presence.contains("<bacon>"));

        let encoded = presence
            .split("<league_of_legends>")
            .nth(1)
            .and_then(|league| league.split("<p>").nth(1))
            .and_then(|payload| payload.split("</p>").next())
            .expect("League payload should be present");
        let decoded = String::from_utf8(
            STANDARD
                .decode(encoded)
                .expect("League payload should be base64"),
        )
        .expect("League payload should be utf8");
        let league_payload: String =
            serde_json::from_str(&decoded).expect("League payload should be a JSON string");

        assert!(league_payload.contains("\"profileIcon\": \"3151\""));
        assert!(league_payload.contains("\"gameStatus\": \"outOfGame\""));

        let encoded = presence
            .split("<valorant>")
            .nth(1)
            .and_then(|valorant| valorant.split("<p>").nth(1))
            .and_then(|payload| payload.split("</p>").next())
            .expect("valorant payload should be present");
        let decoded = String::from_utf8(
            STANDARD
                .decode(encoded)
                .expect("valorant payload should be base64"),
        )
        .expect("valorant payload should be utf8");

        assert!(decoded.contains("\"partyClientVersion\": \"release-10.11\""));
        assert!(decoded.contains("\"customGameName\": \"Ghosty Active!\""));
    }

    #[test]
    fn rewrites_presence_inside_mixed_xml_fragments() {
        let mut valorant_version = None;
        let rewritten = rewrite_presence_fragment(
            "<message id='1'/><presence><show>chat</show></presence>",
            true,
            PresenceStatus::Offline,
            false,
            &mut valorant_version,
        )
        .expect("rewrite should not fail")
        .expect("mixed presence fragment should be rewritten");

        let PresenceRewrite::Forward(rewritten) = rewritten else {
            panic!("mixed fragment should still forward the preserved message");
        };

        assert!(rewritten.contains("<message"));
        assert!(rewritten.contains("<show>offline</show>"));
    }

    #[test]
    fn rewrite_presence_preserves_text_between_mixed_stanzas() {
        let mut valorant_version = None;
        let rewritten = rewrite_presence_fragment(
            "<message id='1'/>\n<presence><show>chat</show></presence>\n<iq id='2'/>",
            true,
            PresenceStatus::Offline,
            true,
            &mut valorant_version,
        )
        .expect("rewrite should not fail")
        .expect("mixed presence fragment should be rewritten");

        let PresenceRewrite::Forward(rewritten) = rewritten else {
            panic!("mixed fragment should still forward the preserved stanzas");
        };

        assert!(rewritten.contains("<message"));
        let first_newline = rewritten.find('\n').expect("first newline should remain");
        let second_newline = rewritten.rfind('\n').expect("second newline should remain");

        assert!(rewritten[..first_newline].contains("<message"));
        assert!(rewritten[first_newline + 1..].starts_with("<presence"));
        assert!(rewritten[..second_newline].ends_with("</presence>"));
        assert!(rewritten[second_newline + 1..].starts_with("<iq"));
        assert!(rewritten.contains("<show>offline</show>"));
    }

    #[test]
    fn drops_addressed_presence_when_muc_passthrough_is_disabled() {
        let mut valorant_version = None;
        let rewritten = rewrite_presence_fragment(
            "<presence to='room@conference.pvp.net'><show>chat</show></presence>",
            true,
            PresenceStatus::Offline,
            false,
            &mut valorant_version,
        )
        .expect("rewrite should not fail")
        .expect("addressed presence should be handled");

        assert!(matches!(rewritten, PresenceRewrite::Drop));
    }

    #[test]
    fn preserves_addressed_presence_when_muc_passthrough_is_enabled() {
        let mut valorant_version = None;
        let rewritten = rewrite_presence_fragment(
            "<presence to='room@conference.pvp.net'><games><league_of_legends><st>chat</st></league_of_legends></games><show>chat</show><status>hello</status></presence>",
            true,
            PresenceStatus::Offline,
            true,
            &mut valorant_version,
        )
        .expect("rewrite should not fail")
        .expect("addressed presence should be handled");

        let PresenceRewrite::Forward(rewritten) = rewritten else {
            panic!("addressed presence should be forwarded unchanged");
        };

        assert!(rewritten.contains("to=\"room@conference.pvp.net\""));
        assert!(rewritten.contains("<show>chat</show>"));
        assert!(rewritten.contains("<status>hello</status>"));
        assert!(rewritten.contains("<league_of_legends>"));
        assert!(!rewritten.contains("<show>offline</show>"));
    }

    #[test]
    fn rewrites_presence_to_offline_without_dropping_stream() {
        let mut valorant_version = None;
        let rewritten = rewrite_presence_fragment(
            "<presence><games><league_of_legends><st>chat</st></league_of_legends></games><show>chat</show><status>hello</status></presence>",
            true,
            PresenceStatus::Offline,
            false,
            &mut valorant_version,
        )
        .expect("rewrite should succeed")
        .expect("presence should be rewritten");
        let PresenceRewrite::Forward(rewritten) = rewritten else {
            panic!("presence should be forwarded after rewrite");
        };

        assert!(rewritten.contains("<show>offline</show>"));
        assert!(rewritten.contains("<league_of_legends>"));
        assert!(rewritten.contains("<st>offline</st>"));
        assert!(!rewritten.contains("<status>"));
    }

    #[test]
    fn offline_presence_preserves_league_product_node_without_rich_payload() {
        let mut valorant_version = None;
        let rewritten = rewrite_presence_fragment(
            "<presence><games><league_of_legends><st>chat</st><s.p>league_of_legends</s.p><s.c>live</s.c><p>{}</p><m>secret</m></league_of_legends></games><show>chat</show><status>hello</status></presence>",
            true,
            PresenceStatus::Offline,
            false,
            &mut valorant_version,
        )
        .expect("rewrite should succeed")
        .expect("presence should be rewritten");
        let PresenceRewrite::Forward(rewritten) = rewritten else {
            panic!("presence should be forwarded after rewrite");
        };

        assert!(rewritten.contains("<league_of_legends>"));
        assert!(rewritten.contains("<st>offline</st>"));
        assert!(rewritten.contains("<s.p>league_of_legends</s.p>"));
        assert!(rewritten.contains("<s.c>live</s.c>"));
        assert!(!rewritten.contains("<p>"));
        assert!(!rewritten.contains("<m>"));
        assert!(!rewritten.contains("<status>"));
    }
}
