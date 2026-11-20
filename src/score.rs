//! Scoring a session. Everything here is worked out from the log and the mesh's own link
//! data, so every site that holds the same events shows the same leaderboard.
//!
//! Check-ins and code word claims are judged in time order. One that counts captures
//! the site for the player's team, unless the team already holds it, or finds the fox.
//! Every minute after the start, each team scores for its chains: two or more of its
//! sites joined by links that are up at that minute. A dropped link breaks the chain at
//! once, so its points stop until the link is back. Minutes already worked out are kept
//! (`Minutes`) and only redone when new data could change them.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ridgewarden_client::graph::Edge;
use ridgewarden_client::util::short_time;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::chains::{Chain, chains};
use crate::game::{self, Bearing, How, MIN, Play, Rules, Session, Spot, Target};
use crate::words::{self, Check};
use crate::world::{Pos, World, km};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Judged {
    pub id: String,
    pub ts: i64,
    pub call: String,
    pub team: String,
    pub target: Target,
    /// made with a code word rather than on the site's LAN
    pub by_word: bool,
    pub counted: bool,
    /// a code word claim whose word isn't revealed yet
    pub pending: bool,
    pub points: i64,
    /// what happened, in words
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct TeamScore {
    pub team: String,
    pub total: i64,
    pub chain: i64,
    pub builder: i64,
    pub capture: i64,
    pub encounter: i64,
    pub fox: i64,
    /// points per minute from chains right now
    pub rate: i64,
    pub held: usize,
    /// the most sites it had in one chain during the game
    pub best_chain: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PlayerScore {
    pub call: String,
    /// empty for a fox operator who isn't on a team
    pub team: String,
    pub points: i64,
    pub captures: u32,
    pub encounters: u32,
    pub finds: u32,
    pub strikes: i64,
    /// sites where a check-in or claim of theirs counted
    pub sites: BTreeSet<String>,
}

/// A link that went down during the game.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Drop {
    pub link: Edge,
    pub from: i64,
    /// when it came back, if it did before the end
    pub to: Option<i64>,
    /// teams whose chains ran through it
    pub broke: Vec<String>,
}

/// A scored session, as of `at`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Played {
    pub session: Session,
    pub at: i64,
    /// highest total first
    pub teams: Vec<TeamScore>,
    pub players: Vec<PlayerScore>,
    /// oldest first
    pub checkins: Vec<Judged>,
    pub holders: BTreeMap<String, String>,
    pub chains: Vec<Chain>,
    pub up: BTreeSet<Edge>,
    pub known: BTreeSet<Edge>,
    pub positions: BTreeMap<String, Pos>,
    pub drops: Vec<Drop>,
    pub bearings: Vec<Bearing>,
    pub spots: Vec<Spot>,
    /// LAN check-in sites no core polls over its LAN, so nobody can check in there
    pub dead_sites: Vec<String>,
    /// the link data from the start of the session has been pruned
    pub links_pruned: bool,
}

enum Verdict {
    Count,
    No(String),
    Wait(String),
}

#[derive(Default)]
struct Judge {
    holder: BTreeMap<String, String>,
    captures: BTreeMap<(String, String), u32>,
    team_of: BTreeMap<String, String>,
    last: BTreeMap<String, (i64, String)>,
    strikes: BTreeMap<String, i64>,
    counted: Vec<(i64, String, String, String)>,
    /// (site, callsign) of everyone who has had encounter points there
    met: BTreeSet<(String, String)>,
    /// (callsign, target, slot) of every code word tried
    tried: BTreeSet<(String, String, i64)>,
    found: BTreeSet<(String, String)>,
    players: BTreeMap<String, PlayerScore>,
    teams: BTreeMap<String, TeamScore>,
    /// (when, site, team) for each capture
    changes: Vec<(i64, String, String)>,
}

fn ordinal(n: u32) -> String {
    match n {
        2 => "2nd".into(),
        3 => "3rd".into(),
        n => format!("{n}th"),
    }
}

type Reveals = BTreeMap<(String, i64), (String, i64)>;

/// Whether a play is aimed at something in the game, proven the way that thing needs.
fn place(s: &Session, w: &World, p: &Play) -> Option<Verdict> {
    let valid = match (&p.target, &p.how) {
        (Target::Site(n), How::Lan { .. }) => s.sites.contains(n),
        (Target::Site(n), How::Word { .. }) => s.word_sites.contains(n),
        (Target::Fox(f), How::Word { .. }) => s.fox(f).is_some(),
        (Target::Fox(_), How::Lan { .. }) => false,
    };
    if !valid {
        return Some(Verdict::No(match p.how {
            How::Lan { .. } => "not a check-in site in this game".into(),
            How::Word { .. } => "nothing in this game sends that code word".into(),
        }));
    }
    if let (Target::Site(n), How::Lan { key }) = (&p.target, &p.how)
        && !w.lan_keys.get(n).is_some_and(|k| k.contains(key))
    {
        return Some(Verdict::No(
            "not countersigned by the site's own core".into(),
        ));
    }
    None
}

impl Judge {
    fn new(s: &Session) -> Self {
        let mut j = Judge::default();
        for t in &s.teams {
            j.teams.insert(
                t.clone(),
                TeamScore {
                    team: t.clone(),
                    ..Default::default()
                },
            );
        }
        j
    }

    /// For a code word claim: one try per word, and the word must match its slot's.
    fn proof(&mut self, s: &Session, rv: &Reveals, p: &Play) -> Option<Verdict> {
        let How::Word { word } = &p.how else {
            return None;
        };
        let slot = words::slot_of(s, p.ts);
        if !self.tried.insert((p.call.clone(), p.target.key(), slot)) {
            return Some(Verdict::No("one try per code word".into()));
        }
        match words::check(s, rv, &p.target, p.ts, word) {
            Check::Right => None,
            Check::Wrong => Some(Verdict::No("wrong code word".into())),
            Check::Late => Some(Verdict::No(
                "typed in after the word had been revealed".into(),
            )),
            Check::Waiting(t) => Some(Verdict::Wait(format!(
                "waiting for the word to be revealed after {}",
                &short_time(t)[11..]
            ))),
        }
    }

