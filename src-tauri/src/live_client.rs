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

/// Seconds until the local player respawns, while they're dead. Game seconds, so it holds still
/// while the game is paused.
pub fn respawn_in(players: &Value, names: &HashSet<String>) -> Option<f64> {
    let me = find_player(players, names)?;
    if me.get("isDead").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    me.get("respawnTimer").and_then(Value::as_f64)
}

/// Something with a name to show ("Miss Fortune") and one for file names ("MissFortune").
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

/// Kills further apart than this (in seconds) are separate fights.
const FIGHT_GAP: f64 = 10.0;

/// The local player's latest death, and the fight it ended.
pub struct Fight {
    /// When the death happened, in game seconds.
    pub death: f64,
    /// How long the fight had been going, in seconds: from its first kill to the death. 0 when the
    /// death was the fight's only kill.
    pub length: f64,
}

/// The local player's latest death in `events` and the fight that led to it, if they've died.
///
/// The API has no positions, so a fight is told apart from kills elsewhere on the map by who took
/// part: it starts as the death's killer, assisters and victim, and an earlier kill joins it when it
/// happened within `FIGHT_GAP` of the fight's earliest kill so far and shares someone with it (its
/// killer, victim or an assister). Its people then join the fight too.
pub fn last_fight(events: Option<&Value>, names: &HashSet<String>) -> Option<Fight> {
    let kills: Vec<&Value> = events
        .and_then(|e| e.get("Events"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|ev| ev.get("EventName").and_then(Value::as_str) == Some("ChampionKill"))
        .collect();
    let victim = |ev: &Value| ev.get("VictimName").and_then(Value::as_str).unwrap_or("").to_string();
    let death = kills.iter().rposition(|ev| is_me(&victim(ev), names))?;
    let time = |ev: &Value| ev.get("EventTime").and_then(Value::as_f64).unwrap_or(0.0);
    let people = |ev: &Value| -> Vec<String> {
        let assisters = ev.get("Assisters").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
        ["KillerName", "VictimName"]
            .iter()
            .filter_map(|k| ev.get(*k).and_then(Value::as_str))
            .chain(assisters)
            .filter(|n| !n.is_empty())
            .map(|n| without_tag(n).to_string())
            .collect()
    };

    let death_time = time(kills[death]);
    let mut fight: HashSet<String> = people(kills[death]).into_iter().collect();
    let mut start = death_time;
    for ev in kills[..death].iter().rev() {
        let t = time(ev);
        if start - t > FIGHT_GAP {
            break; // events are in time order, so every earlier kill is out of range too
        }
        let who = people(ev);
        if who.iter().any(|n| fight.contains(n)) {
            fight.extend(who);
            start = start.min(t);
        }
    }
    Some(Fight { death: death_time, length: death_time - start })
}

/// Internal names the API uses for non-champion killers, e.g. "Turret_TChaos_L1_P2_2237424422" or
/// "SRU_Dragon_Water6.1.1" (unit name, then camp number and which monster in the camp).
/// The first key the name contains wins, so specific keys must come before general ones.
/// Small camp monsters ("SRU_BlueMini", "SRU_KrugMini") count as their camp.
const KILLER_NAMES: &[(&str, &str)] = &[
    ("Turret", "Turret"),
    ("Minion", "Minion"),
    ("SRU_Dragon_Air", "Cloud Drake"),
    ("SRU_Dragon_Fire", "Infernal Drake"),
    ("SRU_Dragon_Earth", "Mountain Drake"),
    ("SRU_Dragon_Water", "Ocean Drake"),
    ("SRU_Dragon_Hextech", "Hextech Drake"),
    ("SRU_Dragon_Chemtech", "Chemtech Drake"),
    ("SRU_Dragon_Elder", "Elder Dragon"),
    ("SRU_Dragon", "Dragon"),
    ("SRU_Baron", "Baron"),
    ("SRU_RiftHerald", "Rift Herald"),
    ("SRU_Horde", "Voidgrub"),
    ("SRU_Atakhan", "Atakhan"),
    ("SRU_Blue", "Blue Buff"),
    ("SRU_Red", "Red Buff"),
    ("SRU_Gromp", "Gromp"),
    ("SRU_Murkwolf", "Murk Wolf"),
    ("SRU_Razorbeak", "Raptor"),
    ("SRU_Krug", "Krug"),
    ("SRU_Crab", "Scuttle Crab"),
];

fn pretty_killer(name: &str) -> String {
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
        assert_eq!(pretty_killer("SRU_Dragon_Air6.1.1"), "Cloud Drake");
        assert_eq!(pretty_killer("SRU_Dragon_Water6.1.1"), "Ocean Drake");
        assert_eq!(pretty_killer("SRU_Dragon_Elder6.1.1"), "Elder Dragon");
        assert_eq!(pretty_killer("SRU_Dragon_Ruined6.1.1"), "Dragon");
        assert_eq!(pretty_killer("SRU_Horde_Mini18.1.1"), "Voidgrub");
        assert_eq!(pretty_killer("SRU_BlueMini1.1.2"), "Blue Buff");
        assert_eq!(pretty_killer("SRU_Red4.1.1"), "Red Buff");
        assert_eq!(pretty_killer("SRU_KrugMiniMini5.1.3"), "Krug");
        assert_eq!(pretty_killer("SRU_Crab15.1.1"), "Scuttle Crab");
        assert_eq!(pretty_killer("SRU_Gromp13.1.1"), "Gromp");
        assert_eq!(pretty_killer("SRU_Duck20.1.1"), "Duck"); // unknown monsters fall back to cleanup
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
    fn reads_respawn_timer_only_while_dead() {
        let names = my_names(&serde_json::json!({ "riotId": "Robin#EUW" }));
        let players = serde_json::json!([
            { "riotId": "Other#EUW", "isDead": true, "respawnTimer": 3.0 },
            { "riotId": "Robin#EUW", "isDead": true, "respawnTimer": 12.5 },
        ]);
        assert_eq!(respawn_in(&players, &names), Some(12.5));
        let players = serde_json::json!([{ "riotId": "Robin#EUW", "isDead": false, "respawnTimer": 0.0 }]);
        assert_eq!(respawn_in(&players, &names), None);
    }

    fn kill(t: f64, killer: &str, victim: &str, assisters: &[&str]) -> Value {
        serde_json::json!({
            "EventName": "ChampionKill", "EventTime": t,
            "KillerName": killer, "VictimName": victim, "Assisters": assisters,
        })
    }

    #[test]
    fn measures_the_fight_before_a_death() {
        let names = my_names(&serde_json::json!({ "riotId": "Robin#EUW" }));
        let fight = |events: Vec<Value>| last_fight(Some(&serde_json::json!({ "Events": events })), &names);
        let length = |events: Vec<Value>| fight(events).map_or(0.0, |f| f.length);

        // A death on its own, and no death at all.
        assert_eq!(length(vec![kill(300.0, "Faker", "Robin", &[])]), 0.0);
        assert!(fight(vec![kill(300.0, "Robin", "Faker", &[])]).is_none());

        // A teamfight: Ally dies to Zed (who then helps kill Robin), Robin kills Lux before that.
        // An unrelated kill in the middle (another lane) neither joins nor breaks the chain.
        let teamfight = vec![
            kill(100.0, "Old", "Fight", &[]),          // too long before: a different fight
            kill(280.0, "Robin", "Lux", &["Ally"]),    // shares Robin
            kill(286.0, "Top1", "Top2", &[]),          // nobody from the fight
            kill(289.0, "Zed", "Ally", &["Lux"]),      // shares Zed and Ally
            kill(295.0, "Faker", "Robin#EUW", &["Zed"]), // the death; Riot ID tags are ignored
            kill(297.0, "Zed", "Ally2", &[]),          // after the death: not part of what led to it
        ];
        assert_eq!(fight(teamfight.clone()).map(|f| f.death), Some(295.0));
        assert_eq!(length(teamfight), 15.0);

        // A gap longer than FIGHT_GAP ends the chain even if the same people fight again.
        let two_fights = vec![kill(270.0, "Faker", "Ally", &[]), kill(295.0, "Faker", "Robin", &[])];
        assert_eq!(length(two_fights), 0.0);

        // The gap counts from the fight's earliest kill so far, so a long fight chains through.
        let long = vec![
            kill(250.0, "Zed", "A", &[]),
            kill(259.0, "Zed", "B", &[]),
            kill(268.0, "Faker", "C", &["Zed"]),
            kill(277.0, "Faker", "Robin", &[]),
        ];
        assert_eq!(length(long), 27.0);
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
