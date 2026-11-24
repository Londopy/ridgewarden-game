//! Pages for spectators: the live map and leaderboard, the game's debrief, the rules,
//! and `/api/game`. Player forms are in `forms`, the archive in `history`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::connect_info::Connected;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::serve::IncomingStream;
use axum::{Json, Router};
use ridgewarden_client::graph::Edge;
use ridgewarden_client::ui::{Color, Page, pill};
use ridgewarden_client::util::{esc, human_duration, now_ms, short_time};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;

use crate::App;
use crate::forms;
use crate::game::{Rules, Session, Target, current};
use crate::history;
use crate::score::{Played, chain_rate};
use crate::world::Pos;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(game_page))
        .route(
            "/checkin",
            get(forms::checkin_page).post(forms::checkin_post),
        )
        .route("/word", get(forms::word_page).post(forms::word_post))
        .route("/hunt", get(forms::hunt_page).post(forms::hunt_post))
        .route("/spot", get(forms::spot_page).post(forms::spot_post))
        .route("/player.js", get(forms::player_js))
        .route("/key", get(forms::key_of))
        .route("/rules", get(rules_page))
        .route("/games", get(history::games_page))
        .route("/seasons", get(history::seasons_page))
        .route("/awards", get(history::awards_page))
        .route("/player", get(history::player_page))
        .route("/stats", get(history::stats_page))
        .route("/api/game", get(game_json))
        .route("/api/games", get(games_json))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

/// Both ends of a connection: who is asking, and which of our addresses they reached.
#[derive(Clone, Debug)]
pub struct Peer {
    pub remote: SocketAddr,
    pub local: Option<SocketAddr>,
}

impl Connected<IncomingStream<'_, TcpListener>> for Peer {
    fn connect_info(s: IncomingStream<'_, TcpListener>) -> Self {
        Peer {
            remote: *s.remote_addr(),
            local: s.io().local_addr().ok(),
        }
    }
}

pub struct Fail(pub anyhow::Error);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.into())
    }
}

pub async fn page(app: &App, active: &str, title: &str, body: &str, refresh: bool) -> Html<String> {
    let who = match app.me().await {
        Some(w) if !w.node.is_empty() => format!("{} / {}", w.node, w.call),
        Some(w) => w.call,
        None => "core not reachable".into(),
    };
    let page = Page {
        app: "ridgewarden-game",
        nav: &[
            ("/", "Game"),
            ("/checkin", "Check in"),
            ("/word", "Code word"),
            ("/hunt", "Hunt"),
            ("/spot", "Spot"),
            ("/rules", "Rules"),
            ("/games", "Archive"),
        ],
        active,
        who: &who,
        refresh,
    };
    Html(page.render(title, body))
}

pub const STYLE: &str = "<style>
.map{width:100%;height:auto;display:block;border:1px solid var(--line);border-radius:6px;background:var(--card);margin:8px 0}
.map .up{stroke:var(--fg);stroke-width:1.5;opacity:.5}.map .down{stroke:var(--mute);stroke-width:1.2;stroke-dasharray:5 4}
.map .chain{stroke-width:5}.map .relay{fill:var(--mute)}.map .free{fill:var(--bg);stroke:var(--fg);stroke-width:2}
.map .held{stroke:var(--fg);stroke-width:1.5}.map .bearing{stroke:var(--fg);stroke-width:1.2;stroke-dasharray:2 3}
.map .fox{fill:var(--yellow);stroke:var(--fg);stroke-width:1.5}
.map text{fill:var(--fg);font-size:13px;paint-order:stroke;stroke:var(--card);stroke-width:3px}
.sw{display:inline-block;width:.8em;height:.8em;border-radius:2px;margin-right:.35em;vertical-align:-.05em}
td.n,th.n{text-align:right}
form.checkin label{display:block;margin:10px 0}form.checkin input,form.checkin select{font:inherit;padding:6px 8px;min-width:12em}
form.checkin button{font:inherit;padding:8px 16px;margin-top:6px}form.checkin [hidden]{display:none}
.banner,code{overflow-wrap:anywhere}
</style>";

/// Swaps in a fresh copy of the page every 15 s; without it the page still reloads
/// itself every minute.
const LIVE: &str = "<script>setInterval(function(){fetch(location.href).then(function(r){return r.ok?r.text():null}).then(function(t){if(!t)return;var m=new DOMParser().parseFromString(t,'text/html').querySelector('main');if(m)document.querySelector('main').replaceWith(m)}).catch(function(){})},15000)</script>";