    fn travel(&mut self, s: &Session, w: &World, p: &Play) -> Option<Verdict> {
        let Target::Site(site) = &p.target else {
            return None;
        };
        let (pt, ps) = self.last.get(&p.call).cloned()?;
        let (Some(a), Some(b)) = (w.positions.get(&ps), w.positions.get(site)) else {
            return None;
        };
        let dist = km(*a, *b);
        let need = (dist / s.rules.max_speed_kmh * 3_600_000.0) as i64;
        if ps == *site || p.ts - pt >= need {
            return None;
        }
        let limit = s.rules.strikes;
        let n = self.strikes.get(&p.call).copied().unwrap_or(0) + 1;
        self.strikes.insert(p.call.clone(), n);
        let ladder = if n == 1 {
            "first warning".to_string()
        } else if n < limit {
            format!("strike {n} of {limit}")
        } else {
            format!("strike {n} of {limit}, out of this game")
        };
        Some(Verdict::No(format!(
            "too fast: {dist:.1} km from {ps} in {} min is over {:.0} km/h ({ladder})",
            (p.ts - pt) / MIN,
            s.rules.max_speed_kmh
        )))
    }

    fn judge(&mut self, s: &Session, w: &World, rv: &Reveals, p: &Play) -> Verdict {
        if p.ts < s.start || p.ts > s.end {
            return Verdict::No("outside the game's time".into());
        }
        if let Some(v) = place(s, w, p) {
            return v;
        }
        if !s.teams.contains(&p.team) {
            return Verdict::No(format!("there is no team called {}", p.team));
        }
        if !s.roster.is_empty() && !s.roster.contains_key(&p.call) {
            return Verdict::No(format!("{} isn't on this game's roster", p.call));
        }
        if let Some(t) = s.roster.get(&p.call).or(self.team_of.get(&p.call))
            && *t != p.team
        {
            return Verdict::No(format!("{} plays for {t}", p.call));
        }
        let limit = s.rules.strikes;
        if self.strikes.get(&p.call).copied().unwrap_or(0) >= limit {
            return Verdict::No(format!("out of this game after {limit} impossible hops"));
        }
        self.proof(s, rv, p)
            .or_else(|| self.travel(s, w, p))
            .unwrap_or(Verdict::Count)
    }

    fn player(&mut self, call: &str, team: &str) -> &mut PlayerScore {
        self.players
            .entry(call.to_string())
            .or_insert_with(|| PlayerScore {
                call: call.to_string(),
                team: team.to_string(),
                ..Default::default()
            })
    }

    fn credit(&mut self, call: &str, team: &str, points: i64) {
        self.player(call, team).points += points;
        if !team.is_empty() {
            self.teams.entry(team.to_string()).or_default().total += points;
        }
    }

    fn count(&mut self, s: &Session, p: &Play) -> (i64, String) {
        self.team_of.entry(p.call.clone()).or_insert(p.team.clone());
        self.player(&p.call, &p.team);
        match &p.target {
            Target::Site(site) => {
                self.last.insert(p.call.clone(), (p.ts, site.clone()));
                self.player(&p.call, &p.team).sites.insert(site.clone());
                self.capture(s, p, site)
            }
            Target::Fox(fox) => self.find(s, p, fox),
        }
    }

    fn capture(&mut self, s: &Session, p: &Play, site: &str) -> (i64, String) {
        let mut points = 0;
        let mut notes = vec![];
        if self.holder.get(site) == Some(&p.team) {
            notes.push(format!("{} already holds it", p.team));
        } else {
            let n = self
                .captures
                .entry((p.team.clone(), site.to_string()))
                .or_default();
            *n += 1;
            let n = *n;
            let worth = s.rules.capture_worth(n);
            points += worth;
            self.teams.entry(p.team.clone()).or_default().capture += worth;
            self.player(&p.call, &p.team).captures += 1;
            self.credit(&p.call, &p.team, worth);
            self.holder.insert(site.to_string(), p.team.clone());
            self.changes.push((p.ts, site.to_string(), p.team.clone()));
            notes.push(if n == 1 {
                format!("captured for {}", p.team)
            } else {
                format!("captured for {} ({} time)", p.team, ordinal(n))
            });
        }
        let (met, names) = self.meet(s, p, site);
        points += met;
        if !names.is_empty() {
            notes.push(format!("met {}", names.join(", ")));
        }
        self.counted
            .push((p.ts, site.to_string(), p.call.clone(), p.team.clone()));
        (points, notes.join("; "))
    }

    /// Encounter points for a play at `site`: the players of other teams counted there
    /// within the window. Each callsign earns them at most once per site. Returns what this
    /// play's player got.
    fn meet(&mut self, s: &Session, p: &Play, site: &str) -> (i64, Vec<String>) {
        let window = s.rules.encounter_min * MIN;
        let others: Vec<(String, String)> = self
            .counted
            .iter()
            .rev()
            .take_while(|(ts, ..)| p.ts - ts <= window)
            .filter(|(_, at, call, team)| at == site && *team != p.team && *call != p.call)
            .map(|(_, _, call, team)| (call.clone(), team.clone()))
            .collect();
        let pts = s.rules.encounter_points;
        let mut got = 0;
        let mut names = BTreeSet::new();
        for (other, other_team) in others {
            names.insert(other.clone());
            for (call, team) in [(&p.call, &p.team), (&other, &other_team)] {
                if !self.met.insert((site.to_string(), call.clone())) {
                    continue;
                }
                self.player(call, team).encounters += 1;
                self.teams.entry(team.clone()).or_default().encounter += pts;
                self.credit(call, team, pts);
                if *call == p.call {
                    got += pts;
                }
            }
        }
        (got, names.into_iter().collect())
    }

