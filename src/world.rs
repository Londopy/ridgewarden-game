//! What the mesh's own data says, at any moment of a session: which links were up, where
//! each site is, which core polls each site over its LAN, and which links are new enough
//! and old enough for the builder bonus.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::graph::{Edge, edge};
use ridgewarden_client::mesh::{MAX_LINKS_PER_POLL, f, s};
use ridgewarden_client::util::{norm_node, valid_node};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::game::{DAY, Rules};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    pub lat: f64,
    pub lon: f64,
}

impl Pos {
    /// A real location; 0,0 is what an unset one often looks like.
    pub fn new(lat: f64, lon: f64) -> Option<Self> {
        let ok = (-90.0..=90.0).contains(&lat)
            && (-180.0..=180.0).contains(&lon)
            && (lat, lon) != (0.0, 0.0);
        ok.then_some(Pos { lat, lon })
    }

    /// The point `km` away towards `deg` (from true north); flat-earth, fine for a mesh.
    pub fn toward(self, deg: f64, km: f64) -> Pos {
        let d = deg.to_radians();
        Pos {
            lat: self.lat + km * d.cos() / 111.32,
            lon: self.lon + km * d.sin() / (111.32 * self.lat.to_radians().cos().max(0.01)),
        }
    }
}

/// Great-circle distance in km.
pub fn km(a: Pos, b: Pos) -> f64 {
    let (la1, la2) = (a.lat.to_radians(), b.lat.to_radians());
    let dla = la2 - la1;
    let dlo = (b.lon - a.lon).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * 6371.0 * h.sqrt().min(1.0).asin()
}

/// Small tables kept up from every poll, so scoring doesn't scan the polls: each node's
/// location and LAN cores, the days each node was polled and each link seen (kept
/// through a rebuild, since the core prunes polls after 30 days and a link's age is the
/// point), and the oldest poll time that arrived since scores were last worked out.
pub struct Seen;

const SEEN_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS node_days (
    node TEXT NOT NULL,
    day  INTEGER NOT NULL,
    PRIMARY KEY (node, day)
);
CREATE TABLE IF NOT EXISTS link_days (
    a   TEXT NOT NULL,
    b   TEXT NOT NULL,
    day INTEGER NOT NULL,
    PRIMARY KEY (a, b, day)
);
CREATE TABLE IF NOT EXISTS positions (
    node TEXT PRIMARY KEY,
    lat  REAL NOT NULL,
    lon  REAL NOT NULL,
    ts   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS lan_cores (
    node TEXT NOT NULL,
    key  TEXT NOT NULL,
    PRIMARY KEY (node, key)
);
CREATE TABLE IF NOT EXISTS dirty (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    ts INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS polls_ts ON polls(ts);";

fn mark_dirty(tx: &Transaction, ts: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO dirty (id, ts) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET ts = min(ts, excluded.ts)",
        [ts],
    )?;
    Ok(())
}

/// Record the day for each link a poll lists; returns how many were new for that day.
fn record_links(tx: &Transaction, node: &str, p: &Value, day: i64) -> Result<usize> {
    let mut new = 0;
    for l in p
        .get("links")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_LINKS_PER_POLL)
    {
        let nb = norm_node(s(l, "neighbor").unwrap_or(""));
        if !valid_node(&nb) || nb == node {
            continue;
        }
        let (a, b) = edge(node, &nb);
        new += tx.execute(
            "INSERT OR IGNORE INTO link_days (a, b, day) VALUES (?1, ?2, ?3)",
            params![a, b, day],
        )?;
    }
    Ok(new)
}

