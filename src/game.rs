//! Game events and what they say: seasons and sessions started and ended by admins, team
//! lists and player keys set by admins, and the players' own posts: check-ins
//! countersigned by the site they were made at, code words for public sites, fox codes,
//! bearings and spots, each signed by the player's key as well as by a site's core. Events
//! are stored as they came; who may do what is decided when reading, so a change to the
//! admin list needs no rebuild.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use ed25519_dalek::{Signature, VerifyingKey};
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::event::{canonical_string, valid_call};
use ridgewarden_client::mesh::s;
use ridgewarden_client::util::{norm_node, now_ms, valid_node};
use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::world::{Pos, World};

pub const MIN: i64 = 60_000;
pub const DAY: i64 = 86_400_000;
pub const NAME_MAX: usize = 80;
pub const TEAM_MAX: usize = 24;
pub const TEAMS_MAX: usize = 4;
pub const SITES_MAX: usize = 100;
pub const FOXES_MAX: usize = 20;
/// one-time codes per fox, and in a whole game; each is 27 bytes of the start event
pub const CODES_MAX: usize = 100;
const GAME_CODES_MAX: usize = 500;
pub const ROSTER_MAX: usize = 500;
/// longest spot text
pub const TEXT_MAX: usize = 140;
/// longest bearing, and latitude or longitude, as typed; the page writes at most 5 and 10
pub const DEG_MAX: usize = 8;
pub const POS_MAX: usize = 12;
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
    /// a held site goes back to nobody once every link at it has been down this long
    pub hold_min: i64,
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
            hold_min: 30,
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
        within("hold_min", self.hold_min, 5, 1440)?;
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

/// A team list: 2 to 4 valid names, no two the same ignoring case.
pub fn check_teams(teams: &[String]) -> Result<()> {
    if !(2..=TEAMS_MAX).contains(&teams.len()) {
        bail!("a game needs 2 to {TEAMS_MAX} teams");
    }
    check_names("team", teams)
}

/// A hidden transmitter. Whoever finds it takes one of the one-time codes left with it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Fox {
    pub name: String,
    /// whoever hid and runs it, credited for each finder
    pub call: Option<String>,
    /// where it was, once an admin says at the end
    pub at: Option<Pos>,
    /// hashes of its one-time codes (`words::code_hash`), so any site can check a code
    #[serde(default)]
    pub codes: Vec<String>,
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
    /// mixed into the fox codes and their hashes, so no two games share a code
    #[serde(default)]
    pub nonce: String,
    /// the season it counts towards
    pub season: Option<String>,
    pub rules: Rules,
}

impl Session {
    pub fn running(&self, now: i64) -> bool {
        self.start <= now && now < self.end
    }

    /// The public sites, which send rotating code words.
    pub fn word_targets(&self) -> Vec<Target> {
        self.word_sites
            .iter()
            .map(|n| Target::Site(n.clone()))
            .collect()
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
    /// with a code word heard on air, or a fox's one-time code
    Word { word: String },
}

/// A check-in, a code word claim or a fox code.
#[derive(Clone, Debug, PartialEq)]
pub struct Play {
    pub id: String,
    pub ts: i64,
    pub call: String,
    pub team: String,
    pub target: Target,
    pub how: How,
    /// the player key that signed it, if the signature checked out
    pub key: Option<String>,
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
CREATE INDEX IF NOT EXISTS game_events_kind ON game_events(kind, ts);
CREATE TABLE IF NOT EXISTS signers (
    event_id TEXT PRIMARY KEY,
    key      TEXT NOT NULL,
    call     TEXT NOT NULL,
    session  TEXT NOT NULL,
    site     TEXT
);";

/// What players post. Each carries the player's key (`pk`) and signature (`psig`).
pub const PLAYER_KINDS: [&str; 4] = ["game.checkin", "game.word", "game.bearing", "game.spot"];

const KINDS: [&str; 10] = [
    "game.season",
    "game.start",
    "game.teams",
    "game.end",
    "game.key",
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
        tx.execute_batch("DELETE FROM game_events; DELETE FROM signers;")?;
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
        // checked once here rather than on every scoring
        if PLAYER_KINDS.contains(&e.kind.as_str())
            && let Some(key) = signer(&e.kind, &e.payload)
        {
            let p = &e.payload;
            tx.execute(
                "INSERT OR IGNORE INTO signers (event_id, key, call, session, site)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    e.id,
                    key,
                    s(p, "call").unwrap_or_default(),
                    s(p, "session").unwrap_or_default(),
                    s(p, "site")
                ],
            )?;
        }
        Ok(())
    }
}

