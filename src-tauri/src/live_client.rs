//! Riot's Live Client Data API, which the game serves on https://127.0.0.1:2999 during a match.
//! It's read-only and official; nothing here touches the game itself.

use std::collections::HashSet;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

pub struct LiveClient {
    http: reqwest::blocking::Client,
}

impl LiveClient {
    pub fn new() -> Self {
        let http = reqwest::blocking::Client::builder()
            .danger_accept_invalid_certs(true) // the game uses a self-signed certificate
            .timeout(Duration::from_secs(1))
            .build()
            .expect("http client");
        Self { http }
    }

    pub fn get(&self, path: &str) -> Option<Value> {
        let url = format!("https://127.0.0.1:2999/liveclientdata/{path}");
        let data: Value = self.http.get(url).send().ok()?.json().ok()?;
        // During the loading screen the API answers with an error object.
        if data.get("errorCode").is_some() {
            return None;
        }
        Some(data)
    }
}

const NAME_KEYS: [&str; 3] = ["riotId", "riotIdGameName", "summonerName"];

fn without_tag(name: &str) -> &str {
    name.split('#').next().unwrap_or(name)
}

fn is_me(name: &str, names: &HashSet<String>) -> bool {
    names.contains(name) || names.contains(without_tag(name))
}

/// All the names the API might use for the local player.
pub fn my_names(active: &Value) -> HashSet<String> {
    let mut names = HashSet::new();
    for key in NAME_KEYS {
        if let Some(v) = active.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()) {
            names.insert(v.to_string());
            names.insert(without_tag(v).to_string());
        }
    }
    names
}

pub fn is_dead(players: &Value, names: &HashSet<String>) -> bool {
    let me = players.as_array().into_iter().flatten().find(|p| {
        NAME_KEYS
            .iter()
            .filter_map(|k| p.get(*k).and_then(Value::as_str))
            .any(|v| !v.is_empty() && is_me(v, names))
    });
    me.and_then(|p| p.get("isDead")).and_then(Value::as_bool) == Some(true)
}

pub fn find_killer(events: Option<&Value>, names: &HashSet<String>) -> String {
    let events = events.and_then(|e| e.get("Events")).and_then(Value::as_array);
    for ev in events.into_iter().flatten().rev() {
        if ev.get("EventName").and_then(Value::as_str) == Some("ChampionKill") {
            let victim = ev.get("VictimName").and_then(Value::as_str).unwrap_or("");
            if is_me(victim, names) {
                let killer = ev.get("KillerName").and_then(Value::as_str).unwrap_or("");
                return pretty_killer(if killer.is_empty() { "unknown" } else { killer });
            }
        }
    }
    "unknown".into()
}

/// Internal names the API uses for non-champion killers, e.g. "Turret_TChaos_L1_P2_2237424422".
const KILLER_NAMES: [(&str, &str); 7] = [
    ("Turret", "Turret"),
    ("Minion", "Minion"),
    ("SRU_Baron", "Baron"),
    ("SRU_RiftHerald", "Rift Herald"),
    ("SRU_Horde", "Voidgrubs"),
    ("SRU_Dragon_Elder", "Elder Dragon"),
    ("SRU_Dragon", "Dragon"),
];

pub fn pretty_killer(name: &str) -> String {
    if let Some((_, pretty)) = KILLER_NAMES.iter().find(|(key, _)| name.contains(key)) {
        return pretty.to_string();
    }
    if let Some(monster) = name.strip_prefix("SRU_") {
        // Other jungle monsters, e.g. "SRU_Gromp13.1.1" -> "Gromp"
        let trailing = Regex::new(r"[\d.]+$").unwrap();
        return trailing.replace(monster, "").replace('_', " ");
    }
    without_tag(name).to_string() // champions: drop the Riot ID tag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_killer_names() {
        assert_eq!(pretty_killer("Turret_TChaos_L1_P2_2237424422_0"), "Turret");
        assert_eq!(pretty_killer("Minion_T200L1S04N0010"), "Minion");
        assert_eq!(pretty_killer("SRU_Baron12.1.1"), "Baron");
        assert_eq!(pretty_killer("SRU_Dragon_Air6.1.1"), "Dragon");
        assert_eq!(pretty_killer("SRU_Gromp13.1.1"), "Gromp");
        assert_eq!(pretty_killer("Faker#KR1"), "Faker");
    }

    #[test]
    fn finds_local_player_death() {
        let active = serde_json::json!({ "riotId": "Robin#EUW", "summonerName": "Robin" });
        let names = my_names(&active);
        let players = serde_json::json!([
            { "riotId": "Other#EUW", "isDead": true },
            { "riotId": "Robin#EUW", "isDead": false },
        ]);
        assert!(!is_dead(&players, &names));
        let players = serde_json::json!([{ "riotIdGameName": "Robin", "isDead": true }]);
        assert!(is_dead(&players, &names));
    }
}