impl Projection for Seen {
    fn schema(&self) -> &'static str {
        SEEN_SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch("DELETE FROM positions; DELETE FROM lan_cores;")?;
        mark_dirty(tx, i64::MIN)
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        let p = &e.payload;
        let node = norm_node(s(p, "node").unwrap_or(""));
        if e.kind != "node.poll" || !valid_node(&node) {
            return Ok(());
        }
        mark_dirty(tx, ts)?;
        if s(p, "via") == Some("lan") {
            tx.execute(
                "INSERT OR IGNORE INTO lan_cores (node, key) VALUES (?1, ?2)",
                params![node, e.author.key],
            )?;
        }
        if p.get("ok").and_then(Value::as_bool) != Some(true) {
            return Ok(());
        }
        if let Some(pos) = f(p, "lat")
            .zip(f(p, "lon"))
            .and_then(|(a, b)| Pos::new(a, b))
        {
            tx.execute(
                "INSERT INTO positions (node, lat, lon, ts) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(node) DO UPDATE SET lat = excluded.lat, lon = excluded.lon,
                 ts = excluded.ts WHERE excluded.ts >= ts",
                params![node, pos.lat, pos.lon, ts],
            )?;
        }
        let day = ts.div_euclid(DAY);
        let new_day = tx.execute(
            "INSERT OR IGNORE INTO node_days (node, day) VALUES (?1, ?2)",
            params![node, day],
        )? + record_links(tx, &node, p, day)?;
        if new_day > 0 {
            // a new day for a node or link can change which links are new from that day on
            mark_dirty(tx, day * DAY)?;
        }
        Ok(())
    }
}

/// The oldest poll time that arrived since the last call, if any did.
pub fn take_dirty(conn: &Connection) -> Result<Option<i64>> {
    let ts = conn
        .query_row("SELECT ts FROM dirty WHERE id = 1", [], |r| r.get(0))
        .optional()?;
    conn.execute("DELETE FROM dirty", [])?;
    Ok(ts)
}

/// One poll of one node: the neighbors it listed and each link's type.
type Poll = (i64, BTreeMap<String, Option<String>>);

#[derive(Default)]
pub struct World {
    /// successful polls per node in the loaded window, oldest first
    polls: BTreeMap<String, Vec<Poll>>,
    pub positions: BTreeMap<String, Pos>,
    /// node -> keys of the cores that poll it over the node's own LAN
    pub lan_keys: BTreeMap<String, BTreeSet<String>>,
    /// first day each node was polled
    node_first: BTreeMap<String, i64>,
    /// the days each link was seen, in order
    link_days: BTreeMap<Edge, Vec<i64>>,
    /// the oldest poll we still hold
    pub oldest_poll: Option<i64>,
}

