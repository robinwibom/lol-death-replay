//! Riot's Live Client Data API, which the game serves on https://127.0.0.1:2999 during a match.
//! It's read-only and official; nothing here touches the game itself.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::Value;

pub struct LiveClient {
    http: ureq::Agent,
}

impl LiveClient {
    pub fn new() -> Self {
        // Windows' own TLS (SChannel). Certificates aren't checked: the game's is self-signed.
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .disable_verification(true)
            .build();
        let http = ureq::Agent::config_builder()
            .tls_config(tls)
            .timeout_global(Some(Duration::from_secs(1)))
            .http_status_as_error(false) // error answers are JSON too; see below
            .build()
            .into();
        Self { http }
    }

    pub fn get(&self, path: &str) -> Option<Value> {
        let url = format!("https://127.0.0.1:2999/liveclientdata/{path}");
        let data: Value = self.http.get(&url).call().ok()?.body_mut().read_json().ok()?;
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

/// The `playerlist` entry for whoever goes by one of `names`.
pub fn find_player<'a>(players: &'a Value, names: &HashSet<String>) -> Option<&'a Value> {
    players.as_array().into_iter().flatten().find(|p| {
        NAME_KEYS
            .iter()
            .filter_map(|k| p.get(*k).and_then(Value::as_str))
            .any(|v| !v.is_empty() && is_me(v, names))
    })
}

pub fn is_dead(players: &Value, names: &HashSet<String>) -> bool {
    find_player(players, names).and_then(|p| p.get("isDead")).and_then(Value::as_bool) == Some(true)
}

/// Something with a name to show ("Miss Fortune") and one for file names ("MissFortune").
#[derive(Clone, Debug, PartialEq)]
pub struct Named {
    pub display: String,
    pub file: String,
}

/// A player's champion. `championName` is in the game's language and may hold spaces or
/// apostrophes; `rawChampionName` ("game_character_displayname_MissFortune") is always plain English.
pub fn champion(player: &Value) -> Option<Named> {
    let display = player.get("championName").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let file = player
        .get("rawChampionName")
        .and_then(Value::as_str)
        .and_then(|raw| raw.rsplit('_').next())
        .filter(|s| !s.is_empty())
        .unwrap_or(display);
    Some(Named { display: display.to_string(), file: file.to_string() })
}

/// What killed the local player most recently: the killer's champion, or a turret, monster and so on.
pub fn find_killer(events: Option<&Value>, players: Option<&Value>, names: &HashSet<String>) -> Named {
    let events = events.and_then(|e| e.get("Events")).and_then(Value::as_array);
    let killer = events
        .into_iter()
        .flatten()
        .rev()
        .filter(|ev| ev.get("EventName").and_then(Value::as_str) == Some("ChampionKill"))
        .find(|ev| is_me(ev.get("VictimName").and_then(Value::as_str).unwrap_or(""), names))
        .and_then(|ev| ev.get("KillerName").and_then(Value::as_str))
        .filter(|k| !k.is_empty())
        .unwrap_or("unknown");
    let killer_names = HashSet::from([killer.to_string(), without_tag(killer).to_string()]);
    if let Some(champ) = players.and_then(|p| find_player(p, &killer_names)).and_then(champion) {
        return champ;
    }
    let pretty = pretty_killer(killer);
    Named { file: pretty.replace(' ', ""), display: pretty }
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
        return monster.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.').replace('_', " ");
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

    #[test]
    fn names_killer_by_champion() {
        let names = my_names(&serde_json::json!({ "riotId": "Robin#EUW" }));
        let players = serde_json::json!([{
            "riotId": "Faker#KR1",
            "championName": "Miss Fortune",
            "rawChampionName": "game_character_displayname_MissFortune",
        }]);
        let kill = |killer: &str| {
            serde_json::json!({ "Events": [
                { "EventName": "ChampionKill", "VictimName": "Robin", "KillerName": killer },
            ]})
        };
        let by_faker = find_killer(Some(&kill("Faker")), Some(&players), &names);
        assert_eq!((by_faker.display.as_str(), by_faker.file.as_str()), ("Miss Fortune", "MissFortune"));
        let by_herald = find_killer(Some(&kill("SRU_RiftHerald17.1.1")), Some(&players), &names);
        assert_eq!((by_herald.display.as_str(), by_herald.file.as_str()), ("Rift Herald", "RiftHerald"));
    }
}