    /// A fox found: points to the finder, and to whoever runs the fox, once per finder.
    fn find(&mut self, s: &Session, p: &Play, fox: &str) -> (i64, String) {
        if !self.found.insert((p.call.clone(), fox.to_string())) {
            return (0, format!("already found {fox}"));
        }
        let pts = s.rules.fox_points;
        self.player(&p.call, &p.team).finds += 1;
        self.teams.entry(p.team.clone()).or_default().fox += pts;
        self.credit(&p.call, &p.team, pts);
        let mut note = format!("found {fox}");
        if let Some(op) = s.fox(fox).and_then(|f| f.call.clone())
            && op != p.call
        {
            let team = s
                .roster
                .get(&op)
                .or(self.team_of.get(&op))
                .cloned()
                .unwrap_or_default();
            let ep = s.rules.encounter_points;
            if !team.is_empty() {
                self.teams.entry(team.clone()).or_default().fox += ep;
            }
            self.credit(&op, &team, ep);
            note += &format!("; {ep} to {op}, who runs it");
        }
        (pts, note)
    }

    /// Judge every play up to `at`, in order.
    fn judge_all(
        &mut self,
        s: &Session,
        w: &World,
        rv: &Reveals,
        plays: &[Play],
        at: i64,
    ) -> Vec<Judged> {
        let mut out = vec![];
        for p in plays.iter().filter(|p| p.ts <= at) {
            let (counted, pending, points, note) = match self.judge(s, w, rv, p) {
                Verdict::No(why) => (false, false, 0, why),
                Verdict::Wait(why) => (false, true, 0, why),
                Verdict::Count => {
                    let (pts, note) = self.count(s, p);
                    (true, false, pts, note)
                }
            };
            out.push(Judged {
                id: p.id.clone(),
                ts: p.ts,
                call: p.call.clone(),
                team: p.team.clone(),
                target: p.target.clone(),
                by_word: matches!(p.how, How::Word { .. }),
                counted,
                pending,
                points,
                note,
            });
        }
        out
    }
}

/// Every team's chains at time `t`, with their new links marked.
fn all_chains(
    s: &Session,
    w: &World,
    holders: &BTreeMap<String, String>,
    up: &BTreeSet<Edge>,
    t: i64,
) -> Vec<Chain> {
    let sites = s.all_sites();
    let mut out = vec![];
    for team in &s.teams {
        for mut c in chains(team, holders, &sites, up) {
            c.new_links = c
                .links
                .iter()
                .filter(|e| w.is_new_link(e, t, &s.rules))
                .cloned()
                .collect();
            out.push(c);
        }
    }
    out
}

fn chain_rate(r: &Rules, c: &Chain) -> (i64, i64) {
    (
        r.chain_points * c.sites.len() as i64,
        r.builder_points * c.new_links.len() as i64,
    )
}

/// One scored minute.
#[derive(Clone, Debug, PartialEq)]
struct Minute {
    /// (chain, builder) points per team, in the session's team order
    points: Vec<(i64, i64)>,
    /// biggest chain per team
    best: Vec<usize>,
    up: BTreeSet<u32>,
    /// (team index, link) for every link in a chain
    chained: BTreeSet<(usize, u32)>,
}

/// What a session's minutes depend on besides the data: change any of it and every
/// minute is redone.
type Shape = (i64, i64, Vec<String>, Vec<String>, Rules);

/// The minutes of one session worked out so far. Kept between page views; new polls
/// from before the last minute, or a change in who held what when, drop the minutes from
/// then on, and only those are worked out again.
#[derive(Default)]
pub struct Minutes {
    shape: Option<Shape>,
    changes: Vec<(i64, String, String)>,
    edges: Vec<Edge>,
    ids: BTreeMap<Edge, u32>,
    /// minute i is at start + (i + 1) minutes
    done: Vec<Minute>,
    /// the oldest new poll since the last run
    dirty: Option<i64>,
}

impl Minutes {
    /// Note that polls from `ts` on arrived since the last run.
    pub fn touch(&mut self, ts: i64) {
        self.dirty = Some(self.dirty.map_or(ts, |d| d.min(ts)));
    }

    /// Drop what can no longer be trusted.
    fn invalidate(&mut self, s: &Session, changes: &[(i64, String, String)]) {
        let shape: Shape = (
            s.start,
            s.end,
            s.teams.clone(),
            s.all_sites(),
            s.rules.clone(),
        );
        if self.shape.as_ref() != Some(&shape) {
            *self = Minutes {
                shape: Some(shape),
                ..Default::default()
            };
        }
        let differ = self
            .changes
            .iter()
            .zip(changes)
            .position(|(a, b)| a != b)
            .unwrap_or(self.changes.len().min(changes.len()));
        let changed = [self.changes.get(differ), changes.get(differ)]
            .into_iter()
            .flatten()
            .map(|c| c.0)
            .min();
        if let Some(cut) = [changed, self.dirty.take()].into_iter().flatten().min() {
            // minute i at start + (i + 1) min sees everything up to it, so keep those before
            let keep = cut
                .saturating_sub(s.start + 1)
                .div_euclid(MIN)
                .clamp(0, self.done.len() as i64);
            self.done.truncate(keep as usize);
        }
        self.changes = changes.to_vec();
    }

    fn next(&self, s: &Session) -> i64 {
        s.start + (self.done.len() as i64 + 1) * MIN
    }

    fn id(&mut self, e: &Edge) -> u32 {
        if let Some(i) = self.ids.get(e) {
            return *i;
        }
        let i = self.edges.len() as u32;
        self.edges.push(e.clone());
        self.ids.insert(e.clone(), i);
        i
    }

