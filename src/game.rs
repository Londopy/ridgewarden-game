//! Game events and what they say: seasons and sessions started and ended by admins, team
//! lists, check-ins countersigned by the site they were made at, code words claimed for
//! public sites and foxes, the words revealed after each slot, bearings and spots. Events
//! are stored as they came; who may do what is decided when reading, so a change to the
//! admin list needs no rebuild.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::event::valid_call;
use ridgewarden_client::mesh::{f, s};
use ridgewarden_client::util::{norm_node, valid_node};
use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::world::Pos;

pub const MIN: i64 = 60_000;
pub const DAY: i64 = 86_400_000;
pub const NAME_MAX: usize = 80;
pub const TEAM_MAX: usize = 24;
pub const TEAMS_MAX: usize = 8;
pub const SITES_MAX: usize = 100;
pub const FOXES_MAX: usize = 20;
pub const ROSTER_MAX: usize = 500;
/// longest spot text
pub const TEXT_MAX: usize = 140;
/// longest planned session, a week
pub const MINUTES_MAX: i64 = 7 * 24 * 60;
/// bigger payloads are ignored; a full roster fits easily
const PAYLOAD_MAX: usize = 32 * 1024;

/// How a session is scored. The admin's config supplies them when a session starts, and
/// they travel in the start event, so every site scores the session the same way.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Rules {
    /// per minute, for each site in a chain of two or more
    pub chain_points: i64,
    /// for taking a site from another team or from nobody
    pub capture_points: i64,
    /// each repeat capture of the same site by the same team is worth this much of the last
    pub repeat_factor: f64,
    /// to each of two players from different teams who check in at one site close in time
    pub encounter_points: i64,
    pub encounter_min: i64,
    /// extra per minute for each new link in a chain
    pub builder_points: i64,
    /// distinct days a new link must have shown up in link data before it earns the bonus
    pub builder_days: i64,
    /// a link stops being new this many days after it first showed up
    pub builder_window_days: i64,
    /// for finding a fox; its operator gets `encounter_points` for each finder
    pub fox_points: i64,
    /// how long each code word lasts
    pub word_min: i64,
    /// faster than this between two sites is an impossible hop
    pub max_speed_kmh: f64,
    /// impossible hops before a player is out of the session
    pub strikes: i64,
    /// link types that count as roads (AREDN's "RF", "DTD", "TUN", ...)
    pub link_types: Vec<String>,
    /// a site's link data older than this counts as unknown, which doesn't score
    pub fresh_s: i64,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            chain_points: 1,
            capture_points: 10,
            repeat_factor: 0.5,
            encounter_points: 5,
            encounter_min: 10,
            builder_points: 2,
            builder_days: 3,
            builder_window_days: 90,
            fox_points: 20,
            word_min: 15,
            max_speed_kmh: 100.0,
            strikes: 3,
            link_types: vec!["RF".into()],
            fresh_s: 600,
        }
    }
}

fn within<T: PartialOrd + std::fmt::Display>(name: &str, v: T, lo: T, hi: T) -> Result<()> {
    if v < lo || v > hi {
        bail!("{name} must be between {lo} and {hi}");
    }
    Ok(())
}

impl Rules {
    pub fn validate(&self) -> Result<()> {
        for (name, p) in [
            ("chain_points", self.chain_points),
            ("capture_points", self.capture_points),
            ("encounter_points", self.encounter_points),
            ("builder_points", self.builder_points),
            ("fox_points", self.fox_points),
        ] {
            within(name, p, 0, 1000)?;
        }
        within("repeat_factor", self.repeat_factor, 0.0, 1.0)?;
        within("encounter_min", self.encounter_min, 0, 120)?;
        within("builder_days", self.builder_days, 1, 365)?;
        within(
            "builder_window_days",
            self.builder_window_days,
            self.builder_days,
            3650,
        )?;
        within("word_min", self.word_min, 5, 120)?;
        within("max_speed_kmh", self.max_speed_kmh, 1.0, 1000.0)?;
        within("strikes", self.strikes, 1, 10)?;
        within("fresh_s", self.fresh_s, 60, 86_400)?;
        let types_ok = self
            .link_types
            .iter()
            .all(|t| !t.is_empty() && t.len() <= 16 && t.bytes().all(|b| b.is_ascii_uppercase()));
        if self.link_types.is_empty() || self.link_types.len() > 8 || !types_ok {
            bail!("link_types must be 1 to 8 upper-case names such as \"RF\"");
        }
        Ok(())
    }