pub const PALETTE: [(&str, &str); 8] = [
    ("red", "#c0392b"),
    ("blue", "#2a7ab8"),
    ("green", "#2e8b3d"),
    ("orange", "#d4881c"),
    ("teal", "#0f8f8f"),
    ("magenta", "#b5338a"),
    ("brown", "#7a5c2e"),
    ("gray", "#6b7a8f"),
];

/// A color per team: its own if it's named after one, else the next free one.
pub fn team_colors(teams: &[String]) -> BTreeMap<String, &'static str> {
    let mut out = BTreeMap::new();
    let mut used = BTreeSet::new();
    for t in teams {
        if let Some((i, (_, hex))) = PALETTE
            .iter()
            .enumerate()
            .find(|(_, (name, _))| name.eq_ignore_ascii_case(t))
        {
            out.insert(t.clone(), *hex);
            used.insert(i);
        }
    }
    for t in teams {
        if !out.contains_key(t) {
            let i = (0..PALETTE.len()).find(|i| !used.contains(i)).unwrap_or(0);
            used.insert(i);
            out.insert(t.clone(), PALETTE[i].1);
        }
    }
    out
}

pub fn swatch(colors: &BTreeMap<String, &str>, team: &str) -> String {
    format!(
        "<span class=sw style=\"background:{}\"></span>{}",
        colors.get(team).copied().unwrap_or("transparent"),
        esc(team)
    )
}

#[derive(Deserialize)]
pub struct Pick {
    s: Option<String>,
}

/// The game asked for (by id or a prefix of at least 8), else the one running, else the latest.
fn pick<'a>(sessions: &'a [Session], id: Option<&str>, now: i64) -> Option<&'a Session> {
    match id.filter(|i| i.len() >= 8) {
        Some(i) => sessions.iter().find(|s| s.id.starts_with(i)),
        None => current(sessions, now).or(sessions.first()),
    }
}

pub fn status_line(s: &Session, now: i64) -> String {
    if s.end <= now {
        format!(
            "{} Ran {} to {} ({}). Final scores.",
            pill(Color::Unknown, "OVER"),
            esc(&short_time(s.start)),
            esc(&short_time(s.end)),
            human_duration(s.end - s.start)
        )
    } else {
        format!(
            "{} Started {}, ends {} ({} left) unless an admin ends it sooner.",
            pill(Color::Green, "LIVE"),
            esc(&short_time(s.start)),
            esc(&short_time(s.end)),
            human_duration(s.end - now)
        )
    }
}

/// "Cuesta Peak", or "fox Fox 1".
pub fn target_label(app: &App, t: &Target) -> String {
    match t {
        Target::Site(n) => app.cfg.label(n),
        Target::Fox(f) => format!("fox {f}"),
    }
}

fn scoreboard(p: &Played, colors: &BTreeMap<String, &str>) -> String {
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Team</th><th class=n>Points</th><th class=n>Chains</th>\
         <th class=n>New links</th><th class=n>Captures</th><th class=n>Encounters</th>\
         <th class=n>Foxes</th><th class=n>Now</th><th class=n>Sites held</th></tr>",
    );
    for t in &p.teams {
        let _ = write!(
            b,
            "<tr><td>{}</td><td class=n><b>{}</b></td><td class=n>{}</td><td class=n>{}</td>\
             <td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}/min</td>\
             <td class=n>{}</td></tr>",
            swatch(colors, &t.team),
            t.total,
            t.chain,
            t.builder,
            t.capture,
            t.encounter,
            t.fox,
            t.rate,
            t.held
        );
    }
    b.push_str("</table></div>");
    b
}

fn chains_html(app: &App, p: &Played, colors: &BTreeMap<String, &str>) -> String {
    if p.chains.is_empty() {
        return "<p class=mute>No chains right now. A chain is two or more of a team's sites \
                joined by links that are up.</p>"
            .into();
    }
    let mut b = String::from("<ul class=plain>");
    for c in &p.chains {
        let names: Vec<String> = c.sites.iter().map(|n| esc(&app.cfg.label(n))).collect();
        let (chain, builder) = chain_rate(&p.session.rules, c);
        let rate = chain + builder;
        let _ = write!(
            b,
            "<li>{}: {} <span class=\"mute small\">({} sites, {}, {rate} points a minute{})</span></li>",
            swatch(colors, &c.team),
            names.join(" &ndash; "),
            c.sites.len(),
            match c.links.len() {
                1 => "1 link".to_string(),
                n => format!("{n} links"),
            },
            match c.new_links.len() {
                0 => String::new(),
                1 => ", 1 new link".into(),
                n => format!(", {n} new links"),
            }
        );
    }
    b.push_str("</ul>");
    b
}