    /// Work out every minute from the first missing one up to `at`.
    fn run(&mut self, s: &Session, w: &World, at: i64) {
        let mut holders = BTreeMap::new();
        let mut changes = self.changes.clone().into_iter().peekable();
        let mut t = self.next(s);
        while t <= at {
            while let Some((_, site, team)) = changes.next_if(|(ts, ..)| *ts <= t) {
                holders.insert(site, team);
            }
            let up = w.up_at(t, &s.rules);
            let mut m = Minute {
                points: vec![(0, 0); s.teams.len()],
                best: vec![0; s.teams.len()],
                up: up.iter().map(|e| self.id(e)).collect(),
                chained: BTreeSet::new(),
            };
            for c in all_chains(s, w, &holders, &up, t) {
                let Some(ti) = s.teams.iter().position(|x| *x == c.team) else {
                    continue;
                };
                let (chain, builder) = chain_rate(&s.rules, &c);
                m.points[ti].0 += chain;
                m.points[ti].1 += builder;
                m.best[ti] = m.best[ti].max(c.sites.len());
                for e in &c.links {
                    let id = self.id(e);
                    m.chained.insert((ti, id));
                }
            }
            self.done.push(m);
            t += MIN;
        }
    }

    /// Links that went down during the game, from the minutes worked out.
    fn drops(&self, s: &Session) -> Vec<Drop> {
        let mut out: Vec<Drop> = vec![];
        let mut open: BTreeMap<u32, usize> = BTreeMap::new();
        for (i, m) in self.done.iter().enumerate() {
            let t = s.start + (i as i64 + 1) * MIN;
            if let Some(prev) = i.checked_sub(1).map(|p| &self.done[p]) {
                for e in prev.up.difference(&m.up) {
                    let broke = prev
                        .chained
                        .iter()
                        .filter(|(_, x)| x == e)
                        .map(|(ti, _)| s.teams[*ti].clone())
                        .collect();
                    open.insert(*e, out.len());
                    out.push(Drop {
                        link: self.edges[*e as usize].clone(),
                        from: t,
                        to: None,
                        broke,
                    });
                }
            }
            for e in &m.up {
                if let Some(d) = open.remove(e) {
                    out[d].to = Some(t);
                }
            }
        }
        out.sort_by(|a, b| (a.from, &a.link).cmp(&(b.from, &b.link)));
        out
    }

    /// Every link that was up in some scored minute.
    fn ever_up(&self) -> BTreeSet<Edge> {
        self.done
            .iter()
            .flat_map(|m| &m.up)
            .map(|e| self.edges[*e as usize].clone())
            .collect()
    }
}

/// Polls are loaded this many minutes at a time, so a long game doesn't hold them all.
const CHUNK: i64 = 6 * 60;

