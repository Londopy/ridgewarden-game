//! ridgewarden-game: Capture the Site on a real AREDN mesh. Teams check in at sites in
//! person, and score for chains of their sites while the RF links between them are up in
//! the mesh's own data; a site whose links stay down goes back to nobody. Fox hunts,
//! public sites taken with code words heard on air, seasons and awards are built on the
//! same log.
//!
//! Runs next to a ridgewarden core and builds its view from the core's event feed. Admins
//! start and end games through their own core. Players sign every post with their own
//! key; check-ins are made on a site's own page from that node's LAN and countersigned by
//! that site's core.

pub mod archive;
pub mod chains;
pub mod config;
pub mod forms;
pub mod game;
pub mod history;
pub mod score;
pub mod web;
pub mod words;
pub mod world;

#[cfg(test)]
mod testing;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;
use ridgewarden_client::core::{Projection, Whoami};
use ridgewarden_client::mesh::Mesh;
use ridgewarden_client::util::now_ms;
use ridgewarden_client::{CoreClient, open_db};
use rusqlite::{Connection, OptionalExtension, params};

use crate::archive::Archive;
use crate::config::{Config, Net};
use crate::game::{DAY, Game, Session};
use crate::score::{Minutes, Played};
use crate::world::{Seen, take_dirty};

pub const PROJECTIONS: &[&dyn Projection] = &[&Mesh, &Game, &Seen];

/// Scores are worked out again at most this often; a check-in clears the cache.
const VIEW_TTL: Duration = Duration::from_secs(10);
/// Seasons, awards and stats are worked out again at most this often.
const ARCHIVE_TTL: Duration = Duration::from_secs(60);
/// Polls and link samples are kept as long as the core keeps them.
const RAW_KEEP: i64 = 30 * DAY;
/// An ended game's result is frozen this long after the end, once late events from
/// sites that were cut off have had a chance to arrive.
const SETTLE: i64 = 3_600_000;

pub(crate) const RESULTS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS results (
    session TEXT PRIMARY KEY,
    json    TEXT NOT NULL
);";

pub struct App {
    pub cfg: Config,
    pub lan: Vec<Net>,
    pub core: CoreClient,
    /// whether this app has the core's token, so it can have events signed
    pub signs: bool,
    pub db: Mutex<Connection>,
    cache: Mutex<HashMap<String, (Instant, Arc<Played>)>>,
    minutes: Mutex<HashMap<String, Minutes>>,
    archive: Mutex<Option<(Instant, Arc<Archive>)>>,
    me: Mutex<Option<Whoami>>,
    /// last form post per address, to slow down scripts
    pub posts: Mutex<HashMap<IpAddr, Instant>>,
    /// when the feed was last read to its end
    caught_up: AtomicI64,
}