const MAP_W: f64 = 800.0;
const MAP_MARGIN: f64 = 50.0;
/// How far a bearing line reaches on the map.
const BEARING_KM: f64 = 15.0;

/// A flat projection fitted to a set of points; fine at the size of a mesh.
struct Proj {
    k: f64,
    x0: f64,
    y0: f64,
    scale: f64,
    ox: f64,
    oy: f64,
    h: f64,
}

impl Proj {
    fn fit(points: &[Pos]) -> Self {
        let (w, m) = (MAP_W, MAP_MARGIN);
        let lats = points.iter().map(|p| p.lat);
        let mid = (lats.clone().fold(f64::MAX, f64::min) + lats.fold(f64::MIN, f64::max)) / 2.0;
        let k = mid.to_radians().cos();
        let span = |f: &dyn Fn(&Pos) -> f64| {
            points
                .iter()
                .map(f)
                .fold((f64::MAX, f64::MIN), |a, v| (a.0.min(v), a.1.max(v)))
        };
        let ((x0, x1), (y0, y1)) = (span(&|p| p.lon * k), span(&|p| -p.lat));
        let (dx, dy) = ((x1 - x0).max(1e-9), (y1 - y0).max(1e-9));
        // as tall as the area's shape wants, within reason
        let h = ((w - 2.0 * m) * dy / dx + 2.0 * m).clamp(320.0, 640.0);
        let scale = ((w - 2.0 * m) / dx).min((h - 2.0 * m) / dy);
        Proj {
            k,
            x0,
            y0,
            scale,
            ox: (w - (x1 - x0) * scale) / 2.0,
            oy: (h - (y1 - y0) * scale) / 2.0,
            h,
        }
    }

    fn xy(&self, p: Pos) -> (f64, f64) {
        (
            self.ox + (p.lon * self.k - self.x0) * self.scale,
            self.oy + (-p.lat - self.y0) * self.scale,
        )
    }
}

fn map_links(app: &App, p: &Played, colors: &BTreeMap<String, &str>, pr: &Proj) -> String {
    let chain_of: BTreeMap<&Edge, &String> = p
        .chains
        .iter()
        .flat_map(|c| c.links.iter().map(move |e| (e, &c.team)))
        .collect();
    let mut b = String::new();
    for e in &p.known {
        let (Some(a), Some(z)) = (p.positions.get(&e.0), p.positions.get(&e.1)) else {
            continue;
        };
        let ((x1, y1), (x2, y2)) = (pr.xy(*a), pr.xy(*z));
        let (class, style) = match chain_of.get(e) {
            Some(team) => (
                "chain",
                format!(
                    " style=\"stroke:{}\"",
                    colors.get(*team).copied().unwrap_or("")
                ),
            ),
            None if p.up.contains(e) => ("up", String::new()),
            None => ("down", String::new()),
        };
        let _ = write!(
            b,
            "<line class={class}{style} x1=\"{x1:.1}\" y1=\"{y1:.1}\" x2=\"{x2:.1}\" y2=\"{y2:.1}\"><title>{} &ndash; {}</title></line>",
            esc(&app.cfg.label(&e.0)),
            esc(&app.cfg.label(&e.1))
        );
    }
    b
}

fn map_nodes(
    app: &App,
    p: &Played,
    colors: &BTreeMap<String, &str>,
    pr: &Proj,
    nodes: &BTreeSet<&String>,
) -> String {
    let sites = p.session.all_sites();
    let mut b = String::new();
    for n in nodes {
        let Some(pos) = p.positions.get(*n) else {
            continue;
        };
        let (x, y) = pr.xy(*pos);
        let label = esc(&app.cfg.label(n));
        if !sites.contains(n) {
            let _ = write!(
                b,
                "<circle class=relay cx=\"{x:.1}\" cy=\"{y:.1}\" r=3><title>{label} (relay)</title></circle>"
            );
            continue;
        }
        let (circle, text) = match p.holders.get(*n) {
            Some(t) => (
                format!(
                    "class=held style=\"fill:{}\"",
                    colors.get(t).copied().unwrap_or("")
                ),
                format!("{label} ({})", esc(t)),
            ),
            None => ("class=free".to_string(), label),
        };
        let _ = write!(
            b,
            "<circle {circle} cx=\"{x:.1}\" cy=\"{y:.1}\" r=9><title>{text}</title></circle>\
             <text x=\"{:.1}\" y=\"{:.1}\">{text}</text>",
            x + 12.0,
            y + 4.0
        );
    }
    b
}