/// The text a player signs for a post: its payload without `psig`, plus the event kind,
/// as canonical JSON. The page's script builds the same text. A payload with a `kind` of
/// its own has none, since the event's kind would take its place unsigned.
pub fn signed_text(kind: &str, payload: &Value) -> Option<String> {
    let mut m = payload.as_object()?.clone();
    if m.contains_key("kind") {
        return None;
    }
    m.remove("psig");
    m.insert("kind".into(), kind.into());
    Some(canonical_string(&Value::Object(m)))
}

/// A player key as the pages show it: 64 lower-case hex digits of a usable Ed25519 key.
pub fn valid_key(k: &str) -> bool {
    hex::decode(k).ok().is_some_and(|b| {
        hex::encode(&b) == k
            && <[u8; 32]>::try_from(b).is_ok_and(|b| VerifyingKey::from_bytes(&b).is_ok())
    })
}

/// The player key that signed a post, if its `pk` and `psig` check out.
pub fn signer(kind: &str, payload: &Value) -> Option<String> {
    let pk = s(payload, "pk").filter(|k| valid_key(k))?;
    let key: [u8; 32] = hex::decode(pk).ok()?.try_into().ok()?;
    let psig = s(payload, "psig")?;
    let sig: [u8; 64] = hex::decode(psig).ok()?.try_into().ok()?;
    // one spelling per signature, or a replay in capitals would pass as a new post
    if hex::encode(sig) != psig {
        return None;
    }
    // verify_strict refuses the weak keys that would "verify" anything
    VerifyingKey::from_bytes(&key)
        .ok()?
        .verify_strict(
            signed_text(kind, payload)?.as_bytes(),
            &Signature::from_bytes(&sig),
        )
        .ok()?;
    Some(pk.to_string())
}

struct Row {
    id: String,
    kind: String,
    ts: i64,
    lamport: i64,
    key: String,
    call: String,
    payload: Value,
    /// for a player's post, the player key whose signature checked out
    signer: Option<String>,
}

impl Row {
    fn about(&self, session: &str) -> bool {
        s(&self.payload, "session") == Some(session)
    }

    /// Whether a player's post counts for `call`: signed by the key it plays with, or by
    /// any player key while it has none.
    fn by(&self, call: &str, keys: &Keys) -> bool {
        match keys.get(call) {
            Some(k) => self.signer.as_ref() == Some(k),
            None => self.signer.is_some(),
        }
    }
}

/// Events of the given kinds, oldest first.
fn rows(conn: &Connection, kinds: &[&str]) -> Result<Vec<Row>> {
    let mut st = conn.prepare(
        "SELECT e.event_id, e.kind, e.ts, e.lamport, e.key, e.call, e.payload, s.key
         FROM game_events e LEFT JOIN signers s ON s.event_id = e.event_id
         WHERE e.kind = ?1 ORDER BY e.ts, e.lamport, e.event_id",
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
                signer: r.get(7)?,
            })
        })? {
            out.push(r?);
        }
    }
    out.sort_by(|a, b| (a.ts, a.lamport, &a.id).cmp(&(b.ts, b.lamport, &b.id)));
    Ok(out)
}

