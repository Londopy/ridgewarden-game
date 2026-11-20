//! The archive: every game, seasons and their standings, awards, each player's record, and
//! the numbers that say whether the game is doing its job.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::Html;
use ridgewarden_client::util::{esc, now_ms, short_time};
use serde::Deserialize;

use crate::App;
use crate::archive::{Award, Record, SeasonView};
use crate::web::{Fail, STYLE, page};

fn archive_nav() -> &'static str {
    "<p><a href=\"/games\">Games</a> &middot; <a href=\"/seasons\">Seasons</a> &middot; \
     <a href=\"/awards\">Awards</a> &middot; <a href=\"/stats\">Numbers</a></p>"
}

/// The date of a moment, e.g. 2026-11-19.
fn day(ms: i64) -> String {
    short_time(ms)[..10].to_string()
}

pub async fn games_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let a = app.archive()?;
    let mut b = format!("{STYLE}<h1>Games</h1>{}", archive_nav());
    if a.games.is_empty() {
        b.push_str("<p class=mute>No games yet.</p>");
        return Ok(page(&app, "/games", "Games", &b, false).await);
    }
    b.push_str(
        "<div class=wrap><table><tr><th>Game</th><th>Started</th><th>Status</th>\
         <th class=n>Players</th><th>Winner</th><th>Season</th></tr>",
    );
    for p in a.games.iter().rev() {
        let s = &p.session;
        let status = if s.end <= now {
            format!("ended {}", short_time(s.end))
        } else {
            "running".into()
        };
        let winner = match (p.teams.first(), p.teams.get(1)) {
            _ if s.end > now => String::new(),
            (Some(a), Some(b)) if a.total == b.total && a.total > 0 => {
                format!("tie at {}", a.total)
            }
            (Some(a), _) if a.total > 0 => format!("{} ({})", a.team, a.total),
            _ => String::new(),
        };
        let season = a
            .seasons
            .iter()
            .find(|v| s.season.as_deref() == Some(v.season.id.as_str()))
            .map(|v| v.season.name.clone())
            .unwrap_or_default();
        let _ = write!(
            b,
            "<tr><td><a href=\"/?s={}\">{}</a></td><td>{}</td><td>{}</td><td class=n>{}</td>\
             <td>{}</td><td>{}</td></tr>",
            esc(&s.id),
            esc(&s.name),
            esc(&short_time(s.start)),
            esc(&status),
            p.players.len(),
            esc(&winner),
            esc(&season)
        );
    }
    b.push_str("</table></div>");
    Ok(page(&app, "/games", "Games", &b, false).await)
}

fn standings(players: &[Record]) -> String {
    if players.is_empty() {
        return "<p class=mute>Nobody has played yet.</p>".into();
    }
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Callsign</th><th class=n>Points</th><th class=n>Games</th>\
         <th class=n>Sites</th><th class=n>Foxes</th></tr>",
    );
    for r in players {
        let _ = write!(
            b,
            "<tr><td><a href=\"/player?call={c}\">{c}</a></td><td class=n><b>{}</b></td>\
             <td class=n>{}</td><td class=n>{}</td><td class=n>{}</td></tr>",
            r.points,
            r.games.len(),
            r.sites.len(),
            r.finds,
            c = esc(&r.call)
        );
    }
    b.push_str("</table></div>");
    b
}

fn awards_html(awards: &[Award]) -> String {
    let earned: Vec<&Award> = awards.iter().filter(|a| !a.earned.is_empty()).collect();
    if earned.is_empty() {
        return "<p class=mute>No awards earned yet.</p>".into();
    }
    let mut b = String::from("<ul class=plain>");
    for a in earned {
        let who: Vec<String> = a
            .earned
            .iter()
            .map(|(c, game)| format!("{} ({})", esc(c), esc(game)))
            .collect();
        let _ = write!(
            b,
            "<li><b>{}</b> <span class=\"mute small\">{}</span>: {}</li>",
            esc(&a.name),
            esc(&a.what),
            who.join(", ")
        );
    }
    b.push_str("</ul>");
    b
}

fn season_html(v: &SeasonView) -> String {
    let span = match v.season.end {
        Some(e) => format!("{} to {}", day(v.season.start), day(e)),
        None => format!("since {}, still open", day(v.season.start)),
    };
    let mut b = format!(
        "<h2>{}</h2><p class=mute>{}, {} games.</p>",
        esc(&v.season.name),
        esc(&span),
        v.games.len()
    );
    if !v.teams.is_empty() {
        b.push_str("<p>Teams, by name across the season's games: ");
        let teams: Vec<String> = v
            .teams
            .iter()
            .map(|(t, pts)| format!("{} {pts}", esc(t)))
            .collect();
        b.push_str(&teams.join(", "));
        b.push_str("</p>");
    }
    b.push_str(&standings(&v.players));
    b.push_str("<h3>Awards this season</h3>");
    b.push_str(&awards_html(&v.awards));
    b
}

pub async fn seasons_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let a = app.archive()?;
    let mut b = format!("{STYLE}<h1>Seasons</h1>{}", archive_nav());
    if a.seasons.is_empty() {
        b.push_str(
            "<p class=mute>No seasons yet. An admin opens one with \
             <code>ridgewarden-game season start NAME</code>; games started while it is open \
             count towards it.</p>",
        );
    }
    for v in &a.seasons {
        b.push_str(&season_html(v));
    }
    Ok(page(&app, "/games", "Seasons", &b, false).await)
}