/// Bearings as dotted lines, and foxes where an admin said they were once the game ended.
fn map_hunt(p: &Played, pr: &Proj) -> String {
    let mut b = String::new();
    for x in &p.bearings {
        let ((x1, y1), (x2, y2)) = (pr.xy(x.from), pr.xy(x.from.toward(x.deg, BEARING_KM)));
        let _ = write!(
            b,
            "<line class=bearing x1=\"{x1:.1}\" y1=\"{y1:.1}\" x2=\"{x2:.1}\" y2=\"{y2:.1}\"><title>{} on {}: {:.0}&deg;</title></line>",
            esc(&x.call),
            esc(&x.fox),
            x.deg
        );
    }
    for f in &p.session.foxes {
        let Some(at) = f.at else {
            continue;
        };
        let (x, y) = pr.xy(at);
        let _ = write!(
            b,
            "<rect class=fox x=\"{:.1}\" y=\"{:.1}\" width=12 height=12 transform=\"rotate(45 {x:.1} {y:.1})\">\
             <title>{name}</title></rect><text x=\"{:.1}\" y=\"{:.1}\">{name}</text>",
            x - 6.0,
            y - 6.0,
            x + 12.0,
            y + 4.0,
            name = esc(&f.name)
        );
    }
    b
}

/// The map: sites and relays at their reported locations, links up or down, each team's
/// chains in its color, and the hunt. Every color has a text label too.
fn map(app: &App, p: &Played, colors: &BTreeMap<String, &str>) -> String {
    let sites = p.session.all_sites();
    let nodes: BTreeSet<&String> = sites
        .iter()
        .chain(p.known.iter().flat_map(|(a, b)| [a, b]))
        .filter(|n| p.positions.contains_key(*n))
        .collect();
    if !sites.iter().any(|n| nodes.contains(n)) {
        return "<p class=mute>No game site has reported a location yet, so there is no map. \
                Set each node's location on its setup page.</p>"
            .into();
    }
    let mut points: Vec<Pos> = nodes.iter().map(|n| p.positions[*n]).collect();
    points.extend(p.bearings.iter().map(|x| x.from));
    points.extend(p.session.foxes.iter().filter_map(|f| f.at));
    let pr = Proj::fit(&points);
    let mut b = format!(
        "<svg class=map viewBox=\"0 0 {MAP_W} {:.0}\" role=img aria-label=\"Map of the game's sites and links\">{}{}{}</svg>",
        pr.h,
        map_links(app, p, colors, &pr),
        map_hunt(p, &pr),
        map_nodes(app, p, colors, &pr, &nodes)
    );
    b.push_str(
        "<p class=\"mute small\">Big dots are game sites, filled in the holding team's color. \
         Small dots are relays. Thick colored lines are chains, thin lines are links that are \
         up, dashed lines are links that are down or have no fresh data. Dotted lines are \
         bearings on foxes, and diamonds are foxes, shown once the game is over.",
    );
    let unplaced: Vec<String> = sites
        .iter()
        .filter(|n| !nodes.contains(n))
        .map(|n| esc(&app.cfg.label(n)))
        .collect();
    if !unplaced.is_empty() {
        let _ = write!(
            b,
            " Not on the map (no location reported): {}.",
            unplaced.join(", ")
        );
    }
    b.push_str("</p>");
    b
}