/// Score session `s` as of `now`, reusing and extending `minutes`.
pub fn play(conn: &Connection, s: &Session, now: i64, minutes: &mut Minutes) -> Result<Played> {
    let at = s.end.min(now);
    let mut w = World::places(conn)?;
    let reveals = game::reveals(conn, s)?;
    let mut j = Judge::new(s);
    let checkins = j.judge_all(s, &w, &reveals, &game::plays(conn, &s.id)?, at);
    minutes.invalidate(s, &j.changes);
    // the map wants the last hour of links, and each minute the polls before it
    let fresh = 2 * s.rules.fresh_s * 1000;
    while minutes.next(s) <= at {
        let from = minutes.next(s);
        let to = (from + CHUNK * MIN).min(at);
        w.load_polls(conn, from - fresh, to)?;
        minutes.run(s, &w, to);
    }
    // the map shows the last hour of links as well as every link up during the game
    let recent = (at - 60 * MIN).max(s.start);
    w.load_polls(conn, recent - fresh, at)?;
    let mut known = w.known_since(recent, &s.rules);
    known.extend(minutes.ever_up());

    for (ti, team) in s.teams.iter().enumerate() {
        let ts = j.teams.entry(team.clone()).or_default();
        for m in &minutes.done {
            let (chain, builder) = m.points[ti];
            ts.chain += chain;
            ts.builder += builder;
            ts.total += chain + builder;
            ts.best_chain = ts.best_chain.max(m.best[ti]);
        }
    }
    let up = w.up_at(at, &s.rules);
    let chains = all_chains(s, &w, &j.holder, &up, at);
    for c in &chains {
        let (chain, builder) = chain_rate(&s.rules, c);
        j.teams.entry(c.team.clone()).or_default().rate += chain + builder;
    }
    for team in j.holder.values() {
        j.teams.entry(team.clone()).or_default().held += 1;
    }
    for (call, n) in &j.strikes {
        if let Some(p) = j.players.get_mut(call) {
            p.strikes = *n;
        }
    }
    let mut teams: Vec<TeamScore> = j.teams.into_values().collect();
    teams.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.team.cmp(&b.team)));
    let mut players: Vec<PlayerScore> = j.players.into_values().collect();
    players.sort_by(|a, b| b.points.cmp(&a.points).then_with(|| a.call.cmp(&b.call)));
    Ok(Played {
        dead_sites: s
            .sites
            .iter()
            .filter(|n| !w.has_lan_core(n))
            .cloned()
            .collect(),
        links_pruned: w.oldest_poll.is_none_or(|o| o > s.start),
        known,
        drops: minutes.drops(s),
        bearings: game::bearings(conn, s)?,
        spots: game::spots(conn, &s.id)?,
        positions: w.positions,
        session: s.clone(),
        at,
        teams,
        players,
        checkins,
        holders: j.holder,
        chains,
        up,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::DAY;
    use crate::testing::{G, T0, player, team};
    use serde_json::json;

    #[test]
    fn chain_scores_while_its_link_is_up_and_stops_when_it_drops() {
        let mut g = G::new();
        g.polls(T0 - 10 * MIN, T0 + 30 * MIN, "a", &["b"]);
        g.polls(T0 - 10 * MIN, T0 + 30 * MIN, "b", &["a"]);
        g.polls(T0 + 31 * MIN, T0 + 60 * MIN, "a", &[]);
        g.polls(T0 + 31 * MIN, T0 + 60 * MIN, "b", &[]);
        let s = g.start(T0, json!({}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + 2 * MIN, &s, "N0BBB", "Red", "b");

        let p = g.played(T0 + 20 * MIN);
        let red = team(&p, "Red");
        // both sites held from the minute at +2 on: 19 minutes, 2 sites
        assert_eq!(red.chain, 19 * 2);
        assert_eq!(red.capture, 20);
        assert_eq!(red.rate, 2);
        assert_eq!(p.chains.len(), 1);

        let p = g.played(T0 + 60 * MIN);
        let red = team(&p, "Red");
        // the link is gone after two polls without it (+31 and +32), so +2 to +31 scored
        assert_eq!(red.chain, 30 * 2);
        assert_eq!(red.total, 30 * 2 + 20);
        assert_eq!(red.rate, 0, "no chain once the link is down");
        assert!(p.chains.is_empty());
        assert_eq!(team(&p, "Blue").total, 0);
    }

    #[test]
    fn a_link_with_no_fresh_data_does_not_score() {
        let mut g = G::new();
        g.polls(T0 - 10 * MIN, T0 + 5 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + MIN, &s, "N0BBB", "Red", "b");
        // nothing is polled after +5; the default fresh_s is 10 minutes
        let p = g.played(T0 + 30 * MIN);
        assert_eq!(team(&p, "Red").chain, 15 * 2);
    }

    #[test]
    fn repeat_captures_are_worth_less_and_holding_check_ins_nothing() {
        let mut g = G::new();
        let s = g.start(T0, json!({"rules": {"encounter_points": 0}}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + 3 * MIN, &s, "N0BBB", "Blue", "a");
        g.checkin(T0 + 5 * MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + 7 * MIN, &s, "N0BBB", "Blue", "a");
        g.checkin(T0 + 9 * MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + 10 * MIN, &s, "N0CCC", "Red", "a");
        let p = g.played(T0 + 11 * MIN);
        let pts: Vec<i64> = p.checkins.iter().map(|c| c.points).collect();
        assert_eq!(pts, [10, 10, 5, 5, 2, 0]);
        assert!(p.checkins[5].note.contains("already holds"));
        assert!(p.checkins[4].note.contains("3rd time"));
        assert_eq!(team(&p, "Red").capture, 17);
        assert_eq!(p.holders["a"], "Red");
    }

    #[test]
    fn rules_come_from_the_start_event() {
        let mut g = G::new();
        let s = g.start(T0, json!({"rules": {"capture_points": 3}}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        assert_eq!(g.played(T0 + 2 * MIN).checkins[0].points, 3);
        // rules a reader can't accept make the start event invalid, not silently different
        let mut g = G::new();
        g.start(T0, json!({"rules": {"capture_points": -5}}));
        assert!(g.sessions(T0 + MIN).is_empty());
    }

    #[test]
    fn an_encounter_credits_both_sides_once() {
        let mut g = G::new();
        let s = g.start(T0, json!({}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + 4 * MIN, &s, "N0BBB", "Blue", "a");
        g.checkin(T0 + 6 * MIN, &s, "N0AAA", "Red", "a");
        // N0AAA has had its encounter at a; N0DDD hasn't
        g.checkin(T0 + 7 * MIN, &s, "N0DDD", "Blue", "a");
        // too late to meet anyone
        g.checkin(T0 + 30 * MIN, &s, "N0CCC", "Blue", "a");
        let p = g.played(T0 + 31 * MIN);
        assert_eq!(
            player(&p, "N0DDD").points,
            5 + 5,
            "a 2nd capture and an encounter"
        );
        assert_eq!(player(&p, "N0AAA").points, 10 + 5 + 5);
        assert_eq!(player(&p, "N0AAA").encounters, 1);
        assert_eq!(player(&p, "N0BBB").points, 10 + 5);
        assert_eq!(player(&p, "N0CCC").encounters, 0);
        assert_eq!(team(&p, "Red").encounter, 5);
        assert_eq!(team(&p, "Blue").encounter, 10);
    }

    #[test]
    fn impossible_hops_warn_then_strike_out() {
        let mut g = G::new();
        g.poll(T0 - MIN, "a", &[]);
        g.poll(T0 - MIN, "c", &[]);
        let s = g.start(T0, json!({}));
        // a to c is 55 km: 33 minutes at 100 km/h
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + 5 * MIN, &s, "N0AAA", "Red", "c");
        g.checkin(T0 + 6 * MIN, &s, "N0AAA", "Red", "c");
        g.checkin(T0 + 7 * MIN, &s, "N0AAA", "Red", "c");
        g.checkin(T0 + 90 * MIN, &s, "N0AAA", "Red", "c");
        g.checkin(T0 + MIN, &s, "N0BBB", "Blue", "a");
        g.checkin(T0 + 40 * MIN, &s, "N0BBB", "Blue", "c");
        let p = g.played(T0 + 100 * MIN);
        let aaa: Vec<&Judged> = p.checkins.iter().filter(|c| c.call == "N0AAA").collect();
        assert!(aaa[0].counted);
        assert!(
            !aaa[1].counted && aaa[1].note.contains("first warning"),
            "{}",
            aaa[1].note
        );
        assert!(aaa[2].note.contains("strike 2 of 3"));
        assert!(aaa[3].note.contains("out of this game"));
        assert!(
            !aaa[4].counted,
            "out for the rest of the game, however slowly they travel"
        );
        assert_eq!(player(&p, "N0AAA").strikes, 3);
        let bbb: Vec<&Judged> = p.checkins.iter().filter(|c| c.call == "N0BBB").collect();
        assert!(bbb[1].counted, "{}", bbb[1].note);
        assert_eq!(p.holders["c"], "Blue");
    }

    #[test]
    fn check_ins_count_only_when_signed_by_the_sites_own_core() {
        let other = "ab".repeat(32);
        let mut g = G::new();
        g.poll(T0 - MIN, "a", &[]);
        let s = g.start(T0, json!({}));
        let payload = json!({"session": s, "call": "N0AAA", "team": "Red", "site": "a"});
        g.add_as(&other, 1000, "game.checkin", payload, T0 + MIN);
        let p = g.played(T0 + 2 * MIN);
        assert!(!p.checkins[0].counted);
        assert!(p.checkins[0].note.contains("countersigned"));

        // the core that polls a over its LAN is a's core
        let mut g = G::new();
        let poll = json!({"node": "a", "ok": true, "via": "lan"});
        g.add_as(&other, 999, "node.poll", poll, T0);
        let s = g.start(T0, json!({}));
        let payload = json!({"session": s, "call": "N0AAA", "team": "Red", "site": "a"});
        g.add_as(&other, 1000, "game.checkin", payload, T0 + MIN);
        assert!(g.played(T0 + 2 * MIN).checkins[0].counted);

        // a core that only crawls a node isn't that node's core
        let mut g = G::new();
        let poll = json!({"node": "a", "ok": true, "via": "crawl"});
        g.add_as(&other, 999, "node.poll", poll, T0);
        let s = g.start(T0, json!({}));
        let payload = json!({"session": s, "call": "N0AAA", "team": "Red", "site": "a"});
        g.add_as(&other, 1000, "game.checkin", payload, T0 + MIN);
        assert!(!g.played(T0 + 2 * MIN).checkins[0].counted);
    }

    #[test]
    fn only_the_station_that_started_the_game_reveals_its_words() {
        let mut g = G::new();
        let s = g.start(T0, json!({"sites": [], "word_sites": ["c"]}));
        let c = json!({"site": "c"});
        g.word(T0 + 5 * MIN, &s, "N0AAA", "Red", c.clone(), "FAKE WORD 00");
        // another admin, not the starter, "reveals" a word early
        let other = "ab".repeat(32);
        let reveal = json!({"session": s, "site": "c", "slot": 0, "word": "FAKE WORD 00"});
        g.add_as(&other, 1000, "game.reveal", reveal, T0 + 6 * MIN);
        let admins = vec![g.key.clone(), other];
        let session = game::sessions(&g.f.conn, &admins, T0 + 7 * MIN)
            .unwrap()
            .remove(0);
        let p = crate::played(
            &g.f.conn,
            &session,
            T0 + 7 * MIN,
            0,
            &mut Minutes::default(),
        );
        assert!(p.unwrap().checkins[0].pending);
    }

    #[test]
    fn players_keep_their_team_and_a_roster_closes_the_game() {
        let notes = |p: &Played| -> Vec<(String, bool)> {
            p.checkins
                .iter()
                .map(|c| (c.note.clone(), c.counted))
                .collect()
        };
        let mut g = G::new();
        let s = g.start(T0, json!({}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + 2 * MIN, &s, "N0AAA", "Blue", "b");
        g.checkin(T0 + 4 * MIN, &s, "N0QQQ", "Green", "c");
        let n = notes(&g.played(T0 + 5 * MIN));
        assert!(n[0].1);
        assert_eq!(n[1], ("N0AAA plays for Red".to_string(), false));
        assert_eq!(n[2], ("there is no team called Green".to_string(), false));

        // once an admin puts anyone on the roster, only rostered callsigns play
        let mut g = G::new();
        let s = g.start(T0, json!({}));
        let roster = json!({"N0ZZZ": "Blue", "N0AAA": "Red"});
        let teams = json!({"session": s, "teams": ["Red", "Blue"], "roster": roster});
        g.f.add("game.teams", teams, T0);
        g.checkin(T0 + MIN, &s, "N0ZZZ", "Red", "c");
        g.checkin(T0 + 2 * MIN, &s, "N0MADEUP1", "Red", "c");
        g.checkin(T0 + 3 * MIN, &s, "N0AAA", "Red", "a");
        let n = notes(&g.played(T0 + 5 * MIN));
        assert_eq!(n[0], ("N0ZZZ plays for Blue".to_string(), false));
        assert_eq!(
            n[1],
            ("N0MADEUP1 isn't on this game's roster".to_string(), false)
        );
        assert!(n[2].1);

        // only admins set teams
        let other = "ef".repeat(32);
        let teams = json!({"session": s, "teams": ["Red", "Blue"], "roster": {"N0AAA": "Blue"}});
        g.add_as(&other, 1000, "game.teams", teams, T0 + 6 * MIN);
        assert_eq!(g.sessions(T0 + 7 * MIN)[0].roster["N0AAA"], "Red");
    }

    #[test]
    fn games_only_start_from_admins_and_end_stops_scoring() {
        let mut g = G::new();
        let other = "cd".repeat(32);
        let start = json!({"name": "Not ours", "teams": ["Red", "Blue"], "sites": ["a"]});
        g.add_as(&other, 1000, "game.start", start, T0 - DAY);
        assert!(g.sessions(T0).is_empty());

        g.polls(T0 - 10 * MIN, T0 + 30 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({"minutes": 60}));
        assert_eq!(g.sessions(T0 + MIN)[0].end, T0 + 60 * MIN);
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + MIN, &s, "N0BBB", "Red", "b");
        // someone else can't end it; an admin can, earlier than planned
        g.add_as(
            &other,
            1001,
            "game.end",
            json!({"session": s}),
            T0 + 5 * MIN,
        );
        g.f.add("game.end", json!({"session": s}), T0 + 10 * MIN);
        g.checkin(T0 + 12 * MIN, &s, "N0CCC", "Blue", "c");
        let p = g.played(T0 + 30 * MIN);
        assert!(p.session.ended);
        assert_eq!(p.session.end, T0 + 10 * MIN);
        assert_eq!(team(&p, "Red").chain, 10 * 2, "minutes +1 to +10");
        assert!(
            p.checkins.iter().all(|c| c.call != "N0CCC"),
            "after the end"
        );
    }

    #[test]
    fn builder_bonus_needs_a_new_link_seen_on_enough_days() {
        let mut g = G::new();
        // day 0: a and b are both up but don't hear each other
        g.poll(T0 - 3 * 86_400_000, "a", &[]);
        g.poll(T0 - 3 * 86_400_000, "b", &[]);
        // the new link shows up on days 1 and 2, and on game day
        g.poll(T0 - 2 * 86_400_000, "a", &["b"]);
        g.poll(T0 - 86_400_000, "b", &["a"]);
        g.polls(T0 - 10 * MIN, T0 + 10 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({}));
        g.checkin(T0, &s, "N0AAA", "Red", "a");
        g.checkin(T0, &s, "N0BBB", "Red", "b");
        let p = g.played(T0 + 10 * MIN);
        assert_eq!(team(&p, "Red").builder, 10 * 2, "3 days by game day");
        assert_eq!(p.chains[0].new_links.len(), 1);

        let mut g = G::new();
        g.poll(T0 - 3 * 86_400_000, "a", &[]);
        g.poll(T0 - 86_400_000, "a", &["b"]);
        g.polls(T0 - 10 * MIN, T0 + 10 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({}));
        g.checkin(T0, &s, "N0AAA", "Red", "a");
        g.checkin(T0, &s, "N0BBB", "Red", "b");
        let p = g.played(T0 + 10 * MIN);
        assert_eq!(team(&p, "Red").builder, 0, "only 2 days");

        let mut g = G::new();
        g.poll(T0 - 3 * 86_400_000, "a", &["b"]);
        g.poll(T0 - 2 * 86_400_000, "a", &["b"]);
        g.polls(T0 - 10 * MIN, T0 + 10 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({}));
        g.checkin(T0, &s, "N0AAA", "Red", "a");
        g.checkin(T0, &s, "N0BBB", "Red", "b");
        let p = g.played(T0 + 10 * MIN);
        assert_eq!(
            team(&p, "Red").builder,
            0,
            "there since the first poll, so not new"
        );
        assert_eq!(team(&p, "Red").chain, 10 * 2);
    }

    #[test]
    fn a_settled_result_survives_pruned_link_data() {
        let mut g = G::new();
        g.polls(T0 - 10 * MIN, T0 + 30 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({"minutes": 20}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + MIN, &s, "N0BBB", "Red", "b");
        let session = g.sessions(T0 + 3 * 60 * MIN).remove(0);
        // not frozen while the feed hasn't been read to the end since the game settled
        let mut m = Minutes::default();
        let early = crate::played(
            &g.f.conn,
            &session,
            T0 + 3 * 60 * MIN,
            T0 + 30 * MIN,
            &mut m,
        )
        .unwrap();
        assert_eq!(team(&early, "Red").chain, 20 * 2);
        let n: i64 =
            g.f.conn
                .query_row("SELECT count(*) FROM results", [], |r| r.get(0))
                .unwrap();
        assert_eq!(n, 0);

        let before = g.played(T0 + 3 * 60 * MIN);
        g.f.conn.execute("DELETE FROM polls", []).unwrap();
        assert_eq!(g.played(T0 + 4 * 60 * MIN), before);
    }

    #[test]
    fn a_public_site_is_taken_with_the_code_word_once_it_is_revealed() {
        let mut g = G::new();
        let s = g.start(T0, json!({"sites": [], "word_sites": ["c"]}));
        let c = json!({"site": "c"});
        g.word(
            T0 + 5 * MIN,
            &s,
            "N0AAA",
            "Red",
            c.clone(),
            "otter kite 0 1",
        );
        g.word(T0 + 6 * MIN, &s, "N0CCC", "Red", c.clone(), "OTTER KITE 02");
        let p = g.played(T0 + 10 * MIN);
        assert!(p.checkins[0].pending && !p.checkins[0].counted);
        assert!(p.holders.is_empty(), "nothing is taken before the reveal");

        g.reveal(T0 + 17 * MIN, &s, c.clone(), 0, "OTTER KITE 01");
        g.word(
            T0 + 20 * MIN,
            &s,
            "N0BBB",
            "Blue",
            c.clone(),
            "CEDAR MESA 77",
        );
        g.word(
            T0 + 21 * MIN,
            &s,
            "N0BBB",
            "Blue",
            c.clone(),
            "CEDAR MESA 78",
        );
        let p = g.played(T0 + 25 * MIN);
        let notes: Vec<(&str, bool, bool)> = p
            .checkins
            .iter()
            .map(|j| (j.note.as_str(), j.counted, j.pending))
            .collect();
        assert_eq!(notes[0], ("captured for Red", true, false));
        assert_eq!(notes[1], ("wrong code word", false, false));
        assert!(notes[2].2, "slot 1 isn't revealed yet");
        assert_eq!(notes[3], ("one try per code word", false, false));
        assert_eq!(p.holders["c"], "Red");
        assert!(p.checkins[0].by_word);
    }

    #[test]
    fn a_fox_find_pays_the_finder_and_whoever_runs_it() {
        let mut g = G::new();
        let s = g.start(
            T0,
            json!({"foxes": [{"name": "Fox 1", "call": "N0FOX"}], "rules": {"encounter_points": 3}}),
        );
        let fox = json!({"fox": "Fox 1"});
        g.word(
            T0 + 5 * MIN,
            &s,
            "N0AAA",
            "Red",
            fox.clone(),
            "CEDAR MESA 77",
        );
        g.word(
            T0 + 10 * MIN,
            &s,
            "N0BBB",
            "Blue",
            fox.clone(),
            "CEDAR MESA 77",
        );
        // a minute into the next slot the old word still counts, but N0AAA already found it
        g.word(
            T0 + 16 * MIN,
            &s,
            "N0AAA",
            "Red",
            fox.clone(),
            "CEDAR MESA 77",
        );
        g.reveal(T0 + 17 * MIN, &s, fox, 0, "CEDAR MESA 77");
        let p = g.played(T0 + 18 * MIN);
        assert_eq!(player(&p, "N0AAA").points, 20);
        assert_eq!(player(&p, "N0AAA").finds, 1);
        assert_eq!(player(&p, "N0BBB").points, 20);
        assert_eq!(p.checkins[2].note, "already found Fox 1");
        let op = player(&p, "N0FOX");
        assert_eq!(
            (op.points, op.team.as_str()),
            (6, ""),
            "3 for each of two finders"
        );
        assert_eq!(team(&p, "Red").fox, 20);
        assert_eq!(team(&p, "Red").total, 20);
        assert!(p.holders.is_empty(), "a fox isn't territory");

        // after the game an admin says where it was, for the map
        let end = json!({"session": s, "foxes": {"Fox 1": [35.4, -120.6], "Nope": [1, 1]}});
        g.f.add("game.end", end, T0 + 20 * MIN);
        let foxes = &g.sessions(T0 + 21 * MIN)[0].foxes;
        assert_eq!(foxes[0].at, crate::world::Pos::new(35.4, -120.6));
    }

    #[test]
    fn worked_out_minutes_match_a_fresh_count_after_late_data() {
        let mut g = G::new();
        g.polls(T0 - 10 * MIN, T0 + 20 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + MIN, &s, "N0BBB", "Red", "b");
        let mut m = Minutes::default();
        let first = g.played_with(T0 + 60 * MIN, &mut m);
        assert_eq!(
            team(&first, "Red").chain,
            30 * 2,
            "fresh data runs out at +30"
        );

        // polls that were stuck behind a split arrive, and a check-in made meanwhile
        g.polls(T0 + 21 * MIN, T0 + 60 * MIN, "a", &["b"]);
        g.checkin(T0 + 40 * MIN, &s, "N0CCC", "Blue", "b");
        let again = g.played_with(T0 + 60 * MIN, &mut m);
        assert_eq!(again, g.played(T0 + 60 * MIN));
        assert_eq!(
            team(&again, "Red").chain,
            39 * 2,
            "up to +39, when Blue took b"
        );

        // and the next minutes carry on from there
        g.polls(T0 + 61 * MIN, T0 + 70 * MIN, "a", &["b"]);
        assert_eq!(
            g.played_with(T0 + 70 * MIN, &mut m),
            g.played(T0 + 70 * MIN)
        );

        // a check-in on a minute mark, the first poll of a new day, and an end that
        // arrives late all reach back into minutes already worked out
        g.checkin(T0 + 50 * MIN, &s, "N0AAA", "Red", "b");
        g.poll(T0 + 45 * MIN, "b", &["c"]);
        g.f.add("game.end", json!({"session": s}), T0 + 55 * MIN);
        assert_eq!(
            g.played_with(T0 + 80 * MIN, &mut m),
            g.played(T0 + 80 * MIN)
        );
    }

    #[test]
    fn a_link_stops_being_new_after_the_window() {
        let mut g = G::new();
        g.poll(T0 - 101 * DAY, "a", &[]);
        for d in [100, 99, 98] {
            g.poll(T0 - d * DAY, "a", &["b"]);
        }
        g.polls(T0 - 10 * MIN, T0 + 10 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({}));
        g.checkin(T0, &s, "N0AAA", "Red", "a");
        g.checkin(T0, &s, "N0BBB", "Red", "b");
        assert_eq!(
            team(&g.played(T0 + 10 * MIN), "Red").builder,
            0,
            "first seen 100 days ago"
        );

        let mut g = G::new();
        g.poll(T0 - 101 * DAY, "a", &[]);
        for d in [100, 99, 98] {
            g.poll(T0 - d * DAY, "a", &["b"]);
        }
        g.polls(T0 - 10 * MIN, T0 + 10 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({"rules": {"builder_window_days": 200}}));
        g.checkin(T0, &s, "N0AAA", "Red", "a");
        g.checkin(T0, &s, "N0BBB", "Red", "b");
        assert_eq!(team(&g.played(T0 + 10 * MIN), "Red").builder, 10 * 2);
    }

    #[test]
    fn the_debrief_lists_each_drop_and_whose_chain_it_broke() {
        let mut g = G::new();
        g.polls(T0 - 10 * MIN, T0 + 10 * MIN, "a", &["b"]);
        g.polls(T0 + 11 * MIN, T0 + 20 * MIN, "a", &[]);
        g.polls(T0 + 21 * MIN, T0 + 30 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({}));
        g.checkin(T0, &s, "N0AAA", "Red", "a");
        g.checkin(T0, &s, "N0BBB", "Red", "b");
        let p = g.played(T0 + 30 * MIN);
        assert_eq!(p.drops.len(), 1);
        let d = &p.drops[0];
        assert_eq!(d.link, ("a".to_string(), "b".to_string()));
        assert_eq!((d.from, d.to), (T0 + 12 * MIN, Some(T0 + 21 * MIN)));
        assert_eq!(d.broke, ["Red"]);
        assert_eq!(team(&p, "Red").best_chain, 2);
    }
}