    /// Points for a team's nth capture of the same site, n from 1.
    pub fn capture_worth(&self, n: u32) -> i64 {
        (self.capture_points as f64 * self.repeat_factor.powi(n as i32 - 1)).floor() as i64
    }

    pub fn counts(&self, link_type: Option<&str>) -> bool {
        // older polls may not say; a link with no type is taken as RF, like Link Doctor does
        let t = link_type.unwrap_or("RF");
        self.link_types.iter().any(|x| x.eq_ignore_ascii_case(t))
    }
}

/// Team and fox names: letters, digits, spaces and hyphens, shown on every page.
pub fn valid_team(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= TEAM_MAX
        && t.trim() == t
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b' ' || b == b'-')
}

/// A list of names: valid, no two the same ignoring case.
fn check_names(what: &str, names: &[String]) -> Result<()> {
    if let Some(t) = names.iter().find(|t| !valid_team(t)) {
        bail!("{t:?} is not a {what} name (up to {TEAM_MAX} letters, digits, spaces and hyphens)");
    }
    for (i, a) in names.iter().enumerate() {
        if names[..i].iter().any(|b| b.eq_ignore_ascii_case(a)) {
            bail!("{what} {a:?} is listed twice");
        }
    }
    Ok(())
}

/// A team list: 2 to 8 valid names, no two the same ignoring case.
pub fn check_teams(teams: &[String]) -> Result<()> {
    if !(2..=TEAMS_MAX).contains(&teams.len()) {
        bail!("a game needs 2 to {TEAMS_MAX} teams");
    }
    check_names("team", teams)
}

/// A hidden transmitter that sends rotating code words.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Fox {
    pub name: String,
    /// whoever hid and runs it, credited for each finder
    pub call: Option<String>,
    /// where it was, once an admin says at the end
    pub at: Option<Pos>,
}

/// Where a play happened: a site (by LAN check-in or code word) or a fox.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Site(String),
    Fox(String),
}

impl Target {
    /// The name code words are derived from and reveals are filed under.
    pub fn key(&self) -> String {
        match self {
            Target::Site(n) => format!("site:{n}"),
            Target::Fox(n) => format!("fox:{n}"),
        }
    }