fn sites_table(app: &App, p: &Played, colors: &BTreeMap<String, &str>) -> String {
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Site</th><th>Taken by</th><th>Held by</th></tr>",
    );
    for n in p.session.all_sites() {
        let how = match (
            p.session.sites.contains(&n),
            p.session.word_sites.contains(&n),
        ) {
            (true, true) => "LAN check-in or code word",
            (true, false) => "LAN check-in",
            _ => "code word heard on air",
        };
        let held = match p.holders.get(&n) {
            Some(t) => swatch(colors, t),
            None if p.dead_sites.contains(&n) => {
                "<span class=mute>nobody; no core polls this node over its LAN</span>".into()
            }
            None if p.lost.iter().any(|l| l.site == n) => {
                "<span class=mute>nobody; lost when its links stayed down</span>".into()
            }
            None => "<span class=mute>nobody yet</span>".into(),
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{how}</td><td>{held}</td></tr>",
            esc(&app.cfg.label(&n))
        );
    }
    b.push_str("</table></div>");
    b
}

fn players_table(p: &Played, colors: &BTreeMap<String, &str>) -> String {
    if p.players.is_empty() {
        return "<p class=mute>Nobody has checked in yet.</p>".into();
    }
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Callsign</th><th>Team</th><th class=n>Points</th>\
         <th class=n>Captures</th><th class=n>Encounters</th><th class=n>Foxes</th><th>Notes</th></tr>",
    );
    for pl in &p.players {
        let note = match pl.strikes {
            0 => String::new(),
            n if n >= p.session.rules.strikes => "out: too many impossible hops".into(),
            n => format!("{n} impossible hop{}", if n == 1 { "" } else { "s" }),
        };
        let team = if pl.team.is_empty() {
            "<span class=mute>runs a fox</span>".into()
        } else {
            swatch(colors, &pl.team)
        };
        let _ = write!(
            b,
            "<tr><td><a href=\"/player?call={}\">{}</a></td><td>{team}</td><td class=n>{}</td>\
             <td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td>{}</td></tr>",
            esc(&pl.call),
            esc(&pl.call),
            pl.points,
            pl.captures,
            pl.encounters,
            pl.finds,
            esc(&note)
        );
    }
    b.push_str("</table></div>");
    b
}

fn checkins_table(app: &App, p: &Played, colors: &BTreeMap<String, &str>) -> String {
    if p.checkins.is_empty() {
        return "<p class=mute>No check-ins yet.</p>".into();
    }
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Time</th><th>Callsign</th><th>Team</th><th>Where</th>\
         <th>Result</th></tr>",
    );
    for c in p.checkins.iter().rev().take(40) {
        let result = match (c.counted, c.pending) {
            (true, _) => format!(
                "{} {}",
                pill(Color::Green, &format!("+{}", c.points)),
                esc(&c.note)
            ),
            (false, true) => format!("{} {}", pill(Color::Unknown, "waiting"), esc(&c.note)),
            (false, false) => format!("{} {}", pill(Color::Yellow, "not counted"), esc(&c.note)),
        };
        let how = if c.by_word { " (code word)" } else { "" };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}{how}</td><td>{result}</td></tr>",
            esc(&short_time(c.ts)[11..]),
            esc(&c.call),
            swatch(colors, &c.team),
            esc(&target_label(app, &c.target))
        );
    }
    b.push_str("</table></div>");
    b
}

fn foxes_html(p: &Played) -> String {
    if p.session.foxes.is_empty() {
        return String::new();
    }
    let mut b = String::from(
        "<h2>Foxes</h2><div class=wrap><table><tr><th>Fox</th><th>Run by</th>\
         <th class=n>Found by</th><th class=n>Bearings</th><th>Where it was</th></tr>",
    );
    for f in &p.session.foxes {
        let finders = p
            .checkins
            .iter()
            .filter(|c| c.counted && c.target == Target::Fox(f.name.clone()))
            .count();
        let bearings = p.bearings.iter().filter(|x| x.fox == f.name).count();
        let at = match f.at {
            Some(a) => format!("{:.4}, {:.4}", a.lat, a.lon),
            None => "<span class=mute>shown once an admin ends the game</span>".into(),
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td class=n>{finders}</td><td class=n>{bearings}</td><td>{at}</td></tr>",
            esc(&f.name),
            esc(f.call.as_deref().unwrap_or("")),
        );
    }
    b.push_str("</table></div><p><a href=\"/hunt\">Share a bearing</a></p>");
    b
}

pub fn spots_list(p: &Played) -> String {
    if p.spots.is_empty() {
        return "<p class=mute>No spots yet.</p>".into();
    }
    let mut b = String::from("<ul class=plain>");
    for x in p.spots.iter().rev().take(20) {
        let _ = write!(
            b,
            "<li><span class=\"mute small\">{}</span> <b>{}</b>: {}</li>",
            esc(&short_time(x.ts)[11..]),
            esc(&x.call),
            esc(&x.text)
        );
    }
    b.push_str("</ul>");
    b
}

