//! Test helpers: a fake mesh of three check-in sites and a relay, and games played on it.

use ridgewarden_client::core::{Feed, FeedItem};
use ridgewarden_client::mesh::Mesh;
use ridgewarden_client::mesh::testing::Fake;
use ridgewarden_client::util::ms_to_rfc3339;
use ridgewarden_client::util::now_ms;
use ridgewarden_client::{Author, Event};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::game::{self, DAY, Game, MIN};
use crate::score::Minutes;
use crate::score::Played;
use crate::world::Seen;
use crate::{PROJECTIONS, RESULTS_SCHEMA};

/// Midnight UTC, so tests about days start on a day boundary.
pub const T0: i64 = 20_833 * DAY;

/// a and b are 5.6 km apart, c is 55 km north of a, r is a relay between them.
fn pos(node: &str) -> (f64, f64) {
    match node {
        "a" => (35.0, -120.0),
        "b" => (35.05, -120.0),
        "c" => (35.5, -120.0),
        _ => (35.02, -120.01),
    }
}

pub struct G {
    pub f: Fake,
    /// the fake station's key: admin, and the core of every site
    pub key: String,
}

impl G {
    pub fn new() -> Self {
        let mut f = Fake::new(vec![Box::new(Mesh), Box::new(Game), Box::new(Seen)]);
        f.conn.execute_batch(RESULTS_SCHEMA).unwrap();
        let e = f.add("x.hello", json!({}), 0);
        G {
            f,
            key: e.author.key,
        }
    }

    pub fn poll(&mut self, ts: i64, node: &str, links: &[&str]) {
        let links: Vec<Value> = links
            .iter()
            .map(|n| json!({"neighbor": n, "type": "RF", "snr": 25.0}))
            .collect();
        let (lat, lon) = pos(node);
        self.f.add(
            "node.poll",
            json!({"node": node, "ok": true, "via": "lan", "lat": lat, "lon": lon, "links": links}),
            ts,
        );
    }

    /// Polls of `node` every minute from `from` to `to`, listing `links`.
    pub fn polls(&mut self, from: i64, to: i64, node: &str, links: &[&str]) {
        let mut t = from;
        while t <= to {
            self.poll(t, node, links);
            t += MIN;
        }
    }

    /// Start a game at a, b and c, each polled over its LAN by our core half an hour before.
    pub fn start(&mut self, ts: i64, extra: Value) -> String {
        for site in ["a", "b", "c"] {
            self.poll(ts - 30 * MIN, site, &[]);
        }
        let mut p =
            json!({"name": "Test game", "teams": ["Red", "Blue"], "sites": ["a", "b", "c"]});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        self.f.add("game.start", p, ts).id
    }

    pub fn checkin(
        &mut self,
        ts: i64,
        session: &str,
        call: &str,
        team: &str,
        site: &str,
    ) -> String {
        self.f
            .add(
                "game.checkin",
                json!({"session": session, "call": call, "team": team, "site": site}),
                ts,
            )
            .id
    }

    /// An event from another station, applied straight to the projections.
    pub fn add_as(&mut self, key: &str, lamport: u64, kind: &str, payload: Value, ts: i64) {
        let e = unsigned(key, lamport, kind, payload, ts);
        let tx = self.f.conn.transaction().unwrap();
        for p in PROJECTIONS {
            p.apply(&tx, &e, ts).unwrap();
        }
        tx.commit().unwrap();
    }

    pub fn sessions(&self, now: i64) -> Vec<game::Session> {
        game::sessions(&self.f.conn, std::slice::from_ref(&self.key), now).unwrap()
    }

    /// The latest game, scored as of `now`.
    pub fn played(&self, now: i64) -> Played {
        self.played_with(now, &mut Minutes::default())
    }

    /// The latest game, scored as of `now`, reusing the minutes worked out before. Polls
    /// that arrived since then are noted the way the app does.
    pub fn played_with(&self, now: i64, minutes: &mut Minutes) -> Played {
        if let Some(ts) = crate::world::take_dirty(&self.f.conn).unwrap() {
            minutes.touch(ts);
        }
        let s = self.sessions(now).into_iter().next().expect("a game");
        crate::played(&self.f.conn, &s, now, now, minutes).unwrap()
    }

