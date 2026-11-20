//! Everything across games: seasons and their standings, each player's record, awards in
//! the style of POTA's (earned by doing, never prizes), and the numbers that show whether
//! the game is doing its job.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Result;
use ridgewarden_client::graph::Edge;
use rusqlite::Connection;

use crate::game::{self, Season};
use crate::score::Played;
use crate::world::World;

/// One player's games and what they did in them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Record {
    pub call: String,
    /// (game id, game name, start, team, points), oldest first
    pub games: Vec<(String, String, i64, String, i64)>,
    pub points: i64,
    pub sites: BTreeSet<String>,
    pub captures: u64,
    pub encounters: u64,
    pub finds: u64,
    /// the longest chain a team of theirs held, in sites
    pub best_chain: u64,
    /// games where their team scored on a new link
    pub builder_games: u64,
}

impl Record {
    fn add(&mut self, p: &Played) {
        let Some(me) = p.players.iter().find(|x| x.call == self.call) else {
            return;
        };
        self.games.push((
            p.session.id.clone(),
            p.session.name.clone(),
            p.session.start,
            me.team.clone(),
            me.points,
        ));
        self.points += me.points;
        self.sites.extend(me.sites.iter().cloned());
        self.captures += u64::from(me.captures);
        self.encounters += u64::from(me.encounters);
        self.finds += u64::from(me.finds);
        if let Some(t) = p.teams.iter().find(|t| t.team == me.team) {
            self.best_chain = self.best_chain.max(t.best_chain as u64);
            self.builder_games += u64::from(t.builder > 0);
        }
    }
}

struct Def {
    name: &'static str,
    tiers: &'static [u64],
    /// what earning it means, with {} for the tier
    what: &'static str,
    value: fn(&Record) -> u64,
}

const AWARDS: &[Def] = &[
    Def {
        name: "Site visitor",
        tiers: &[3, 10, 25],
        what: "counted at {} different sites",
        value: |r| r.sites.len() as u64,
    },
    Def {
        name: "Regular",
        tiers: &[3, 10, 25],
        what: "played {} games",
        value: |r| r.games.len() as u64,
    },
    Def {
        name: "Fox finder",
        tiers: &[1, 5, 20],
        what: "found {} foxes",
        value: |r| r.finds,
    },
    Def {
        name: "Meet and greet",
        tiers: &[5, 25],
        what: "met players from other teams {} times",
        value: |r| r.encounters,
    },
    Def {
        name: "Chain gang",
        tiers: &[3, 5, 8],
        what: "on a team that held a chain of {} sites",
        value: |r| r.best_chain,
    },
    Def {
        name: "Builder's friend",
        tiers: &[1],
        what: "on a team that scored on a new link in {} game",
        value: |r| r.builder_games,
    },
];

/// One tier of one award, and who has earned it.
#[derive(Clone, Debug, PartialEq)]
pub struct Award {
    pub name: String,
    pub what: String,
    /// (callsign, the game it was earned in), in the order they earned it
    pub earned: Vec<(String, String)>,
}

/// Awards earned over `games` (oldest first): each tier goes to a player in the game
/// where their running total first reached it.
pub fn awards(games: &[Arc<Played>]) -> Vec<Award> {
    let mut records: BTreeMap<String, Record> = BTreeMap::new();
    let mut out: Vec<Award> = AWARDS
        .iter()
        .flat_map(|d| {
            d.tiers.iter().map(|t| Award {
                name: format!("{} {t}", d.name),
                what: d.what.replace("{}", &t.to_string()),
                earned: vec![],
            })
        })
        .collect();
    for p in games {
        for pl in &p.players {
            let r = records.entry(pl.call.clone()).or_insert_with(|| Record {
                call: pl.call.clone(),
                ..Default::default()
            });
            r.add(p);
            let mut i = 0;
            for d in AWARDS {
                for t in d.tiers {
                    let a = &mut out[i];
                    i += 1;
                    if (d.value)(r) >= *t && !a.earned.iter().any(|(c, _)| *c == r.call) {
                        a.earned.push((r.call.clone(), p.session.name.clone()));
                    }
                }
            }
        }
    }
    out
}

/// A season: its games and standings.
#[derive(Clone, Debug)]
pub struct SeasonView {
    pub season: Season,
    pub games: Vec<Arc<Played>>,
    /// highest points first
    pub players: Vec<Record>,
    /// (team name, points), highest first; teams are matched by name across games
    pub teams: Vec<(String, i64)>,
    pub awards: Vec<Award>,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub games: usize,
    pub players: usize,
    /// (game name, id, players) of the game with the most players
    pub biggest: Option<(String, String, usize)>,
    /// games with 10 or more players
    pub ten_plus: usize,
    /// links that appeared after this app started watching, with the day they did
    pub new_links: Vec<(Edge, i64)>,
    pub new_nodes: Vec<(String, i64)>,
}

pub struct Archive {
    /// oldest first
    pub games: Vec<Arc<Played>>,
    pub seasons: Vec<SeasonView>,
    pub records: BTreeMap<String, Record>,
    pub awards: Vec<Award>,
    pub stats: Stats,
}

fn records(games: &[Arc<Played>]) -> BTreeMap<String, Record> {
    let mut out: BTreeMap<String, Record> = BTreeMap::new();
    for p in games {
        for pl in &p.players {
            out.entry(pl.call.clone())
                .or_insert_with(|| Record {
                    call: pl.call.clone(),
                    ..Default::default()
                })
                .add(p);
        }
    }
    out
}