/// Sites that went back to nobody because every link at them stayed down.
fn lost_html(app: &App, p: &Played) -> String {
    if p.lost.is_empty() {
        return String::new();
    }
    let mut b = format!(
        "<p>Sites lost after {} minutes with every link down:</p><ul class=plain>",
        p.session.rules.hold_min
    );
    for l in &p.lost {
        let _ = write!(
            b,
            "<li><span class=\"mute small\">{}</span> {}, lost by {}</li>",
            esc(&short_time(l.ts)[11..]),
            esc(&app.cfg.label(&l.site)),
            esc(&l.team)
        );
    }
    b.push_str("</ul>");
    b
}

/// What the network did during the game, for the debrief: every link that dropped, for
/// how long, and whose chains it broke.
fn debrief(app: &App, p: &Played) -> String {
    if p.drops.is_empty() {
        return "<p class=mute>No link dropped during the game.</p>".into();
    }
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Link</th><th>Down at</th><th>For</th><th>Broke chains of</th></tr>",
    );
    for d in &p.drops {
        let length = match d.to {
            Some(t) => human_duration(t - d.from),
            None => "the rest of the game".into(),
        };
        let _ = write!(
            b,
            "<tr><td>{} &ndash; {}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(&app.cfg.label(&d.link.0)),
            esc(&app.cfg.label(&d.link.1)),
            esc(&short_time(d.from)[11..]),
            esc(&length),
            esc(&d.broke.join(", "))
        );
    }
    b.push_str("</table></div>");
    b
}

fn banners(app: &App, p: &Played) -> String {
    let mut b = String::new();
    if p.links_pruned {
        b.push_str(
            "<div class=banner>This site has no link data from the start of this game, so \
             chain points from before its first poll are missing here.</div>",
        );
    }
    if !p.dead_sites.is_empty() {
        let names: Vec<String> = p
            .dead_sites
            .iter()
            .map(|n| esc(&app.cfg.label(n)))
            .collect();
        let _ = write!(
            b,
            "<div class=banner>No core polls {} over its LAN, so nobody can check in there.</div>",
            names.join(", ")
        );
    }
    b
}

async fn game_page(
    State(app): State<Arc<App>>,
    Query(q): Query<Pick>,
) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let sessions = app.sessions()?;
    let mut b = String::from(STYLE);
    if app.cfg.admins.is_empty() {
        b.push_str(
            "<div class=banner>No admins are set in this site's game.toml, so no game started \
             anywhere shows here. Add the admins' public keys to <code>admins</code>.</div>",
        );
    }
    let Some(s) = pick(&sessions, q.s.as_deref(), now) else {
        b.push_str(
            "<h1>No game yet</h1><p>When an admin starts a game, the map and scores show here. \
             Capture the Site is a training exercise: teams visit sites on the mesh in person, \
             check in on the site's own network, and score while the radio links between their \
             sites stay up.</p><p><a href=\"/rules\">How scoring works</a></p>",
        );
        return Ok(page(&app, "/", "Game", &b, true).await);
    };
    let p = app.played(s)?;
    let colors = team_colors(&s.teams);
    let live = s.running(now);
    let _ = write!(b, "<h1>{}</h1><p>{}</p>", esc(&s.name), status_line(s, now));
    b.push_str(&banners(&app, &p));
    b.push_str("<h2>Scores</h2>");
    b.push_str(&scoreboard(&p, &colors));
    b.push_str(if live {
        "<h2>Map now</h2>"
    } else {
        "<h2>Map at the end</h2>"
    });
    b.push_str(&map(&app, &p, &colors));
    b.push_str("<h2>Chains</h2>");
    b.push_str(&chains_html(&app, &p, &colors));
    b.push_str("<h2>Sites</h2>");
    b.push_str(&sites_table(&app, &p, &colors));
    b.push_str(&foxes_html(&p));
    b.push_str("<h2>Players</h2>");
    b.push_str(&players_table(&p, &colors));
    b.push_str("<h2>Check-ins and code words</h2>");
    b.push_str(&checkins_table(&app, &p, &colors));
    b.push_str("<h2>Spots</h2>");
    b.push_str(&spots_list(&p));
    b.push_str("<h2>What the network did</h2>");
    b.push_str(&debrief(&app, &p));
    b.push_str(&lost_html(&app, &p));
    let _ = write!(
        b,
        "<p class=\"mute small\">Worked out at {} from the signed game log and the mesh's own \
         link data. <a href=\"/rules\">How scoring works</a>.</p>",
        esc(&short_time(p.at))
    );
    if live {
        b.push_str(LIVE);
    }
    Ok(page(&app, "/", &s.name, &b, live).await)
}