    /// A code word claim for a site or fox (`target` is {"site": ..} or {"fox": ..}).
    pub fn word(
        &mut self,
        ts: i64,
        session: &str,
        call: &str,
        team: &str,
        target: Value,
        word: &str,
    ) {
        let mut p = json!({"session": session, "call": call, "team": team, "word": word});
        for (k, v) in target.as_object().unwrap() {
            p[k] = v.clone();
        }
        self.f.add("game.word", p, ts);
    }

    pub fn reveal(&mut self, ts: i64, session: &str, target: Value, slot: i64, word: &str) {
        let mut p = json!({"session": session, "slot": slot, "word": word});
        for (k, v) in target.as_object().unwrap() {
            p[k] = v.clone();
        }
        self.f.add("game.reveal", p, ts);
    }
}

pub fn team<'a>(p: &'a Played, name: &str) -> &'a crate::score::TeamScore {
    p.teams.iter().find(|t| t.team == name).unwrap()
}

pub fn player<'a>(p: &'a Played, call: &str) -> &'a crate::score::PlayerScore {
    p.players.iter().find(|x| x.call == call).unwrap()
}

/// An event `Fake` can't sign for. Apps don't check signatures (the core does), so an
/// unsigned one is enough here.
pub fn unsigned(key: &str, lamport: u64, kind: &str, payload: Value, ts: i64) -> Event {
    Event {
        id: format!("{lamport:064x}"),
        author: Author {
            call: "N0OTHER".into(),
            key: key.into(),
        },
        seq: lamport,
        prev: None,
        lamport,
        wall_time: ms_to_rfc3339(ts),
        mono_time: 0,
        kind: kind.into(),
        payload,
        sig: String::new(),
        log: None,
    }
}

/// A stand-in for the core's local API: whoami, the feed, and append, which logs the
/// event as the core's own.
pub struct FakeCore {
    pub url: String,
    pub key: String,
    pub feed: Arc<Mutex<Vec<FeedItem>>>,
}

impl FakeCore {
    /// A core at node `node` holding `events` (kind, payload, ts), all its own.
    pub async fn start(node: &str, events: &[(&str, Value, i64)]) -> Self {
        let key = "ab".repeat(32);
        let feed: Arc<Mutex<Vec<FeedItem>>> = Arc::default();
        for (kind, payload, ts) in events {
            push(&feed, &key, kind, payload.clone(), *ts);
        }
        let me = json!({"call": "N0CALL", "key": key, "node": node});
        let (f, k) = (feed.clone(), key.clone());
        let app = Router::new()
            .route("/v1/whoami", get(move || async move { Json(me) }))
            .route(
                "/v1/feed",
                get(|State(f): State<Arc<Mutex<Vec<FeedItem>>>>, Query(q): Query<BTreeMap<String, i64>>| async move {
                    let after = q.get("after").copied().unwrap_or(-1);
                    let events = f.lock().unwrap().iter().filter(|i| i.seq > after).cloned().collect();
                    Json(Feed { instance: "test".into(), events })
                }),
            )
            .route(
                "/local/v1/append",
                post(move |State(f): State<Arc<Mutex<Vec<FeedItem>>>>, Json(b): Json<Value>| async move {
                    Json(push(&f, &k, b["kind"].as_str().unwrap(), b["payload"].clone(), now_ms()))
                }),
            )
            .with_state(f);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        FakeCore { url, key, feed }
    }

    /// Log an event as the core's own, as if something else had appended it.
    pub fn add(&self, kind: &str, payload: Value, ts: i64) {
        push(&self.feed, &self.key, kind, payload, ts);
    }

    pub fn appended(&self, kind: &str) -> usize {
        self.feed
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.event.kind == kind)
            .count()
    }
}

fn push(feed: &Mutex<Vec<FeedItem>>, key: &str, kind: &str, payload: Value, ts: i64) -> Event {
    let mut f = feed.lock().unwrap();
    let seq = f.len() as i64;
    let e = unsigned(key, seq as u64, kind, payload, ts);
    f.push(FeedItem {
        seq,
        event: e.clone(),
        trusted: true,
        seen_ms: Some(ts),
    });
    e
}
