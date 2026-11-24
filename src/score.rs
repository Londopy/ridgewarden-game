//! Scoring a session. Everything here is worked out from the log and the mesh's own link
//! data, so every site that holds the same events shows the same leaderboard.
//!
//! First comes which links were up at each minute after the start. Then the check-ins,
//! code words and fox codes are judged in time order, minute by minute: one that counts
//! captures the site for the player's team, unless the team already holds it, or finds
//! the fox, and a held site whose links have all been down for `hold_min` goes back to
//! nobody. Then every minute each team scores for its chains: two or more of its sites
//! joined by links that are up at that minute. A dropped link breaks the chain at once, so
//! its points stop until the link is back. Minutes already worked out are kept (`Minutes`)
//! and only redone when new data could change them.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ridgewarden_client::graph::Edge;
use ridgewarden_client::util::short_time;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::chains::{Chain, chains};
use crate::game::{self, Bearing, How, Keys, MIN, Play, Rules, Session, Spot, Target};
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

/// A site that went back to nobody because its links were down.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Lost {
    pub ts: i64,
    pub site: String,
    pub team: String,
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
    /// for each held site, when it was taken or last had a link up; it goes back to nobody
    /// once `hold_min` has passed since then
    #[serde(default)]
    pub linked: BTreeMap<String, i64>,
    pub chains: Vec<Chain>,
    pub up: BTreeSet<Edge>,
    pub known: BTreeSet<Edge>,
    pub positions: BTreeMap<String, Pos>,
    pub drops: Vec<Drop>,
    /// oldest first
    #[serde(default)]
    pub lost: Vec<Lost>,
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

/// From `.0` on, site `.1` is held by team `.2`, or by nobody.
type Change = (i64, String, Option<String>);