fn season_view(season: Season, all: &[Arc<Played>]) -> SeasonView {
    let games: Vec<Arc<Played>> = all
        .iter()
        .filter(|p| p.session.season.as_deref() == Some(season.id.as_str()))
        .cloned()
        .collect();
    let mut players: Vec<Record> = records(&games).into_values().collect();
    players.sort_by(|a, b| b.points.cmp(&a.points).then_with(|| a.call.cmp(&b.call)));
    let mut teams: BTreeMap<String, i64> = BTreeMap::new();
    for p in &games {
        for t in &p.teams {
            *teams.entry(t.team.clone()).or_default() += t.total;
        }
    }
    let mut teams: Vec<(String, i64)> = teams.into_iter().collect();
    teams.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    SeasonView {
        awards: awards(&games),
        season,
        games,
        players,
        teams,
    }
}

fn stats(games: &[Arc<Played>], w: &World) -> Stats {
    let players: BTreeSet<&String> = games
        .iter()
        .flat_map(|p| p.players.iter().map(|x| &x.call))
        .collect();
    let biggest = games
        .iter()
        .max_by_key(|p| p.players.len())
        .filter(|p| !p.players.is_empty())
        .map(|p| {
            (
                p.session.name.clone(),
                p.session.id.clone(),
                p.players.len(),
            )
        });
    Stats {
        games: games.len(),
        players: players.len(),
        biggest,
        ten_plus: games.iter().filter(|p| p.players.len() >= 10).count(),
        new_links: w.new_links(),
        new_nodes: w.new_nodes(),
    }
}

/// Put the archive together from every game, newest first as `sessions` gives them.
pub fn build(conn: &Connection, admins: &[String], mut games: Vec<Arc<Played>>) -> Result<Archive> {
    games.reverse();
    let seasons = game::seasons(conn, admins)?
        .into_iter()
        .map(|s| season_view(s, &games))
        .collect();
    Ok(Archive {
        stats: stats(&games, &World::places(conn)?),
        records: records(&games),
        awards: awards(&games),
        seasons,
        games,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::MIN;
    use crate::testing::{G, T0};
    use serde_json::json;

    /// Three short games; N0AAA plays each at a different site, N0BBB always at a.
    fn three_games() -> (G, Vec<Arc<Played>>) {
        let mut g = G::new();
        let season = g.f.add(
            "game.season",
            json!({"action": "start", "name": "Winter"}),
            T0 - 60 * MIN,
        );
        let mut games = vec![];
        for (i, site) in ["a", "b", "c"].iter().enumerate() {
            let t = T0 + i as i64 * 1000 * MIN;
            let extra = if i < 2 {
                json!({"minutes": 30, "season": season.id})
            } else {
                json!({"minutes": 30})
            };
            let s = g.start(t, extra);
            g.checkin(t + MIN, &s, "N0AAA", "Red", site);
            g.checkin(t + 2 * MIN, &s, "N0BBB", "Blue", "a");
            games.push(Arc::new(g.played(t + 40 * MIN)));
        }
        (g, games)
    }

    #[test]
    fn awards_go_to_whoever_reaches_each_tier_in_the_game_they_reach_it() {
        let (_, games) = three_games();
        let aw = awards(&games);
        let get = |n: &str| aw.iter().find(|a| a.name == n).unwrap();
        assert_eq!(
            get("Site visitor 3").earned,
            [("N0AAA".to_string(), "Test game".to_string())]
        );
        assert_eq!(get("Regular 3").earned.len(), 2, "both played three games");
        assert!(get("Site visitor 10").earned.is_empty());
        assert!(get("Fox finder 1").earned.is_empty());
    }

    #[test]
    fn a_season_counts_only_its_own_games() {
        let (g, games) = three_games();
        let mut newest_first = games.clone();
        newest_first.reverse();
        let a = build(&g.f.conn, std::slice::from_ref(&g.key), newest_first).unwrap();
        assert_eq!(a.seasons.len(), 1);
        let s = &a.seasons[0];
        assert_eq!(s.games.len(), 2);
        let aaa = s.players.iter().find(|r| r.call == "N0AAA").unwrap();
        assert_eq!(aaa.games.len(), 2);
        assert_eq!(aaa.sites.len(), 2);
        assert_eq!(a.records["N0AAA"].games.len(), 3);
        assert_eq!(a.stats.games, 3);
        assert_eq!(a.stats.players, 2);
        assert_eq!(a.stats.ten_plus, 0);
    }

    #[test]
    fn the_numbers_show_what_the_mesh_grew_by() {
        let mut g = G::new();
        g.poll(T0 - 3 * crate::game::DAY, "a", &[]);
        g.poll(T0 - 2 * crate::game::DAY, "n", &["a"]);
        g.poll(T0 - crate::game::DAY, "a", &["b"]);
        let a = build(&g.f.conn, std::slice::from_ref(&g.key), vec![]).unwrap();
        let nodes: Vec<&str> = a.stats.new_nodes.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(nodes, ["n"], "b was only heard of, never polled");
        let links: Vec<&Edge> = a.stats.new_links.iter().map(|(e, _)| e).collect();
        assert_eq!(
            links.len(),
            2,
            "a-n and a-b both appeared after a was first polled"
        );
    }
}