impl World {
    /// Everything but the polls themselves.
    pub fn places(conn: &Connection) -> Result<Self> {
        let mut w = World::default();
        let mut st = conn.prepare("SELECT node, lat, lon FROM positions")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?)))? {
            let (node, lat, lon) = r?;
            w.positions.insert(node, Pos { lat, lon });
        }
        let mut st = conn.prepare("SELECT node, key FROM lan_cores")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
            let (node, key) = r?;
            w.lan_keys.entry(node).or_default().insert(key);
        }
        let mut st = conn.prepare("SELECT node, min(day) FROM node_days GROUP BY node")?;
        for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
            let (node, day) = r?;
            w.node_first.insert(node, day);
        }
        let mut st = conn.prepare("SELECT a, b, day FROM link_days ORDER BY day")?;
        for r in st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?))
        })? {
            let (a, b, day) = r?;
            w.link_days.entry(edge(&a, &b)).or_default().push(day);
        }
        w.oldest_poll = conn.query_row("SELECT min(ts) FROM polls", [], |r| r.get(0))?;
        Ok(w)
    }

    /// Load the successful polls from `from` to `to`, replacing any loaded before.
    pub fn load_polls(&mut self, conn: &Connection, from: i64, to: i64) -> Result<()> {
        let mut links: BTreeMap<String, BTreeMap<String, Option<String>>> = BTreeMap::new();
        let mut st = conn.prepare(
            "SELECT event_id, neighbor, link_type FROM link_samples WHERE ts BETWEEN ?1 AND ?2",
        )?;
        for r in st.query_map(params![from, to], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?))
        })? {
            let (id, nb, ty) = r?;
            links.entry(id).or_default().insert(nb, ty);
        }
        self.polls.clear();
        let mut st = conn.prepare(
            "SELECT node, ts, event_id FROM polls WHERE ok = 1 AND ts BETWEEN ?1 AND ?2 ORDER BY ts",
        )?;
        for r in st.query_map(params![from, to], |r| {
            Ok((r.get::<_, String>(0)?, r.get(1)?, r.get::<_, String>(2)?))
        })? {
            let (node, ts, id) = r?;
            let nbs = links.remove(&id).unwrap_or_default();
            self.polls.entry(node).or_default().push((ts, nbs));
        }
        Ok(())
    }

    /// Links up at `t` by the mesh's own data, with the same rule as Link Doctor: an end
    /// polled within `fresh_s` lists the other, and one poll without it is a hiccup, so a
    /// link is only gone after two polls in a row without it.
    pub fn up_at(&self, t: i64, rules: &Rules) -> BTreeSet<Edge> {
        let mut up = BTreeSet::new();
        for (node, polls) in &self.polls {
            let i = polls.partition_point(|p| p.0 <= t);
            if i == 0 || polls[i - 1].0 < t - rules.fresh_s * 1000 {
                continue;
            }
            // the poll before counts too, unless it's so old it says nothing about now
            let first = if i >= 2 && polls[i - 2].0 >= t - 2 * rules.fresh_s * 1000 {
                i - 2
            } else {
                i - 1
            };
            for (_, nbs) in &polls[first..i] {
                for (nb, ty) in nbs {
                    if nb != node && rules.counts(ty.as_deref()) {
                        up.insert(edge(node, nb));
                    }
                }
            }
        }
        up
    }

    /// Every link of a counted type in the loaded polls from `since` on, up or not.
    pub fn known_since(&self, since: i64, rules: &Rules) -> BTreeSet<Edge> {
        let mut out = BTreeSet::new();
        for (node, polls) in &self.polls {
            let recent = polls.iter().filter(|(ts, _)| *ts >= since);
            for (nb, ty) in recent.flat_map(|(_, nbs)| nbs) {
                if nb != node && rules.counts(ty.as_deref()) {
                    out.insert(edge(node, nb));
                }
            }
        }
        out
    }

    /// The day a link first showed up, if one of its ends was polled on an earlier day
    /// without it: proof it wasn't there before.
    fn appeared(&self, e: &Edge) -> Option<i64> {
        let first = *self.link_days.get(e)?.first()?;
        [&e.0, &e.1]
            .iter()
            .filter_map(|n| self.node_first.get(*n))
            .min()
            .is_some_and(|polled| *polled < first)
            .then_some(first)
    }

    /// A link earns the builder bonus at time `t` if it is new (it appeared less than
    /// `builder_window_days` ago) and has shown up on `builder_days` distinct days by then.
    pub fn is_new_link(&self, e: &Edge, t: i64, rules: &Rules) -> bool {
        let day = t.div_euclid(DAY);
        self.appeared(e).is_some_and(|first| {
            day - first < rules.builder_window_days
                && self.link_days[e].iter().filter(|d| **d <= day).count() as i64
                    >= rules.builder_days
        })
    }

    /// Links that appeared after this app started watching, with the day they did.
    pub fn new_links(&self) -> Vec<(Edge, i64)> {
        self.link_days
            .keys()
            .filter_map(|e| Some((e.clone(), self.appeared(e)?)))
            .collect()
    }

    /// Nodes first polled after this app started watching, with the day they were.
    pub fn new_nodes(&self) -> Vec<(String, i64)> {
        let Some(begin) = self.node_first.values().min() else {
            return vec![];
        };
        self.node_first
            .iter()
            .filter(|(_, d)| *d > begin)
            .map(|(n, d)| (n.clone(), *d))
            .collect()
    }

    pub fn has_lan_core(&self, site: &str) -> bool {
        self.lan_keys.get(site).is_some_and(|k| !k.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_between_two_hills() {
        // about 0.1 degree of latitude apart
        let a = Pos::new(35.30, -120.66).unwrap();
        let b = Pos::new(35.40, -120.66).unwrap();
        assert!((km(a, b) - 11.12).abs() < 0.05, "{}", km(a, b));
        assert_eq!(km(a, a), 0.0);
        let east = a.toward(90.0, 10.0);
        assert!((km(a, east) - 10.0).abs() < 0.1, "{}", km(a, east));
        assert!(Pos::new(0.0, 0.0).is_none());
        assert!(Pos::new(91.0, 0.0).is_none());
    }
}