#[derive(Default)]
struct Judge {
    keys: Keys,
    reveals: Reveals,
    holder: BTreeMap<String, String>,
    /// for each held site, when it was taken or last had a link up
    linked: BTreeMap<String, i64>,
    captures: BTreeMap<(String, String), u32>,
    team_of: BTreeMap<String, String>,
    last: BTreeMap<String, (i64, String)>,
    strikes: BTreeMap<String, i64>,
    counted: Vec<(i64, String, String, String)>,
    /// (site, callsign) of everyone who has had encounter points there
    met: BTreeSet<(String, String)>,
    /// (callsign, target, slot) of every code word tried
    tried: BTreeSet<(String, String, i64)>,
    /// hashes of the fox codes used
    used: BTreeSet<String>,
    found: BTreeSet<(String, String)>,
    players: BTreeMap<String, PlayerScore>,
    teams: BTreeMap<String, TeamScore>,
    /// captures, and sites going back to nobody
    changes: Vec<Change>,
    lost: Vec<Lost>,
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
    fn new(s: &Session, keys: Keys, reveals: Reveals) -> Self {
        let mut j = Judge {
            keys,
            reveals,
            ..Default::default()
        };
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

    /// Whether a play was signed by the key its callsign plays with, or by any player key
    /// while the callsign has none.
    fn signed(&self, p: &Play) -> Option<Verdict> {
        match (&p.key, self.keys.get(&p.call)) {
            (None, _) => Some(Verdict::No("not signed with the player's own key".into())),
            (Some(k), Some(bound)) if k != bound => Some(Verdict::No(format!(
                "signed with a key that isn't {}'s",
                p.call
            ))),
            _ => None,
        }
    }

    /// For a code word claim: one try per word, and the word must match its slot's. For a
    /// fox, one of its codes that nobody has used.
    fn proof(&mut self, s: &Session, p: &Play) -> Option<Verdict> {
        let How::Word { word } = &p.how else {
            return None;
        };
        if let Target::Fox(fox) = &p.target {
            return self.redeem(s, &p.call, fox, word);
        }
        let slot = words::slot_of(s, p.ts);
        if !self.tried.insert((p.call.clone(), p.target.key(), slot)) {
            return Some(Verdict::No("one try per code word".into()));
        }
        match words::check(s, &self.reveals, &p.target, p.ts, word) {
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

    /// A fox code counts once, for the first player to send it, and a player finds each fox
    /// once; a second find doesn't use up the code.
    fn redeem(&mut self, s: &Session, call: &str, fox: &str, typed: &str) -> Option<Verdict> {
        if self.found.contains(&(call.to_string(), fox.to_string())) {
            return Some(Verdict::No(format!("already found {fox}")));
        }
        let hash = words::code_hash(&s.nonce, fox, typed);
        if !s.fox(fox).is_some_and(|f| f.codes.contains(&hash)) {
            return Some(Verdict::No(format!("not one of {fox}'s codes")));
        }
        if !self.used.insert(hash) {
            return Some(Verdict::No("that code was already used".into()));
        }
        None
    }

    /// At minute `t`, with `up` the game sites that have a link up: a held site whose
    /// links have all been down for `hold_min` since it was taken or last had one up goes
    /// back to nobody.
    fn decay(&mut self, s: &Session, t: i64, up: &BTreeSet<&str>) {
        let mut gone = vec![];
        for (site, since) in &mut self.linked {
            if up.contains(site.as_str()) {
                *since = t;
            } else if t - *since >= s.rules.hold_min * MIN {
                gone.push(site.clone());
            }
        }
        for site in gone {
            self.linked.remove(&site);
            if let Some(team) = self.holder.remove(&site) {
                self.changes.push((t, site.clone(), None));
                self.lost.push(Lost { ts: t, site, team });
            }
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

    fn judge(&mut self, s: &Session, w: &World, p: &Play) -> Verdict {
        if p.ts < s.start || p.ts > s.end {
            return Verdict::No("outside the game's time".into());
        }
        if let Some(v) = place(s, w, p).or_else(|| self.signed(p)) {
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
        self.proof(s, p)
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
            self.linked.insert(site.to_string(), p.ts);
            self.changes
                .push((p.ts, site.to_string(), Some(p.team.clone())));
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
        self.found.insert((p.call.clone(), fox.to_string()));
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

    /// Judge every play up to `at`, in order, letting held sites decay minute by minute in
    /// between. `up[i]` is the game sites with a link up at start + (i + 1) minutes.
    fn judge_all(
        &mut self,
        s: &Session,
        w: &World,
        plays: &[Play],
        up: &[BTreeSet<&str>],
        at: i64,
    ) -> Vec<Judged> {
        let mut out = vec![];
        let mut minutes = up
            .iter()
            .enumerate()
            .map(|(i, u)| (s.start + (i as i64 + 1) * MIN, u))
            .peekable();
        for p in plays.iter().filter(|p| p.ts <= at) {
            // a site that decays at the very moment of a check-in is free for it to take
            while let Some((t, u)) = minutes.next_if(|(t, _)| *t <= p.ts) {
                self.decay(s, t, u);
            }
            let (counted, pending, points, note) = match self.judge(s, w, p) {
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
        for (t, u) in minutes {
            self.decay(s, t, u);
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

/// What one chain earns a minute: (chain points, builder bonus).
pub fn chain_rate(r: &Rules, c: &Chain) -> (i64, i64) {
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
    /// (team index, link) for every link in a chain
    chained: BTreeSet<(usize, u32)>,
}

/// What a session's minutes depend on besides the data: change any of it and every
/// minute is redone.
type Shape = (i64, i64, Vec<String>, Vec<String>, Rules);

/// The minutes of one session worked out so far, kept between page views: which links
/// were up in each, and what each scored. New polls from before the last minute drop
/// both from then on; a change in who held what when drops the scores from then on.
/// Only the minutes dropped are worked out again.
#[derive(Default)]
pub struct Minutes {
    shape: Option<Shape>,
    changes: Vec<Change>,
    edges: Vec<Edge>,
    ids: BTreeMap<Edge, u32>,
    /// the links up in each minute; minute i is at start + (i + 1) minutes
    ups: Vec<BTreeSet<u32>>,
    /// the minutes scored, never more than `ups`
    done: Vec<Minute>,
    /// the oldest new poll since the last run
    dirty: Option<i64>,
}

/// How many of the first `len` minutes come before `cut`: minute i, at start + (i + 1)
/// minutes, sees everything up to it.
fn before(s: &Session, cut: i64, len: usize) -> usize {
    cut.saturating_sub(s.start + 1)
        .div_euclid(MIN)
        .clamp(0, len as i64) as usize
}

impl Minutes {
    /// Note that polls from `ts` on arrived since the last run.
    pub fn touch(&mut self, ts: i64) {
        self.dirty = Some(self.dirty.map_or(ts, |d| d.min(ts)));
    }

    /// Start again if the session changed shape, and drop the minutes that polls which
    /// arrived since the last run could change.
    fn refresh(&mut self, s: &Session) {
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
        if let Some(cut) = self.dirty.take() {
            let keep = before(s, cut, self.ups.len());
            self.ups.truncate(keep);
            self.done.truncate(keep);
        }
    }

    /// Take the latest captures and losses; minutes scored from the first one that
    /// differs on are scored again.
    fn rescore(&mut self, s: &Session, changes: &[Change]) {
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
        if let Some(cut) = changed {
            let keep = before(s, cut, self.done.len());
            self.done.truncate(keep);
        }
        self.changes = changes.to_vec();
    }

    fn next(&self, s: &Session) -> i64 {
        s.start + (self.ups.len() as i64 + 1) * MIN
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

    /// Work out which links were up in each minute from the first missing one up to `at`.
    fn find_ups(&mut self, s: &Session, w: &World, at: i64) {
        let mut t = self.next(s);
        while t <= at {
            let up = w.up_at(t, &s.rules).iter().map(|e| self.id(e)).collect();
            self.ups.push(up);
            t += MIN;
        }
    }

    /// For each minute worked out, the game sites with a link up.
    fn linked<'a>(&'a self, sites: &[String]) -> Vec<BTreeSet<&'a str>> {
        self.ups
            .iter()
            .map(|up| {
                up.iter()
                    .flat_map(|e| {
                        let (a, b) = &self.edges[*e as usize];
                        [a, b]
                    })
                    .filter(|n| sites.contains(n))
                    .map(String::as_str)
                    .collect()
            })
            .collect()
    }

    /// Score every minute whose links are known and that isn't scored yet.
    fn score(&mut self, s: &Session, w: &World) {
        let mut holders = BTreeMap::new();
        let mut changes = self.changes.iter().peekable();
        for i in self.done.len()..self.ups.len() {
            let t = s.start + (i as i64 + 1) * MIN;
            while let Some((_, site, team)) = changes.next_if(|(ts, ..)| *ts <= t) {
                match team {
                    Some(team) => holders.insert(site.clone(), team.clone()),
                    None => holders.remove(site),
                };
            }
            let up: BTreeSet<Edge> = self.ups[i]
                .iter()
                .map(|e| self.edges[*e as usize].clone())
                .collect();
            let mut m = Minute {
                points: vec![(0, 0); s.teams.len()],
                best: vec![0; s.teams.len()],
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
                    m.chained.insert((ti, self.ids[e]));
                }
            }
            self.done.push(m);
        }
    }

    /// Links that went down during the game, from the minutes scored.
    fn drops(&self, s: &Session) -> Vec<Drop> {
        let mut out: Vec<Drop> = vec![];
        let mut open: BTreeMap<u32, usize> = BTreeMap::new();
        for i in 0..self.done.len() {
            let t = s.start + (i as i64 + 1) * MIN;
            if let Some(p) = i.checked_sub(1) {
                for e in self.ups[p].difference(&self.ups[i]) {
                    let broke = self.done[p]
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
            for e in &self.ups[i] {
                if let Some(d) = open.remove(e) {
                    out[d].to = Some(t);
                }
            }
        }
        out.sort_by(|a, b| (a.from, &a.link).cmp(&(b.from, &b.link)));
        out
    }

    /// Every link that was up in some minute worked out.
    fn ever_up(&self) -> BTreeSet<Edge> {
        self.ups
            .iter()
            .flatten()
            .map(|e| self.edges[*e as usize].clone())
            .collect()
    }
}

/// Polls are loaded this many minutes at a time, so a long game doesn't hold them all.
const CHUNK: i64 = 6 * 60;

/// Score session `s` as of `now` with the players' keys `keys`, reusing and extending
/// `minutes`.
pub fn play(
    conn: &Connection,
    s: &Session,
    now: i64,
    keys: &Keys,
    minutes: &mut Minutes,
) -> Result<Played> {
    let at = s.end.min(now);
    let mut w = World::places(conn)?;
    minutes.refresh(s);
    // each minute needs the polls from a little before it
    let fresh = 2 * s.rules.fresh_s * 1000;
    while minutes.next(s) <= at {
        let from = minutes.next(s);
        let to = (from + CHUNK * MIN).min(at);
        w.load_polls(conn, from - fresh, to)?;
        minutes.find_ups(s, &w, to);
    }
    let mut j = Judge::new(s, keys.clone(), game::reveals(conn, s)?);
    let plays = game::plays(conn, &s.id)?;
    let checkins = j.judge_all(s, &w, &plays, &minutes.linked(&s.all_sites()), at);
    minutes.rescore(s, &j.changes);
    minutes.score(s, &w);
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
        lost: j.lost,
        bearings: game::bearings(conn, s, keys)?,
        spots: game::spots(conn, &s.id, keys)?,
        positions: w.positions,
        session: s.clone(),
        at,
        teams,
        players,
        checkins,
        holders: j.holder,
        linked: j.linked,
        chains,
        up,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::DAY;
    use crate::testing::{G, T0, pk, player, player_key, signed, team};
    use ed25519_dalek::SigningKey;
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
        // c keeps a link up, so whoever takes it still holds it at the end
        g.polls(T0 - MIN, T0 + 100 * MIN, "c", &["r"]);
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
        let checkin = |s: &str| {
            let p = json!({"session": s, "call": "N0AAA", "team": "Red", "site": "a", "t": T0});
            signed("game.checkin", p, &player_key("N0AAA"))
        };
        let mut g = G::new();
        g.poll(T0 - MIN, "a", &[]);
        let s = g.start(T0, json!({}));
        g.add_as(&other, 1000, "game.checkin", checkin(&s), T0 + MIN);
        let p = g.played(T0 + 2 * MIN);
        assert!(!p.checkins[0].counted);
        assert!(p.checkins[0].note.contains("countersigned"));

        // the core that polls a over its LAN is a's core
        let mut g = G::new();
        let poll = json!({"node": "a", "ok": true, "via": "lan"});
        g.add_as(&other, 999, "node.poll", poll, T0);
        let s = g.start(T0, json!({}));
        g.add_as(&other, 1000, "game.checkin", checkin(&s), T0 + MIN);
        assert!(g.played(T0 + 2 * MIN).checkins[0].counted);

        // a core that only crawls a node isn't that node's core
        let mut g = G::new();
        let poll = json!({"node": "a", "ok": true, "via": "crawl"});
        g.add_as(&other, 999, "node.poll", poll, T0);
        let s = g.start(T0, json!({}));
        g.add_as(&other, 1000, "game.checkin", checkin(&s), T0 + MIN);
        assert!(!g.played(T0 + 2 * MIN).checkins[0].counted);
    }

    #[test]
    fn plays_count_only_when_signed_by_the_key_the_callsign_plays_with() {
        let notes = |p: &Played| -> Vec<(String, String, bool)> {
            p.checkins
                .iter()
                .map(|c| (c.call.clone(), c.note.clone(), c.counted))
                .collect()
        };
        let mut g = G::new();
        let s = g.start(T0, json!({}));
        let (mine, thief) = (player_key("N0AAA"), SigningKey::from_bytes(&[5; 32]));
        let at = |site: &str, ts: i64| json!({"session": s, "call": "N0AAA", "team": "Red", "site": site, "t": ts});
        // a check-in on a site's LAN makes its key the callsign's
        g.post(T0 + MIN, "game.checkin", at("a", T0 + MIN), &mine);
        g.post(T0 + 2 * MIN, "game.checkin", at("b", T0 + 2 * MIN), &thief);
        // a post without the player's signature, and one whose signature is for other text
        let unsigned = json!({"session": s, "call": "N0CCC", "team": "Red", "site": "c"});
        g.f.add("game.checkin", unsigned, T0 + 3 * MIN);
        let mut forged = signed("game.checkin", at("c", T0 + 4 * MIN), &mine);
        forged["site"] = json!("b");
        g.f.add("game.checkin", forged, T0 + 4 * MIN);
        let p = g.played(T0 + 5 * MIN);
        let n = notes(&p);
        assert!(n[0].2, "{n:?}");
        assert_eq!(n[1].1, "signed with a key that isn't N0AAA's");
        assert_eq!(n[2].1, "not signed with the player's own key");
        assert_eq!(n[3].1, "not signed with the player's own key");
        assert!(n.iter().skip(1).all(|x| !x.2));
        assert_eq!(p.holders.get("b"), None);

        // the same signed post sent on again counts once, also with its signature shouted
        let replay = signed("game.checkin", at("c", T0 + 6 * MIN), &mine);
        let mut shouted = replay.clone();
        shouted["psig"] = json!(replay["psig"].as_str().unwrap().to_uppercase());
        g.f.add("game.checkin", replay.clone(), T0 + 6 * MIN);
        g.f.add("game.checkin", replay, T0 + 7 * MIN);
        g.f.add("game.checkin", shouted, T0 + 7 * MIN + 1);
        assert_eq!(g.played(T0 + 8 * MIN).checkins.len(), 5);

        // an admin can give the callsign to the other key; nobody else can
        let other_admin = "ef".repeat(32);
        let set = json!({"call": "N0AAA", "key": pk(&mine)});
        g.add_as(&other_admin, 1000, "game.key", set, T0 + 8 * MIN);
        let set = json!({"call": "N0AAA", "key": pk(&thief)});
        g.f.add("game.key", set, T0 + 9 * MIN);
        let n = notes(&g.played(T0 + 10 * MIN));
        assert_eq!(n[0].1, "signed with a key that isn't N0AAA's");
        assert!(n[1].2, "{n:?}");
    }

    fn keys_of(g: &G) -> game::Keys {
        game::keys(&g.f.conn, std::slice::from_ref(&g.key)).unwrap()
    }

    #[test]
    fn a_post_from_afar_doesnt_take_a_callsign_but_a_lan_check_in_does() {
        let mut g = G::new();
        let s = g.start(T0, json!({"foxes": [{"name": "Fox 1"}]}));
        let (mine, thief) = (player_key("N0AAA"), SigningKey::from_bytes(&[5; 32]));
        let spot = |text: &str| json!({"session": s, "call": "N0AAA", "text": text, "t": T0});
        // first with the callsign, but from anywhere: shown while the callsign has no key
        g.post(T0 + MIN, "game.spot", spot("not me"), &thief);
        assert_eq!(g.played(T0 + 2 * MIN).spots.len(), 1);
        assert_eq!(keys_of(&g).get("N0AAA"), None);
        g.checkin(T0 + 2 * MIN, &s, "N0AAA", "Red", "a");
        assert_eq!(keys_of(&g).get("N0AAA"), Some(&pk(&mine)));
        g.post(T0 + 3 * MIN, "game.spot", spot("heading north"), &mine);
        let bearing = json!({"session": s, "call": "N0AAA", "fox": "Fox 1", "lat": "35.30000",
            "lon": "-120.66000", "deg": "45.0", "t": T0});
        g.post(T0 + 4 * MIN, "game.bearing", bearing.clone(), &thief);
        g.post(T0 + 5 * MIN, "game.bearing", bearing, &mine);
        let p = g.played(T0 + 6 * MIN);
        let texts: Vec<&str> = p.spots.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, ["heading north"]);
        assert_eq!(p.bearings.len(), 1);
        assert_eq!((p.bearings[0].ts, p.bearings[0].deg), (T0 + 5 * MIN, 45.0));
        assert_eq!(
            p.bearings[0].from,
            crate::world::Pos::new(35.3, -120.66).unwrap()
        );
    }

    #[test]
    fn with_a_roster_only_rostered_callsigns_get_keys() {
        let mut g = G::new();
        let s = g.start(T0, json!({}));
        let teams = json!({"session": s, "teams": ["Red", "Blue"], "roster": {"N0AAA": "Red"}});
        g.f.add("game.teams", teams, T0 + MIN);
        let spot = |call: &str| json!({"session": s, "call": call, "text": "here", "t": T0});
        g.post(
            T0 + 2 * MIN,
            "game.spot",
            spot("N0VIC"),
            &SigningKey::from_bytes(&[5; 32]),
        );
        g.post(
            T0 + 3 * MIN,
            "game.spot",
            spot("N0AAA"),
            &player_key("N0AAA"),
        );
        let keys = keys_of(&g);
        assert_eq!(keys.get("N0VIC"), None, "not on the roster");
        assert_eq!(
            keys.get("N0AAA"),
            Some(&pk(&player_key("N0AAA"))),
            "on it, from anywhere"
        );
    }

    #[test]
    fn the_first_check_in_is_first_by_lamport_not_by_a_cores_clock() {
        let mut g = G::new();
        let s = g.start(T0, json!({}));
        let slow = "ab".repeat(32);
        g.add_as(
            &slow,
            900,
            "node.poll",
            json!({"node": "b", "ok": true, "via": "lan"}),
            T0,
        );
        let checkin = |site: &str, key: &SigningKey| {
            let p = json!({"session": s, "call": "N0AAA", "team": "Red", "site": site, "t": T0});
            signed("game.checkin", p, key)
        };
        let (owner, late) = (player_key("N0AAA"), SigningKey::from_bytes(&[5; 32]));
        let ours = g.key.clone();
        g.add_as(
            &ours,
            1000,
            "game.checkin",
            checkin("a", &owner),
            T0 + 10 * MIN,
        );
        // logged after it, by a core whose clock is behind
        g.add_as(&slow, 1001, "game.checkin", checkin("b", &late), T0 + MIN);
        assert_eq!(keys_of(&g).get("N0AAA"), Some(&pk(&owner)));
    }

    #[test]
    fn a_held_site_goes_back_to_nobody_once_its_links_stay_down() {
        let mut g = G::new();
        g.polls(T0 - 10 * MIN, T0 + 20 * MIN, "a", &["b"]);
        g.polls(T0 + 21 * MIN, T0 + 90 * MIN, "a", &[]);
        // c's link drops for 20 minutes at a time, never for the full half hour
        g.polls(T0, T0 + 10 * MIN, "c", &["r"]);
        g.polls(T0 + 11 * MIN, T0 + 30 * MIN, "c", &[]);
        g.polls(T0 + 31 * MIN, T0 + 32 * MIN, "c", &["r"]);
        g.polls(T0 + 33 * MIN, T0 + 52 * MIN, "c", &[]);
        g.polls(T0 + 53 * MIN, T0 + 90 * MIN, "c", &["r"]);
        let s = g.start(T0, json!({}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + MIN, &s, "N0BBB", "Red", "b");
        g.checkin(T0 + MIN, &s, "N0CCC", "Red", "c");

        // a-b is up until +21; the last minute either had a link was +21
        let p = g.played(T0 + 50 * MIN);
        assert_eq!((p.holders.len(), p.lost.len()), (3, 0));
        let linked: Vec<(&str, i64)> = p.linked.iter().map(|(s, t)| (s.as_str(), *t)).collect();
        assert_eq!(
            linked,
            [
                ("a", T0 + 21 * MIN),
                ("b", T0 + 21 * MIN),
                ("c", T0 + 33 * MIN)
            ],
            "the last minute each had a link up"
        );
        let p = g.played(T0 + 51 * MIN);
        let lost: Vec<(i64, &str, &str)> = p
            .lost
            .iter()
            .map(|l| (l.ts, l.site.as_str(), l.team.as_str()))
            .collect();
        assert_eq!(
            lost,
            [(T0 + 51 * MIN, "a", "Red"), (T0 + 51 * MIN, "b", "Red")]
        );
        assert_eq!(p.holders.keys().collect::<Vec<_>>(), ["c"]);
        assert_eq!(team(&p, "Red").held, 1);

        // lost is lost: taking it again is a capture, and a repeat one for Red
        g.checkin(T0 + 55 * MIN, &s, "N0DDD", "Blue", "a");
        g.checkin(T0 + 56 * MIN, &s, "N0BBB", "Red", "b");
        let p = g.played(T0 + 60 * MIN);
        let pts: Vec<(&str, i64)> = p.checkins[3..]
            .iter()
            .map(|c| (c.note.as_str(), c.points))
            .collect();
        assert_eq!(
            pts,
            [
                ("captured for Blue", 10),
                ("captured for Red (2nd time)", 5)
            ]
        );
        assert_eq!(p.holders["a"], "Blue");

        // the hold comes from the rules; with no polls after +20, the link counts as up
        // until its data is 10 minutes old, at +30
        let mut g = G::new();
        g.polls(T0 - 10 * MIN, T0 + 20 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({"rules": {"hold_min": 60}}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        assert!(g.played(T0 + 89 * MIN).lost.is_empty());
        assert_eq!(g.played(T0 + 90 * MIN).lost.len(), 1);
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
            &admins,
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
            std::slice::from_ref(&g.key),
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
    fn a_result_frozen_before_holds_and_losses_were_kept_still_reads_back() {
        let mut g = G::new();
        g.polls(T0 - 10 * MIN, T0 + 30 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({"minutes": 20}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + MIN, &s, "N0BBB", "Red", "b");
        let frozen = g.played(T0 + 3 * 60 * MIN);
        assert!(!frozen.linked.is_empty());
        // older versions froze results without these
        let mut old = serde_json::to_value(&frozen).unwrap();
        for field in ["linked", "lost"] {
            old.as_object_mut().unwrap().remove(field);
        }
        g.f.conn
            .execute("UPDATE results SET json = ?1", [old.to_string()])
            .unwrap();
        // with the polls gone, scoring it again would come out differently
        g.f.conn.execute("DELETE FROM polls", []).unwrap();
        let back = g.played(T0 + 4 * 60 * MIN);
        assert_eq!(back.teams, frozen.teams, "read back, not scored again");
        assert!(back.linked.is_empty() && back.lost.is_empty());
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
    fn a_fox_is_found_with_one_of_its_codes_and_each_code_counts_once() {
        let mut g = G::new();
        let codes = ["AAAA-BBBB-CCCC", "DDDD-EEEE-FFFF", "GGGG-HHHH-JJJJ"];
        let hashes: Vec<String> = codes
            .iter()
            .map(|c| words::code_hash("ab", "Fox 1", c))
            .collect();
        let s = g.start(
            T0,
            json!({"nonce": "ab", "foxes": [{"name": "Fox 1", "call": "N0FOX", "codes": hashes}],
                "rules": {"encounter_points": 3}}),
        );
        let fox = json!({"fox": "Fox 1"});
        let mut try_code = |ts: i64, call: &str, team: &str, code: &str| {
            g.word(T0 + ts * MIN, &s, call, team, fox.clone(), code)
        };
        try_code(5, "N0AAA", "Red", "aaaa bbbb cccc");
        // passed on to someone else, it's already spent
        try_code(6, "N0BBB", "Blue", "AAAA-BBBB-CCCC");
        try_code(7, "N0BBB", "Blue", "DDDD-EEEE-FFFX");
        // a second find doesn't use up a code, so it's still good for someone else
        try_code(8, "N0AAA", "Red", "GGGG-HHHH-JJJJ");
        try_code(9, "N0CCC", "Blue", "GGGG-HHHH-JJJJ");
        let p = g.played(T0 + 10 * MIN);
        let notes: Vec<(&str, bool)> = p
            .checkins
            .iter()
            .map(|c| (c.note.as_str(), c.counted))
            .collect();
        assert_eq!(
            notes,
            [
                ("found Fox 1; 3 to N0FOX, who runs it", true),
                ("that code was already used", false),
                ("not one of Fox 1's codes", false),
                ("already found Fox 1", false),
                ("found Fox 1; 3 to N0FOX, who runs it", true),
            ]
        );
        assert_eq!(player(&p, "N0AAA").points, 20);
        assert_eq!(player(&p, "N0AAA").finds, 1);
        let op = player(&p, "N0FOX");
        assert_eq!(
            (op.points, op.team.as_str()),
            (6, ""),
            "3 for each of two finders"
        );
        assert_eq!((team(&p, "Red").fox, team(&p, "Blue").fox), (20, 20));
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