pub async fn awards_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let a = app.archive()?;
    let mut b = format!(
        "{STYLE}<h1>Awards</h1>{}<p>Awards are earned by doing, across every game, the way \
         POTA's are. They are a record, not a prize.</p>",
        archive_nav()
    );
    b.push_str("<div class=wrap><table><tr><th>Award</th><th>For</th><th>Earned by</th></tr>");
    for aw in &a.awards {
        let who: Vec<String> = aw.earned.iter().map(|(c, _)| esc(c)).collect();
        let who = if who.is_empty() {
            "<span class=mute>nobody yet</span>".to_string()
        } else {
            who.join(", ")
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{who}</td></tr>",
            esc(&aw.name),
            esc(&aw.what)
        );
    }
    b.push_str("</table></div>");
    Ok(page(&app, "/games", "Awards", &b, false).await)
}

#[derive(Deserialize)]
pub struct Who {
    call: Option<String>,
}

pub async fn player_page(
    State(app): State<Arc<App>>,
    Query(q): Query<Who>,
) -> Result<Html<String>, Fail> {
    let a = app.archive()?;
    let call = q.call.unwrap_or_default().trim().to_ascii_uppercase();
    let mut b = format!("{STYLE}<h1>{}</h1>{}", esc(&call), archive_nav());
    let Some(r) = a.records.get(&call) else {
        b.push_str("<p class=mute>No games on record for this callsign.</p>");
        return Ok(page(&app, "/games", "Player", &b, false).await);
    };
    let sites: Vec<String> = r.sites.iter().map(|n| esc(&app.cfg.label(n))).collect();
    let _ = write!(
        b,
        "<div class=cards><div class=card><div class=n>{}</div>points</div>\
         <div class=card><div class=n>{}</div>games</div><div class=card><div class=n>{}</div>sites</div>\
         <div class=card><div class=n>{}</div>foxes found</div></div>\
         <p>Sites: {}</p><h2>Games</h2><div class=wrap><table><tr><th>Game</th><th>Day</th>\
         <th>Team</th><th class=n>Points</th></tr>",
        r.points,
        r.games.len(),
        r.sites.len(),
        r.finds,
        if sites.is_empty() {
            "none yet".to_string()
        } else {
            sites.join(", ")
        }
    );
    for (id, name, start, team, points) in r.games.iter().rev() {
        let _ = write!(
            b,
            "<tr><td><a href=\"/?s={}\">{}</a></td><td>{}</td><td>{}</td><td class=n>{points}</td></tr>",
            esc(id),
            esc(name),
            esc(&day(*start)),
            esc(team)
        );
    }
    b.push_str("</table></div><h2>Awards</h2><ul class=plain>");
    let mine: Vec<&Award> = a
        .awards
        .iter()
        .filter(|aw| aw.earned.iter().any(|(c, _)| *c == call))
        .collect();
    if mine.is_empty() {
        b.push_str("<li class=mute>None yet.</li>");
    }
    for aw in mine {
        let _ = write!(
            b,
            "<li><b>{}</b> <span class=\"mute small\">{}</span></li>",
            esc(&aw.name),
            esc(&aw.what)
        );
    }
    b.push_str("</ul>");
    Ok(page(&app, "/games", &call, &b, false).await)
}

pub async fn stats_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let a = app.archive()?;
    let st = &a.stats;
    let mut b = format!("{STYLE}<h1>Numbers</h1>{}", archive_nav());
    let biggest = match &st.biggest {
        Some((name, id, n)) => format!(
            "<a href=\"/?s={}\">{}</a> had the most players: {n}.",
            esc(id),
            esc(name)
        ),
        None => "No game has had players yet.".into(),
    };
    let _ = write!(
        b,
        "<div class=cards><div class=card><div class=n>{}</div>games</div>\
         <div class=card><div class=n>{}</div>different players</div>\
         <div class=card><div class=n>{}</div>games with 10 or more players</div>\
         <div class=card><div class=n>{}</div>new nodes seen</div></div><p>{biggest}</p>",
        st.games,
        st.players,
        st.ten_plus,
        st.new_nodes.len()
    );
    b.push_str(
        "<h2>What the mesh grew by</h2><p class=\"mute small\">Nodes and links that first \
         showed up after this app started watching. If the game got someone to build a node, \
         it is here.</p>",
    );
    if st.new_nodes.is_empty() && st.new_links.is_empty() {
        b.push_str("<p class=mute>Nothing new yet.</p>");
    }
    let mut items: Vec<(i64, String)> = st
        .new_nodes
        .iter()
        .map(|(n, d)| (*d, format!("node {}", esc(&app.cfg.label(n)))))
        .collect();
    items.extend(st.new_links.iter().map(|((x, y), d)| {
        (
            *d,
            format!(
                "link {} &ndash; {}",
                esc(&app.cfg.label(x)),
                esc(&app.cfg.label(y))
            ),
        )
    }));
    items.sort();
    b.push_str("<ul class=plain>");
    for (d, what) in items.iter().rev().take(50) {
        let _ = write!(
            b,
            "<li><span class=\"mute small\">{}</span> {what}</li>",
            esc(&day(d * crate::game::DAY))
        );
    }
    b.push_str("</ul>");
    Ok(page(&app, "/games", "Numbers", &b, false).await)
}