async fn game_json(State(app): State<Arc<App>>, Query(q): Query<Pick>) -> Result<Response, Fail> {
    let sessions = app.sessions()?;
    Ok(match pick(&sessions, q.s.as_deref(), now_ms()) {
        Some(s) => Json(app.played(s)?.as_ref().clone()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "no game"}))).into_response(),
    })
}

/// A game as `/api/games` lists it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Listed {
    pub id: String,
    pub name: String,
    pub start: i64,
    /// when it ended, or is planned to end
    pub end: i64,
    pub ended: bool,
}

/// Every game, newest first; an empty list before the first one.
async fn games_json(State(app): State<Arc<App>>) -> Result<Json<Vec<Listed>>, Fail> {
    let games = app.sessions()?.into_iter().map(|s| Listed {
        id: s.id,
        name: s.name,
        start: s.start,
        end: s.end,
        ended: s.ended,
    });
    Ok(Json(games.collect()))
}

/// The rules in words, with the numbers a game uses.
pub fn rules_html(r: &Rules) -> String {
    let types = r.link_types.join(", ");
    format!(
        "<ol>\
<li><b>Players.</b> Every post crosses the mesh's radio links, so players are licensed \
operators, each under their own callsign. Spectators only watch. Each player has their own \
key, made in the browser from their callsign and a passphrase that never leaves the page, \
and signs every check-in, code word, bearing and spot with it. A callsign's first check-in on \
a site's LAN sets its key for every game after (in a game with a roster, its first post does), \
unless an admin sets another; until then any player key can post under it.</li>\
<li><b>Check in.</b> Go to a check-in site and open its game page on a phone or laptop \
connected to that node's own LAN. Enter your callsign, passphrase and team. The site's core \
countersigns the check-in, so it can't be made from anywhere else. You stay on the team of \
your first check-in for the whole game, unless an admin put you on a team. Once anyone is on \
the roster, only rostered callsigns can play.</li>\
<li><b>Public sites.</b> At a public site you can't reach the node's LAN, so someone there \
sends a code word on air that changes every {wm} minutes. Type what you heard on the code \
word page of any site. One try per word; it counts once the word is revealed, a few minutes \
after it changes, if it was right and sent before the reveal.</li>\
<li><b>Capture.</b> A check-in at a site your team doesn't hold captures it and earns \
{cap} points. Each time your team captures the same site again, it is worth {f}&times; the \
last time, rounded down, so spread out instead of trading one site back and forth. Checking \
in at a site your team already holds earns nothing.</li>\
<li><b>Chains.</b> Every minute, each chain earns {chain} point(s) per site in it. A chain \
is two or more of your team's sites joined by {types} links that are up in the mesh's own \
link data, either directly or through relay nodes that aren't game sites. A site held by \
nobody or by another team breaks the chain. A lone site earns nothing.</li>\
<li><b>Dropped links.</b> When a link drops (two polls in a row without it, or no fresh \
data for {fresh} min), every chain through it breaks at once and its points stop until it is \
back.</li>\
<li><b>Holding a site.</b> A site stays yours only while it has a link up. Once every link \
at it has been down for {hold} minutes, it goes back to nobody and has to be captured again. \
Keeping the network healthy is how you win.</li>\
<li><b>Builder bonus.</b> A new link in a chain earns {bp} extra point(s) a minute. A link \
is new for {bw} days after it first shows up, if one of its ends was polled on an earlier \
day without it, and it only earns the bonus once it has shown up in link data on {bd} \
different days, so it can't be faked for a day.</li>\
<li><b>Encounters.</b> When players from two teams check in at the same site within {em} \
minutes of each other, both get {ep} points. Each callsign can earn this once per site.</li>\
<li><b>Foxes.</b> A fox is a hidden transmitter with one-time codes left with it. When you \
find it, take a code and send it on the code word page of any site. Each code counts once, \
for whoever sends it first, so passing one on gives your find away. Finding a fox earns {fp} \
points, once per fox, and whoever runs it gets {ep} for each finder. Bearings shared on the \
hunt page show on the map for everyone.</li>\
<li><b>Travel time.</b> Two check-ins by one callsign at sites closer in time than their \
distance allows at {speed} km/h don't count. The first is a warning; after {strikes} the \
player is out of the game.</li>\
</ol>",
        wm = r.word_min,
        cap = r.capture_points,
        f = r.repeat_factor,
        chain = r.chain_points,
        fresh = r.fresh_s / 60,
        hold = r.hold_min,
        bp = r.builder_points,
        bw = r.builder_window_days,
        bd = r.builder_days,
        em = r.encounter_min,
        ep = r.encounter_points,
        fp = r.fox_points,
        speed = r.max_speed_kmh,
        strikes = r.strikes,
    )
}