    fn from_payload(p: &Value) -> Option<Self> {
        match (s(p, "site"), s(p, "fox")) {
            (Some(n), None) => {
                let n = norm_node(n);
                valid_node(&n).then_some(Target::Site(n))
            }
            (None, Some(f)) if valid_team(f) => Some(Target::Fox(f.to_string())),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Session {
    /// the id of the start event
    pub id: String,
    pub name: String,
    pub by: String,
    /// the key that started it; that station's app reveals the code words
    pub key: String,
    pub start: i64,
    /// when it ended, or is planned to end
    pub end: i64,
    pub ended: bool,
    pub teams: Vec<String>,
    /// callsign -> team, set by an admin
    pub roster: BTreeMap<String, String>,
    /// sites taken by checking in on the node's LAN
    pub sites: Vec<String>,
    /// public sites, taken with the code word heard on air there
    pub word_sites: Vec<String>,
    pub foxes: Vec<Fox>,
    /// the season it counts towards
    pub season: Option<String>,
    pub rules: Rules,
}

impl Session {
    pub fn running(&self, now: i64) -> bool {
        self.start <= now && now < self.end
    }

    /// Everything that sends code words.
    pub fn word_targets(&self) -> Vec<Target> {
        let sites = self.word_sites.iter().map(|n| Target::Site(n.clone()));
        let foxes = self.foxes.iter().map(|f| Target::Fox(f.name.clone()));
        sites.chain(foxes).collect()
    }

    pub fn fox(&self, name: &str) -> Option<&Fox> {
        self.foxes.iter().find(|f| f.name == name)
    }

    /// Every site a team can hold.
    pub fn all_sites(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sites.iter().chain(&self.word_sites).cloned().collect();
        v.sort();
        v.dedup();
        v
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum How {
    /// on the node's LAN, signed by this core
    Lan { key: String },
    /// with a code word heard on air
    Word { word: String },
}

/// A check-in or a code word claim.
#[derive(Clone, Debug, PartialEq)]
pub struct Play {
    pub id: String,
    pub ts: i64,
    pub call: String,
    pub team: String,
    pub target: Target,
    pub how: How,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Bearing {
    pub ts: i64,
    pub call: String,
    pub fox: String,
    pub from: Pos,
    /// degrees from true north
    pub deg: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Spot {
    pub ts: i64,
    pub call: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Season {
    pub id: String,
    pub name: String,
    pub start: i64,
    pub end: Option<i64>,
}

/// Game events, stored for reading later.
pub struct Game;

const GAME_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS game_events (
    event_id TEXT PRIMARY KEY,
    kind     TEXT NOT NULL,
    ts       INTEGER NOT NULL,
    lamport  INTEGER NOT NULL,
    key      TEXT NOT NULL,
    call     TEXT NOT NULL,
    payload  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS game_events_kind ON game_events(kind, ts);";

const KINDS: [&str; 9] = [
    "game.season",
    "game.start",
    "game.teams",
    "game.end",
    "game.checkin",
    "game.word",
    "game.reveal",
    "game.bearing",
    "game.spot",
];

impl Projection for Game {
    fn schema(&self) -> &'static str {
        GAME_SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute("DELETE FROM game_events", [])?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        if !KINDS.contains(&e.kind.as_str()) || !e.payload.is_object() {
            return Ok(());
        }
        let payload = e.payload.to_string();
        if payload.len() > PAYLOAD_MAX {
            return Ok(());
        }
        tx.execute(
            "INSERT OR IGNORE INTO game_events (event_id, kind, ts, lamport, key, call, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                e.id,
                e.kind,
                ts,
                e.lamport as i64,
                e.author.key,
                e.author.call,
                payload
            ],
        )?;
        Ok(())
    }
}

struct Row {
    id: String,
    kind: String,
    ts: i64,
    lamport: i64,
    key: String,
    call: String,
    payload: Value,
}

impl Row {
    fn about(&self, session: &str) -> bool {
        s(&self.payload, "session") == Some(session)
    }
}

/// Events of the given kinds, oldest first.
fn rows(conn: &Connection, kinds: &[&str]) -> Result<Vec<Row>> {
    let mut st = conn.prepare(
        "SELECT event_id, kind, ts, lamport, key, call, payload FROM game_events
         WHERE kind = ?1 ORDER BY ts, lamport, event_id",
    )?;
    let mut out = vec![];
    for kind in kinds {
        for r in st.query_map([kind], |r| {
            Ok(Row {
                id: r.get(0)?,
                kind: r.get(1)?,
                ts: r.get(2)?,
                lamport: r.get(3)?,
                key: r.get(4)?,
                call: r.get(5)?,
                payload: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or(Value::Null),
            })
        })? {
            out.push(r?);
        }
    }
    out.sort_by(|a, b| (a.ts, a.lamport, &a.id).cmp(&(b.ts, b.lamport, &b.id)));
    Ok(out)
}

fn str_list(v: Option<&Value>) -> Option<Vec<String>> {
    v?.as_array()?
        .iter()
        .map(|x| x.as_str().map(str::to_string))
        .collect()
}

fn node_list(p: &Value, key: &str) -> Result<Vec<String>> {
    let mut v: Vec<String> = match p.get(key) {
        None | Some(Value::Null) => vec![],
        Some(x) => str_list(Some(x))
            .with_context(|| format!("{key} must be a list of node names"))?
            .iter()
            .map(|n| norm_node(n))
            .collect(),
    };
    v.sort();
    v.dedup();
    if let Some(n) = v.iter().find(|n| !valid_node(n)) {
        bail!("{n:?} is not a node name");
    }
    Ok(v)
}

fn parse_teams(p: &Value) -> Option<Vec<String>> {
    let teams = str_list(p.get("teams"))?;
    check_teams(&teams).ok()?;
    Some(teams)
}

/// Callsign -> team; entries naming a team that isn't in `teams` are dropped.
fn parse_roster(p: &Value, teams: &[String]) -> BTreeMap<String, String> {
    p.get("roster")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(call, team)| {
            let team = team.as_str()?;
            (valid_call(call) && teams.iter().any(|t| t == team))
                .then(|| (call.clone(), team.to_string()))
        })
        .take(ROSTER_MAX)
        .collect()
}

fn parse_foxes(p: &Value) -> Result<Vec<Fox>> {
    let list = match p.get("foxes") {
        None | Some(Value::Null) => return Ok(vec![]),
        Some(v) => v.as_array().context("foxes must be a list")?,
    };
    if list.len() > FOXES_MAX {
        bail!("at most {FOXES_MAX} foxes");
    }
    let mut out = vec![];
    for f in list {
        let name = s(f, "name").context("each fox needs a name")?.to_string();
        let call = s(f, "call").map(str::to_string);
        if call.as_deref().is_some_and(|c| !valid_call(c)) {
            bail!(
                "fox {name:?}: {:?} is not a callsign",
                call.unwrap_or_default()
            );
        }
        out.push(Fox {
            name,
            call,
            at: None,
        });
    }
    let names: Vec<String> = out.iter().map(|f| f.name.clone()).collect();
    check_names("fox", &names)?;
    Ok(out)
}

/// What a `game.start` event asks for, checked.
pub struct Start {
    pub name: String,
    pub teams: Vec<String>,
    pub sites: Vec<String>,
    pub word_sites: Vec<String>,
    pub foxes: Vec<Fox>,
    pub season: Option<String>,
    pub rules: Rules,
    pub minutes: i64,
}

impl Start {
    /// Check a start payload; the admin command runs the same check before signing one.
    pub fn parse(p: &Value) -> Result<Self> {
        let name = s(p, "name").unwrap_or("").trim();
        if name.is_empty() || name.chars().count() > NAME_MAX {
            bail!("the name must be 1 to {NAME_MAX} characters");
        }
        let teams = str_list(p.get("teams")).context("teams must be a list of names")?;
        check_teams(&teams)?;
        let sites = node_list(p, "sites")?;
        let word_sites = node_list(p, "word_sites")?;
        let foxes = parse_foxes(p)?;
        if sites.is_empty() && word_sites.is_empty() && foxes.is_empty() {
            bail!("a game needs at least one check-in site, public site or fox");
        }
        if sites.len() + word_sites.len() > SITES_MAX {
            bail!("at most {SITES_MAX} sites");
        }
        let rules = match p.get("rules") {
            Some(v) => serde_json::from_value::<Rules>(v.clone()).context("reading the rules")?,
            None => Rules::default(),
        };
        rules.validate()?;
        // a game with no planned end stops after a week, which also bounds the scoring work
        let minutes = match p.get("minutes") {
            None | Some(Value::Null) => MINUTES_MAX,
            Some(m) => m
                .as_i64()
                .filter(|m| (1..=MINUTES_MAX).contains(m))
                .with_context(|| format!("minutes must be between 1 and {MINUTES_MAX}"))?,
        };
        Ok(Self {
            name: name.to_string(),
            teams,
            sites,
            word_sites,
            foxes,
            season: s(p, "season").map(str::to_string),
            rules,
            minutes,
        })
    }
}

fn parse_start(r: &Row) -> Option<Session> {
    let st = Start::parse(&r.payload).ok()?;
    Some(Session {
        id: r.id.clone(),
        name: st.name,
        by: r.call.clone(),
        key: r.key.clone(),
        start: r.ts,
        end: r.ts + st.minutes * MIN,
        ended: false,
        roster: parse_roster(&r.payload, &st.teams),
        teams: st.teams,
        sites: st.sites,
        word_sites: st.word_sites,
        foxes: st.foxes,
        season: st.season,
        rules: st.rules,
    })
}

/// `[lat, lon]`
fn pos(v: &Value) -> Option<Pos> {
    let a = v.as_array()?;
    Pos::new(a.first()?.as_f64()?, a.get(1)?.as_f64()?)
}

/// Apply the latest team list and the first end to a session.
fn apply_admin_rows(s: &mut Session, rows: &[&Row], now: i64) {
    // the latest team list wins, by lamport so every site picks the same one
    if let Some((teams, p)) = rows
        .iter()
        .filter(|r| r.kind == "game.teams")
        .filter_map(|r| Some((parse_teams(&r.payload)?, r)))
        .max_by(|(_, a), (_, b)| (a.lamport, &a.id).cmp(&(b.lamport, &b.id)))
        .map(|(t, r)| (t, &r.payload))
    {
        s.roster = parse_roster(p, &teams);
        s.teams = teams;
    }
    let ends: Vec<&&Row> = rows.iter().filter(|r| r.kind == "game.end").collect();
    if let Some(t) = ends.iter().map(|r| r.ts.max(s.start)).min()
        && t < s.end
    {
        s.end = t;
    }
    for r in &ends {
        for (name, at) in r
            .payload
            .get("foxes")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if let Some(f) = s.foxes.iter_mut().find(|f| f.name == *name) {
                f.at = pos(at).or(f.at);
            }
        }
    }
    s.ended = s.end <= now;
}

/// Every session started by an admin, newest first, with team changes and ends applied.
pub fn sessions(conn: &Connection, admins: &[String], now: i64) -> Result<Vec<Session>> {
    let admin = |k: &str| admins.iter().any(|a| a == k);
    let rows: Vec<Row> = rows(conn, &["game.start", "game.teams", "game.end"])?
        .into_iter()
        .filter(|r| admin(&r.key))
        .collect();
    let mut out: Vec<Session> = rows
        .iter()
        .filter(|r| r.kind == "game.start")
        .filter_map(parse_start)
        .collect();
    for s in &mut out {
        let mine: Vec<&Row> = rows.iter().filter(|r| r.about(&s.id)).collect();
        apply_admin_rows(s, &mine, now);
    }
    out.sort_by(|a, b| (b.start, &b.id).cmp(&(a.start, &a.id)));
    Ok(out)
}

/// The session running now, if any (the latest started, if two overlap).
pub fn current(sessions: &[Session], now: i64) -> Option<&Session> {
    sessions.iter().find(|s| s.running(now))
}

/// Seasons opened by admins, newest first.
pub fn seasons(conn: &Connection, admins: &[String]) -> Result<Vec<Season>> {
    let rows: Vec<Row> = rows(conn, &["game.season"])?
        .into_iter()
        .filter(|r| admins.contains(&r.key))
        .collect();
    let mut out: Vec<Season> = rows
        .iter()
        .filter(|r| s(&r.payload, "action") == Some("start"))
        .filter_map(|r| {
            let name = s(&r.payload, "name")?.trim();
            (!name.is_empty() && name.chars().count() <= NAME_MAX).then(|| Season {
                id: r.id.clone(),
                name: name.to_string(),
                start: r.ts,
                end: None,
            })
        })
        .collect();
    for season in &mut out {
        season.end = rows
            .iter()
            .filter(|r| {
                s(&r.payload, "action") == Some("end")
                    && s(&r.payload, "season") == Some(season.id.as_str())
            })
            .map(|r| r.ts.max(season.start))
            .min();
    }
    out.sort_by(|a, b| (b.start, &b.id).cmp(&(a.start, &a.id)));
    Ok(out)
}

/// The season open now, if any.
pub fn open_season(seasons: &[Season]) -> Option<&Season> {
    seasons.iter().find(|s| s.end.is_none())
}

fn player(p: &Value) -> Option<(String, String)> {
    let call = s(p, "call")?;
    let team = s(p, "team")?;
    (valid_call(call) && team.len() <= TEAM_MAX).then(|| (call.to_string(), team.to_string()))
}

/// Check-ins and code word claims for one session, oldest first. Whether each counts is
/// the scorer's job.
pub fn plays(conn: &Connection, session: &str) -> Result<Vec<Play>> {
    Ok(rows(conn, &["game.checkin", "game.word"])?
        .into_iter()
        .filter(|r| r.about(session))
        .filter_map(|r| {
            let (call, team) = player(&r.payload)?;
            let target = Target::from_payload(&r.payload)?;
            let how = if r.kind == "game.checkin" {
                matches!(target, Target::Site(_)).then_some(())?;
                How::Lan { key: r.key }
            } else {
                let word = s(&r.payload, "word")?;
                (word.len() <= 64).then_some(())?;
                How::Word {
                    word: word.to_string(),
                }
            };
            Some(Play {
                id: r.id,
                ts: r.ts,
                call,
                team,
                target,
                how,
            })
        })
        .collect())
}

/// The code words revealed for a session by the station that started it: (target key, slot) -> (word, when).
pub fn reveals(conn: &Connection, s_: &Session) -> Result<BTreeMap<(String, i64), (String, i64)>> {
    let mut out = BTreeMap::new();
    for r in rows(conn, &["game.reveal"])? {
        // only the station that started the game has its seed
        if !r.about(&s_.id) || r.key != s_.key {
            continue;
        }
        let (Some(t), Some(slot), Some(word)) = (
            Target::from_payload(&r.payload),
            r.payload.get("slot").and_then(Value::as_i64),
            s(&r.payload, "word"),
        ) else {
            continue;
        };
        // the first reveal stands; rows come oldest first
        out.entry((t.key(), slot))
            .or_insert((word.to_string(), r.ts));
    }
    Ok(out)
}

/// Bearings taken on a session's foxes, oldest first.
pub fn bearings(conn: &Connection, s_: &Session) -> Result<Vec<Bearing>> {
    Ok(rows(conn, &["game.bearing"])?
        .into_iter()
        .filter(|r| r.about(&s_.id))
        .filter_map(|r| {
            let p = &r.payload;
            let call = s(p, "call").filter(|c| valid_call(c))?;
            let fox = s(p, "fox").filter(|f| s_.fox(f).is_some())?;
            let deg = f(p, "deg").filter(|d| (0.0..360.0).contains(d))?;
            let from = Pos::new(f(p, "lat")?, f(p, "lon")?)?;
            Some(Bearing {
                ts: r.ts,
                call: call.to_string(),
                fox: fox.to_string(),
                from,
                deg,
            })
        })
        .collect())
}

/// Spots posted during a session, oldest first.
pub fn spots(conn: &Connection, session: &str) -> Result<Vec<Spot>> {
    Ok(rows(conn, &["game.spot"])?
        .into_iter()
        .filter(|r| r.about(session))
        .filter_map(|r| {
            let call = s(&r.payload, "call").filter(|c| valid_call(c))?;
            let text: String = s(&r.payload, "text")?
                .trim()
                .chars()
                .take(TEXT_MAX)
                .collect();
            (!text.is_empty()).then(|| Spot {
                ts: r.ts,
                call: call.to_string(),
                text,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_rules_are_valid() {
        Rules::default().validate().unwrap();
    }

    #[test]
    fn repeat_captures_are_worth_less() {
        let r = Rules::default();
        let worth: Vec<i64> = (1..=5).map(|n| r.capture_worth(n)).collect();
        assert_eq!(worth, [10, 5, 2, 1, 0]);
    }

    #[test]
    fn team_names() {
        assert!(valid_team("Red"));
        assert!(valid_team("North County-2"));
        assert!(!valid_team(" Red"));
        assert!(!valid_team("<b>"));
        assert!(check_teams(&["Red".into(), "red".into()]).is_err());
        assert!(check_teams(&["Red".into()]).is_err());
    }

    #[test]
    fn untyped_links_count_as_rf() {
        let r = Rules::default();
        assert!(r.counts(None));
        assert!(r.counts(Some("RF")));
        assert!(!r.counts(Some("TUN")));
    }

    #[test]
    fn start_payloads_are_checked() {
        let ok = json!({"name": "Hunt", "teams": ["Red", "Blue"],
            "foxes": [{"name": "Fox 1", "call": "N0FOX"}], "word_sites": ["TST-Park"]});
        let st = Start::parse(&ok).unwrap();
        assert_eq!(st.word_sites, ["tst-park"]);
        assert_eq!(st.minutes, MINUTES_MAX, "no planned end means a week");
        let none = json!({"name": "Empty", "teams": ["Red", "Blue"]});
        assert!(Start::parse(&none).is_err());
        let dup = json!({"name": "Dup", "teams": ["Red", "Blue"],
            "foxes": [{"name": "Fox"}, {"name": "fox"}]});
        assert!(Start::parse(&dup).is_err());
        let bad_window = json!({"name": "W", "teams": ["Red", "Blue"], "sites": ["a"],
            "rules": {"builder_days": 5, "builder_window_days": 4}});
        assert!(Start::parse(&bad_window).is_err());
    }
}