/// Players' posts of the given kinds, oldest first. A signed post that shows up again with
/// the same signature, sent on by someone who saw it, is left out, and so is a copy whose
/// signature was respelled so it no longer checks out, whichever came first.
fn posts(conn: &Connection, kinds: &[&str]) -> Result<Vec<Row>> {
    let rows = rows(conn, kinds)?;
    let good: BTreeSet<String> = rows
        .iter()
        .filter(|r| r.signer.is_some())
        .filter_map(|r| s(&r.payload, "psig"))
        .map(str::to_ascii_lowercase)
        .collect();
    let mut seen = BTreeSet::new();
    Ok(rows
        .into_iter()
        .filter(|r| match (&r.signer, s(&r.payload, "psig")) {
            (Some(_), Some(sig)) => seen.insert(sig.to_string()),
            (None, Some(sig)) => !good.contains(&sig.to_ascii_lowercase()),
            _ => true,
        })
        .collect())
}

/// Callsign -> the player key it plays with.
pub type Keys = BTreeMap<String, String>;

/// Each callsign's player key: the key of its first post that a game's rules would accept
/// on their own, unless an admin set another with `player-key`, in which case the latest
/// one an admin set. In a game with a roster that is any post by a rostered callsign;
/// otherwise only a check-in countersigned by the core that polls that site over its LAN,
/// so nobody can take a callsign from afar. First is by lamport, not by any core's clock.
pub fn keys(conn: &Connection, admins: &[String]) -> Result<Keys> {
    let games: BTreeMap<String, Session> = sessions(conn, admins, now_ms())?
        .into_iter()
        .map(|g| (g.id.clone(), g))
        .collect();
    let lan = World::places(conn)?.lan_keys;
    let mut st = conn.prepare(
        "SELECT e.kind, e.key, s.key, s.call, s.session, s.site
         FROM signers s JOIN game_events e ON e.event_id = s.event_id
         ORDER BY e.lamport, e.event_id",
    )?;
    let mut signed = st.query([])?;
    let mut out = Keys::new();
    while let Some(r) = signed.next()? {
        let (kind, core, key, call, session): (String, String, String, String, String) =
            (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
        let site: Option<String> = r.get(5)?;
        let Some(g) = games.get(&session) else {
            continue;
        };
        let accepted = if g.roster.is_empty() {
            kind == "game.checkin"
                && site.is_some_and(|n| {
                    g.sites.contains(&n) && lan.get(&n).is_some_and(|k| k.contains(&core))
                })
        } else {
            g.roster.contains_key(&call)
        };
        if accepted && valid_call(&call) {
            out.entry(call).or_insert(key);
        }
    }
    let mut set: Vec<Row> = rows(conn, &["game.key"])?
        .into_iter()
        .filter(|r| admins.contains(&r.key))
        .collect();
    // by lamport, like team lists, so every site picks the same one
    set.sort_by(|a, b| (a.lamport, &a.id).cmp(&(b.lamport, &b.id)));
    for r in set {
        if let (Some(call), Some(key)) = (s(&r.payload, "call"), s(&r.payload, "key"))
            && valid_call(call)
            && valid_key(key)
        {
            out.insert(call.to_string(), key.to_string());
        }
    }
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
        let codes = match f.get("codes") {
            None | Some(Value::Null) => vec![],
            Some(c) => str_list(Some(c)).with_context(|| format!("fox {name:?}: bad codes"))?,
        };
        let hashes_ok = codes
            .iter()
            .all(|h| h.len() == 24 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        if !hashes_ok || codes.len() > CODES_MAX {
            bail!("fox {name:?}: codes must be up to {CODES_MAX} code hashes");
        }
        out.push(Fox {
            name,
            call,
            at: None,
            codes,
        });
    }
    let names: Vec<String> = out.iter().map(|f| f.name.clone()).collect();
    check_names("fox", &names)?;
    if out.iter().map(|f| f.codes.len()).sum::<usize>() > GAME_CODES_MAX {
        bail!("at most {GAME_CODES_MAX} fox codes in one game");
    }
    Ok(out)
}

/// What a `game.start` event asks for, checked.
pub struct Start {
    pub name: String,
    pub teams: Vec<String>,
    pub sites: Vec<String>,
    pub word_sites: Vec<String>,
    pub foxes: Vec<Fox>,
    pub nonce: String,
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
        let nonce = s(p, "nonce").unwrap_or("");
        if nonce.len() > 64 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("nonce must be up to 64 hex digits");
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
            nonce: nonce.to_string(),
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
        nonce: st.nonce,
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

/// Check-ins, code word claims and fox codes for one session, oldest first. Whether each
/// counts is the scorer's job.
pub fn plays(conn: &Connection, session: &str) -> Result<Vec<Play>> {
    Ok(posts(conn, &["game.checkin", "game.word"])?
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
                key: r.signer,
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

/// A number a player's post carries as text, as signed, if the text is at most `max` long.
pub fn num(p: &Value, key: &str, max: usize) -> Option<f64> {
    s(p, key)
        .filter(|v| v.len() <= max)?
        .trim()
        .parse()
        .ok()
        .filter(|x: &f64| x.is_finite())
}

/// Bearings taken on a session's foxes, signed by the players' own keys, oldest first.
pub fn bearings(conn: &Connection, s_: &Session, keys: &Keys) -> Result<Vec<Bearing>> {
    Ok(posts(conn, &["game.bearing"])?
        .into_iter()
        .filter(|r| r.about(&s_.id))
        .filter_map(|r| {
            let p = &r.payload;
            let call = s(p, "call").filter(|c| valid_call(c) && r.by(c, keys))?;
            let fox = s(p, "fox").filter(|f| s_.fox(f).is_some())?;
            let deg = num(p, "deg", DEG_MAX).filter(|d| (0.0..360.0).contains(d))?;
            let from = Pos::new(num(p, "lat", POS_MAX)?, num(p, "lon", POS_MAX)?)?;
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

/// Spots posted during a session, signed by the players' own keys, oldest first.
pub fn spots(conn: &Connection, session: &str, keys: &Keys) -> Result<Vec<Spot>> {
    Ok(posts(conn, &["game.spot"])?
        .into_iter()
        .filter(|r| r.about(session))
        .filter_map(|r| {
            let call = s(&r.payload, "call").filter(|c| valid_call(c) && r.by(c, keys))?;
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
        let four =
            json!({"name": "Four", "teams": ["Red", "Blue", "Green", "Gold"], "sites": ["a"]});
        assert!(Start::parse(&four).is_ok());
        let five = json!({"name": "Five", "teams": ["Red", "Blue", "Green", "Gold", "Gray"],
            "sites": ["a"]});
        assert!(Start::parse(&five).is_err());
        let dup = json!({"name": "Dup", "teams": ["Red", "Blue"],
            "foxes": [{"name": "Fox"}, {"name": "fox"}]});
        assert!(Start::parse(&dup).is_err());
        let bad_window = json!({"name": "W", "teams": ["Red", "Blue"], "sites": ["a"],
            "rules": {"builder_days": 5, "builder_window_days": 4}});
        assert!(Start::parse(&bad_window).is_err());
    }

    #[test]
    fn a_signature_checks_out_only_as_it_was_made() {
        use crate::testing::{pk, player_key, signed};
        let key = player_key("N0AAA");
        let p = json!({"session": "s1", "call": "N0AAA", "text": "hi", "t": 1});
        let p = signed("game.spot", p, &key);
        assert_eq!(signer("game.spot", &p), Some(pk(&key)));
        assert_eq!(signer("game.word", &p), None, "under another kind");
        let mut shouted = p.clone();
        shouted["psig"] = json!(p["psig"].as_str().unwrap().to_uppercase());
        assert_eq!(
            signer("game.spot", &shouted),
            None,
            "the same signature in capitals"
        );
        let mut with_kind = p.clone();
        with_kind["kind"] = json!("game.spot");
        assert_eq!(
            signer("game.spot", &with_kind),
            None,
            "a payload with its own kind"
        );
    }
}