async fn rules_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let sessions = app.sessions()?;
    let mut b = String::from(STYLE);
    b.push_str("<h1>How Capture the Site works</h1>");
    match current(&sessions, now) {
        Some(s) => {
            let _ = write!(
                b,
                "<p>The rules of <b>{}</b>, the game running now:</p>",
                esc(&s.name)
            );
            b.push_str(&rules_html(&s.rules));
        }
        None => {
            b.push_str(
                "<p>No game is running. A game started from this site would use these rules:</p>",
            );
            b.push_str(&rules_html(&app.cfg.rules));
        }
    }
    b.push_str(
        "<h2>What it's for</h2><p>The game is training and network familiarisation: finding \
         the sites, learning which links hold the mesh together, and seeing what a dropped link \
         does. Awards are for the record only; there are no prizes. Spectators only read the \
         signed game log and never transmit. Everything is signed and nothing is encrypted.</p>",
    );
    Ok(page(&app, "/rules", "Rules", &b, false).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EXAMPLE};
    use crate::game::MIN;
    use crate::testing::{G, T0};

    #[test]
    fn map_and_scores_show_chains_holders_and_escaped_labels() {
        let mut g = G::new();
        g.polls(T0 - 10 * MIN, T0 + 10 * MIN, "a", &["r"]);
        g.polls(T0 - 10 * MIN, T0 + 10 * MIN, "r", &["b"]);
        let s = g.start(T0, json!({}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + MIN, &s, "N0BBB", "Red", "b");
        let p = g.played(T0 + 5 * MIN);

        let dir = tempfile::tempdir().unwrap();
        let mut cfg: Config = toml::from_str(EXAMPLE).unwrap();
        cfg.data_dir = dir.path().into();
        cfg.site_names.insert("a".into(), "<Hill>".into());
        let app = App::new(cfg, false).unwrap();
        let colors = team_colors(&p.session.teams);

        let m = map(&app, &p, &colors);
        assert!(m.contains("&lt;Hill&gt; (Red)"), "{m}");
        assert!(!m.contains("<Hill>"));
        assert_eq!(m.matches("class=chain style=\"stroke:#c0392b\"").count(), 2);
        assert!(m.contains("<title>r (relay)</title>"));

        let board = scoreboard(&p, &colors);
        assert!(
            board.contains("<b>30</b>"),
            "20 for captures, 5 minutes of a 2-site chain"
        );
        assert!(board.contains("2/min"));
    }

    #[test]
    fn a_chain_of_one_link_says_link() {
        let mut g = G::new();
        g.polls(T0 - 10 * MIN, T0 + 10 * MIN, "a", &["b"]);
        let s = g.start(T0, json!({}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + MIN, &s, "N0BBB", "Red", "b");
        let p = g.played(T0 + 5 * MIN);
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: Config = toml::from_str(EXAMPLE).unwrap();
        cfg.data_dir = dir.path().into();
        let app = App::new(cfg, false).unwrap();
        let html = chains_html(&app, &p, &team_colors(&p.session.teams));
        assert!(
            html.contains("(2 sites, 1 link, 2 points a minute)"),
            "{html}"
        );
    }

    #[test]
    fn teams_named_for_colors_get_them() {
        let c = team_colors(&["Blue".into(), "Hilltoppers".into(), "Red".into()]);
        assert_eq!(c["Blue"], "#2a7ab8");
        assert_eq!(c["Red"], "#c0392b");
        assert_eq!(c["Hilltoppers"], "#2e8b3d");
    }
}