impl App {
    pub fn new(cfg: Config, with_token: bool) -> Result<Self> {
        let token = with_token.then_some(cfg.core.token_file.as_path());
        let core = CoreClient::new(&cfg.core.url, token)?;
        let db = open_db(&cfg.db_path(), PROJECTIONS)?;
        db.execute_batch(RESULTS_SCHEMA)?;
        Ok(Self {
            lan: cfg.lan_nets()?,
            cfg,
            core,
            signs: with_token,
            db: Mutex::new(db),
            cache: Mutex::new(HashMap::new()),
            minutes: Mutex::new(HashMap::new()),
            archive: Mutex::new(None),
            me: Mutex::new(None),
            posts: Mutex::new(HashMap::new()),
            caught_up: AtomicI64::new(0),
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This site's core: its callsign, key and node ("" before it has polled its node).
    pub async fn me(&self) -> Option<Whoami> {
        if let Some(w) = self.me.lock().unwrap().clone() {
            return Some(w);
        }
        let w = self.core.whoami().await.ok()?;
        if !w.node.is_empty() {
            *self.me.lock().unwrap() = Some(w.clone());
        }
        Some(w)
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        game::sessions(&self.db(), &self.cfg.admins, now_ms())
    }

    /// A session's scores, worked out at most every `VIEW_TTL`.
    pub fn played(&self, s: &Session) -> Result<Arc<Played>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, p)) = cache.get(&s.id)
            && at.elapsed() < VIEW_TTL
            && p.session == *s
        {
            return Ok(p.clone());
        }
        let db = self.db();
        let mut minutes = self.minutes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ts) = take_dirty(&db)? {
            for m in minutes.values_mut() {
                m.touch(ts);
            }
        }
        let m = minutes.entry(s.id.clone()).or_default();
        let caught_up = self.caught_up.load(Ordering::Relaxed);
        let p = Arc::new(played(&db, s, now_ms(), caught_up, &self.cfg.admins, m)?);
        if caught_up > s.end + SETTLE {
            // frozen now, so its minutes won't be needed again
            minutes.remove(&s.id);
        }
        if cache.len() > 16 {
            cache.clear();
        }
        cache.insert(s.id.clone(), (Instant::now(), p.clone()));
        Ok(p)
    }

    pub fn forget_scores(&self) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Every game, season, award and statistic, worked out at most every `ARCHIVE_TTL`.
    pub fn archive(&self) -> Result<Arc<Archive>> {
        let mut cache = self.archive.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, a)) = cache.as_ref()
            && at.elapsed() < ARCHIVE_TTL
        {
            return Ok(a.clone());
        }
        let mut games = vec![];
        for s in self.sessions()? {
            games.push(self.played(&s)?);
        }
        let a = Arc::new(archive::build(&self.db(), &self.cfg.admins, games)?);
        *cache = Some((Instant::now(), a.clone()));
        Ok(a)
    }
}

/// Score a session, or read its frozen result. A result is frozen once the feed has been
/// read to the end at least `SETTLE` after the game ended (`caught_up` is when that last
/// happened), and is never rewritten: by then the polls it was scored from get pruned.
pub fn played(
    conn: &Connection,
    s: &Session,
    now: i64,
    caught_up: i64,
    admins: &[String],
    minutes: &mut Minutes,
) -> Result<Played> {
    let frozen: Option<String> = conn
        .query_row(
            "SELECT json FROM results WHERE session = ?1",
            [&s.id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(j) = &frozen {
        match serde_json::from_str::<Played>(j) {
            Ok(mut p) => {
                // where the foxes were may be said after the result was frozen
                p.session.foxes = s.foxes.clone();
                return Ok(p);
            }
            Err(e) => tracing::warn!(
                "the saved result of {} can't be read ({e}); scoring it again",
                s.name
            ),
        }
    }
    let p = score::play(conn, s, now, &game::keys(conn, admins)?, minutes)?;
    if frozen.is_none() && caught_up > s.end + SETTLE {
        conn.execute(
            "INSERT INTO results (session, json) VALUES (?1, ?2)",
            params![s.id, serde_json::to_string(&p)?],
        )?;
    }
    Ok(p)
}

fn prune(conn: &Connection, now: i64) -> Result<()> {
    let cut = now - RAW_KEEP;
    conn.execute("DELETE FROM link_samples WHERE ts < ?1", [cut])?;
    conn.execute("DELETE FROM polls WHERE ts < ?1", [cut])?;
    Ok(())
}

/// Follow the core's feed, reveal code words that are due, and prune old link data,
/// forever.
pub fn spawn_loops(app: Arc<App>) {
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            let started = now_ms();
            match ridgewarden_client::follow(&a.core, &a.db, PROJECTIONS).await {
                Ok(_) => a.caught_up.store(started, Ordering::Relaxed),
                Err(e) => tracing::warn!("following the core: {e:#}"),
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    if app.signs {
        let a = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                match words::reveal_due(&a).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("revealed {n} code words"),
                    Err(e) => tracing::warn!("revealing code words: {e:#}"),
                }
            }
        });
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            if let Err(e) = prune(&app.db(), now_ms()) {
                tracing::warn!("pruning old link data: {e:#}");
            }
        }
    });
}
