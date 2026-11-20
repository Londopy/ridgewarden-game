//! The pages players post from: check-ins on a site's LAN, code words heard on air,
//! bearings on a fox, and spots. Every post goes through this site's core, which signs it.

use std::fmt::Write as _;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Form;
use axum::extract::{ConnectInfo, State};
use axum::http::header::{HOST, ORIGIN};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use ridgewarden_client::event::valid_call;
use ridgewarden_client::util::{esc, now_ms, short_time};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::App;
use crate::config::Net;
use crate::game::{self, Session, TEXT_MAX, Target, current};
use crate::score::Played;
use crate::web::{Fail, Peer, STYLE, page, status_line};
use crate::words;
use crate::world::Pos;

/// One post per address this often.
const POST_GAP: Duration = Duration::from_secs(10);

/// A request from this box itself, e.g. from someone logged in to it from afar.
fn from_self(peer: &Peer) -> bool {
    peer.local
        .is_some_and(|l| l.ip().to_canonical() == peer.remote.ip().to_canonical())
}

/// Whether a device may check in here: it must be on one of the node's LAN networks, and
/// not this box itself.
pub fn on_lan(lan: &[Net], peer: &Peer) -> bool {
    !from_self(peer) && lan.iter().any(|n| n.contains(peer.remote.ip()))
}

/// Why nobody on this device can check in here right now, or None if they can.
fn gate(app: &App, here: &str, s: Option<&Session>, peer: &Peer) -> Option<String> {
    if here.is_empty() {
        return Some("This site's core isn't answering or hasn't polled its node yet, so it can't say which site this is. Try again in a minute.".into());
    }
    let label = esc(&app.cfg.label(here));
    let Some(s) = s else {
        return Some("No game is running right now.".into());
    };
    if !s.sites.iter().any(|n| n == here) {
        let names: Vec<String> = s.sites.iter().map(|n| esc(&app.cfg.label(n))).collect();
        let others = if names.is_empty() {
            "This game has no LAN check-in sites; use the code word page.".to_string()
        } else {
            format!("The check-in sites are {}.", names.join(", "))
        };
        return Some(format!(
            "{label} isn't a check-in site in {}. {others}",
            esc(&s.name)
        ));
    }
    if app.lan.is_empty() || !app.signs {
        return Some(format!(
            "{label} has no LAN or no core token set in its game.toml, so it can't take check-ins."
        ));
    }
    if from_self(peer) {
        return Some(
            "This request comes from the game's own box, which anyone logged in to it could \
             send from afar. Check in from your own phone or laptop on the node's LAN."
                .into(),
        );
    }
    if !on_lan(&app.lan, peer) {
        return Some(format!(
            "Your device ({}) isn't on {label}'s LAN. Check in from a phone or laptop connected \
             to this node's own LAN, by cable or its LAN Wi-Fi, not across the mesh.",
            esc(&peer.remote.ip().to_canonical().to_string())
        ));
    }
    None
}

/// Why nobody can post code words, bearings or spots here right now.
fn open_gate(app: &App, s: Option<&Session>) -> Option<String> {
    if s.is_none() {
        return Some("No game is running right now.".into());
    }
    if !app.signs {
        return Some(
            "This site's game app has no core token, so it can't sign anything. Use another \
             site's page."
                .into(),
        );
    }
    None
}

/// A post must come straight from a browser showing this site's own page. A form on
/// another site carries that site's Origin; a page that rebinds its own DNS name to this
/// box shows up under that name in Host; a proxy adds forwarding headers, and would make
/// every device behind it look like it's on the LAN.
pub fn direct_from_our_page(h: &HeaderMap) -> bool {
    let header = |name| h.get(name).and_then(|v: &HeaderValue| v.to_str().ok());
    let Some(host) = header(HOST) else {
        return false;
    };
    let name = match host.rsplit_once(':') {
        Some((n, port)) if port.bytes().all(|b| b.is_ascii_digit()) => n,
        _ => host,
    }
    .to_ascii_lowercase();
    let local_name = name.starts_with('[')
        || name.parse::<IpAddr>().is_ok()
        || name.ends_with(".local.mesh")
        || !name.contains('.');
    let proxied = ["forwarded", "x-forwarded-for", "via"]
        .iter()
        .any(|n| h.contains_key(*n));
    let origin_ok =
        header(ORIGIN).is_none_or(|o| o.split_once("://").map(|(_, r)| r) == Some(host));
    local_name && !proxied && origin_ok
}

fn refused() -> Response {
    (
        StatusCode::FORBIDDEN,
        "Posts have to come from this site's own page, opened directly.",
    )
        .into_response()
}

fn too_soon(app: &App, peer: &Peer) -> bool {
    let mut posts = app.posts.lock().unwrap_or_else(|e| e.into_inner());
    let ip = peer.remote.ip();
    if posts.get(&ip).is_some_and(|t| t.elapsed() < POST_GAP) {
        return true;
    }
    posts.retain(|_, t| t.elapsed() < POST_GAP);
    posts.insert(ip, Instant::now());
    false
}

fn call_error(call: &str) -> Option<String> {
    (!valid_call(call) || !call.bytes().any(|b| b.is_ascii_digit()))
        .then(|| format!("{} doesn't look like a callsign.", esc(call)))
}

/// Why a player can't play for `team`, in words for the form, or None if they can.
fn team_error(app: &App, s: &Session, call: &str, team: &str) -> anyhow::Result<Option<String>> {
    if let Some(e) = call_error(call) {
        return Ok(Some(e));
    }
    if !s.teams.iter().any(|t| t == team) {
        return Ok(Some("Pick one of the teams in the list.".into()));
    }
    let played = app.played(s)?;
    let on = played
        .players
        .iter()
        .find(|p| p.call == call && !p.team.is_empty())
        .map(|p| &p.team);
    Ok(s.roster
        .get(call)
        .or(on)
        .filter(|t| *t != team)
        .map(|t| format!("{} plays for {} in this game.", esc(call), esc(t))))
}

/// Have this site's core sign an event, then read it back so the page can show it.
async fn sign(app: &App, kind: &str, payload: Value) -> anyhow::Result<String> {
    let ev = app.core.append(kind, payload).await?;
    if let Err(e) = ridgewarden_client::follow(&app.core, &app.db, crate::PROJECTIONS).await {
        tracing::warn!("following the core after a post: {e:#}");
    }
    app.forget_scores();
    Ok(ev.id)
}

fn select(name: &str, options: &[(String, String)]) -> String {
    let mut b = format!("<select name={name} required>");
    for (value, label) in options {
        let _ = write!(
            b,
            "<option value=\"{}\">{}</option>",
            esc(value),
            esc(label)
        );
    }
    b.push_str("</select>");
    b
}

fn teams(s: &Session) -> String {
    let opts: Vec<(String, String)> = s.teams.iter().map(|t| (t.clone(), t.clone())).collect();
    select("team", &opts)
}

fn call_input(call: &str) -> String {
    format!(
        "<label>Callsign<br><input name=call value=\"{}\" required maxlength=20 \
         autocapitalize=characters autocomplete=off spellcheck=false></label>",
        esc(call)
    )
}

fn header(title: &str, s: &Session, error: Option<&str>) -> String {
    let mut b = String::from(STYLE);
    let _ = write!(
        b,
        "<h1>{}</h1><p>{}: {}</p>",
        esc(title),
        esc(&s.name),
        status_line(s, now_ms())
    );
    if let Some(e) = error {
        let _ = write!(b, "<div class=banner>{e}</div>");
    }
    b
}

fn closed(title: &str, why: &str) -> String {
    format!(
        "<h1>{}</h1><p>{why}</p><p><a href=\"/\">See the game</a></p>",
        esc(title)
    )
}

/// The result of a check-in or code word claim, as the scorer judged it.
fn outcome(title: &str, played: &Played, id: &str) -> String {
    let mut b = format!("<h1>{}</h1>", esc(title));
    match played.checkins.iter().find(|c| c.id == id) {
        Some(c) if c.counted => {
            let _ = write!(
                b,
                "<div class=\"headline green\">Counted: {}, {} points.</div>",
                esc(&c.note),
                c.points
            );
        }
        Some(c) if c.pending => {
            let _ = write!(
                b,
                "<div class=\"headline yellow\">Logged. It counts if the word was right: {}.</div>",
                esc(&c.note)
            );
        }
        Some(c) => {
            let _ = write!(
                b,
                "<div class=\"headline yellow\">Logged, but it doesn't count: {}.</div>",
                esc(&c.note)
            );
        }
        None => {
            b.push_str("<p>Sent. It shows on the scoreboard once this site has read it back.</p>")
        }
    }
    b.push_str("<p><a href=\"/\">See the game</a></p>");
    b
}

fn checkin_form(app: &App, here: &str, s: &Session, call: &str, error: Option<&str>) -> String {
    let title = format!("Check in at {}", app.cfg.label(here));
    let mut b = header(&title, s, error);
    let _ = write!(
        b,
        "<form class=checkin method=post action=\"/checkin\">{}<label>Team<br>{}</label>\
         <button type=submit>Check in</button></form>\
         <p class=\"mute small\">You keep the team of your first check-in for the whole game. \
         Your callsign and team are signed by this site's core and shown to everyone watching.</p>",
        call_input(call),
        teams(s)
    );
    b
}

pub async fn checkin_page(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<Peer>,
) -> Result<Html<String>, Fail> {
    let here = app.me().await.map(|w| w.node).unwrap_or_default();
    let sessions = app.sessions()?;
    let s = current(&sessions, now_ms());
    let body = match (gate(&app, &here, s, &peer), s) {
        (None, Some(s)) => checkin_form(&app, &here, s, "", None),
        (why, _) => closed("Check in", &why.unwrap_or_default()),
    };
    Ok(page(&app, "/checkin", "Check in", &body, false).await)
}

#[derive(Deserialize)]
pub struct CheckinForm {
    call: String,
    team: String,
}

pub async fn checkin_post(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    headers: HeaderMap,
    Form(f): Form<CheckinForm>,
) -> Result<Response, Fail> {
    if !direct_from_our_page(&headers) {
        return Ok(refused());
    }
    let here = app.me().await.map(|w| w.node).unwrap_or_default();
    let sessions = app.sessions()?;
    let s = current(&sessions, now_ms());
    let (None, Some(s)) = (gate(&app, &here, s, &peer), s) else {
        return Ok(checkin_page(State(app), ConnectInfo(peer))
            .await?
            .into_response());
    };
    let call = f.call.trim().to_ascii_uppercase();
    let error = if too_soon(&app, &peer) {
        Some("Wait a few seconds before posting again.".to_string())
    } else {
        team_error(&app, s, &call, &f.team)?
    };
    if let Some(e) = error {
        let body = checkin_form(&app, &here, s, &call, Some(&e));
        return Ok(page(&app, "/checkin", "Check in", &body, false)
            .await
            .into_response());
    }
    let payload = json!({"session": s.id, "call": call, "team": f.team, "site": here});
    let id = sign(&app, "game.checkin", payload).await?;
    tracing::info!("check-in by {call} for {} at {here}", f.team);
    let body = outcome(&app.cfg.label(&here), &*app.played(s)?, &id);
    Ok(page(&app, "/checkin", "Checked in", &body, false)
        .await
        .into_response())
}

fn word_targets(app: &App, s: &Session) -> Vec<(String, String)> {
    s.word_targets()
        .into_iter()
        .map(|t| {
            let label = match &t {
                Target::Site(n) => app.cfg.label(n),
                Target::Fox(f) => format!("fox: {f}"),
            };
            (t.key(), label)
        })
        .collect()
}

fn word_form(app: &App, s: &Session, call: &str, error: Option<&str>) -> String {
    let mut b = header("Code word", s, error);
    let targets = word_targets(app, s);
    if targets.is_empty() {
        b.push_str("<p>This game has no public sites or foxes, so there are no code words.</p>");
        return b;
    }
    let _ = write!(
        b,
        "<form class=checkin method=post action=\"/word\">{}<label>Team<br>{}</label>\
         <label>Heard at<br>{}</label><label>Code word<br><input name=word required maxlength=64 \
         autocapitalize=characters autocomplete=off spellcheck=false placeholder=\"OTTER LANTERN 42\">\
         </label><button type=submit>Send</button></form>\
         <p class=\"mute small\">Public sites and foxes send a code word on air that changes \
         every {} minutes. Type it in from any site's page. You get one try per word; it counts \
         once the word is revealed, a few minutes after it changes, if it was right and you \
         sent it before then.</p>",
        call_input(call),
        teams(s),
        select("target", &targets),
        s.rules.word_min
    );
    b
}

pub async fn word_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let sessions = app.sessions()?;
    let s = current(&sessions, now_ms());
    let body = match (open_gate(&app, s), s) {
        (None, Some(s)) => word_form(&app, s, "", None),
        (why, _) => closed("Code word", &why.unwrap_or_default()),
    };
    Ok(page(&app, "/word", "Code word", &body, false).await)
}

#[derive(Deserialize)]
pub struct WordForm {
    call: String,
    team: String,
    target: String,
    word: String,
}

pub async fn word_post(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    headers: HeaderMap,
    Form(f): Form<WordForm>,
) -> Result<Response, Fail> {
    if !direct_from_our_page(&headers) {
        return Ok(refused());
    }
    let sessions = app.sessions()?;
    let s = current(&sessions, now_ms());
    let (None, Some(s)) = (open_gate(&app, s), s) else {
        return Ok(word_page(State(app)).await?.into_response());
    };
    let call = f.call.trim().to_ascii_uppercase();
    let target = s.word_targets().into_iter().find(|t| t.key() == f.target);
    let reveals = game::reveals(&app.db(), s)?;
    let error = if too_soon(&app, &peer) {
        Some("Wait a few seconds before posting again.".to_string())
    } else if target.is_none() {
        Some("Pick where you heard it from the list.".into())
    } else if let Err(e) = words::plausible(&f.word) {
        Some(esc(&e.to_string()))
    } else if target
        .as_ref()
        .is_some_and(|t| words::revealed(&reveals, t, &f.word))
    {
        Some("That word has already been revealed, so it no longer counts.".into())
    } else {
        team_error(&app, s, &call, &f.team)?
    };
    let (None, Some(target)) = (error.as_ref(), target) else {
        let body = word_form(&app, s, &call, error.as_deref());
        return Ok(page(&app, "/word", "Code word", &body, false)
            .await
            .into_response());
    };
    let mut payload = json!({"session": s.id, "call": call, "team": f.team,
        "word": f.word.trim().to_ascii_uppercase()});
    match &target {
        Target::Site(n) => payload["site"] = json!(n),
        Target::Fox(n) => payload["fox"] = json!(n),
    }
    let id = sign(&app, "game.word", payload).await?;
    let body = outcome("Code word", &*app.played(s)?, &id);
    Ok(page(&app, "/word", "Code word", &body, false)
        .await
        .into_response())
}

fn bearings_list(p: &Played) -> String {
    if p.bearings.is_empty() {
        return "<p class=mute>No bearings yet.</p>".into();
    }
    let mut b = String::from(
        "<div class=wrap><table><tr><th>Time</th><th>Callsign</th><th>Fox</th><th>From</th>\
         <th class=n>Bearing</th></tr>",
    );
    for x in p.bearings.iter().rev().take(40) {
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.4}, {:.4}</td><td class=n>{:.0}&deg;</td></tr>",
            esc(&short_time(x.ts)[11..]),
            esc(&x.call),
            esc(&x.fox),
            x.from.lat,
            x.from.lon,
            x.deg
        );
    }
    b.push_str("</table></div>");
    b
}

fn hunt_form(app: &App, s: &Session, p: &Played, error: Option<&str>) -> String {
    let mut b = header("Fox hunt", s, error);
    if s.foxes.is_empty() {
        b.push_str("<p>This game has no foxes.</p>");
        return b;
    }
    let foxes: Vec<(String, String)> = s
        .foxes
        .iter()
        .map(|f| (f.name.clone(), f.name.clone()))
        .collect();
    let mut from = vec![(String::new(), "the location typed below".to_string())];
    from.extend(p.positions.keys().map(|n| (n.clone(), app.cfg.label(n))));
    let _ = write!(
        b,
        "<p>Share a bearing on a fox, the way POTA hunters share spots: it shows on the map as \
         a line from where you took it, so other hunters can cross it with theirs. Finding a \
         fox is proven by its code word, on the <a href=\"/word\">code word page</a>.</p>\
         <form class=checkin method=post action=\"/hunt\">{}<label>Fox<br>{}</label>\
         <label>Taken from<br>{}</label><label>or latitude, longitude<br><input name=at \
         maxlength=40 placeholder=\"35.300, -120.660\"></label><label>Bearing, degrees from true \
         north<br><input name=deg required inputmode=decimal maxlength=6></label>\
         <button type=submit>Share bearing</button></form><h2>Bearings so far</h2>{}",
        call_input(""),
        select("fox", &foxes),
        select("from", &from),
        bearings_list(p)
    );
    b
}

pub async fn hunt_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let sessions = app.sessions()?;
    let s = current(&sessions, now_ms());
    let body = match (open_gate(&app, s), s) {
        (None, Some(s)) => hunt_form(&app, s, &*app.played(s)?, None),
        (why, _) => closed("Fox hunt", &why.unwrap_or_default()),
    };
    Ok(page(&app, "/hunt", "Fox hunt", &body, false).await)
}

#[derive(Deserialize)]
pub struct HuntForm {
    call: String,
    fox: String,
    from: String,
    at: String,
    deg: String,
}

/// "35.3, -120.66" or "35.3 -120.66"
fn parse_pos(s: &str) -> Option<Pos> {
    let mut it = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|x| !x.is_empty());
    let (lat, lon) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if it.next().is_some() {
        return None;
    }
    Pos::new(lat, lon)
}

fn bearing_payload(s: &Session, p: &Played, f: &HuntForm) -> Result<Value, String> {
    let call = f.call.trim().to_ascii_uppercase();
    if let Some(e) = call_error(&call) {
        return Err(e);
    }
    if s.fox(&f.fox).is_none() {
        return Err("Pick a fox from the list.".into());
    }
    let from = if f.at.trim().is_empty() {
        p.positions.get(&f.from).copied()
    } else {
        parse_pos(&f.at)
    }
    .ok_or("Pick a site, or type a latitude and longitude like 35.300, -120.660.")?;
    let deg: f64 = f
        .deg
        .trim()
        .parse()
        .ok()
        .filter(|d| (0.0..360.0).contains(d))
        .ok_or("The bearing is degrees from true north, 0 to 359.")?;
    Ok(
        json!({"session": s.id, "call": call, "fox": f.fox, "lat": from.lat, "lon": from.lon, "deg": deg}),
    )
}

pub async fn hunt_post(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    headers: HeaderMap,
    Form(f): Form<HuntForm>,
) -> Result<Response, Fail> {
    if !direct_from_our_page(&headers) {
        return Ok(refused());
    }
    let sessions = app.sessions()?;
    let s = current(&sessions, now_ms());
    let (None, Some(s)) = (open_gate(&app, s), s) else {
        return Ok(hunt_page(State(app)).await?.into_response());
    };
    let played = app.played(s)?;
    let payload = if too_soon(&app, &peer) {
        Err("Wait a few seconds before posting again.".to_string())
    } else {
        bearing_payload(s, &played, &f)
    };
    let error = match payload {
        Ok(p) => {
            sign(&app, "game.bearing", p).await?;
            None
        }
        Err(e) => Some(e),
    };
    let body = hunt_form(&app, s, &*app.played(s)?, error.as_deref());
    Ok(page(&app, "/hunt", "Fox hunt", &body, false)
        .await
        .into_response())
}

fn spot_form(s: &Session, p: &Played, error: Option<&str>) -> String {
    let mut b = header("Spots", s, error);
    let _ = write!(
        b,
        "<p>Tell everyone what's happening: where you're headed, a site you're on air at, a \
         link that looks shaky. Spots earn no points.</p>\
         <form class=checkin method=post action=\"/spot\">{}<label>Spot<br><input name=text \
         required maxlength={TEXT_MAX}></label><button type=submit>Post spot</button></form>\
         <h2>Latest spots</h2>{}",
        call_input(""),
        crate::web::spots_list(p)
    );
    b
}

pub async fn spot_page(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let sessions = app.sessions()?;
    let s = current(&sessions, now_ms());
    let body = match (open_gate(&app, s), s) {
        (None, Some(s)) => spot_form(s, &*app.played(s)?, None),
        (why, _) => closed("Spots", &why.unwrap_or_default()),
    };
    Ok(page(&app, "/spot", "Spots", &body, false).await)
}

#[derive(Deserialize)]
pub struct SpotForm {
    call: String,
    text: String,
}

pub async fn spot_post(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    headers: HeaderMap,
    Form(f): Form<SpotForm>,
) -> Result<Response, Fail> {
    if !direct_from_our_page(&headers) {
        return Ok(refused());
    }
    let sessions = app.sessions()?;
    let s = current(&sessions, now_ms());
    let (None, Some(s)) = (open_gate(&app, s), s) else {
        return Ok(spot_page(State(app)).await?.into_response());
    };
    let call = f.call.trim().to_ascii_uppercase();
    let text = f.text.trim();
    let error = if too_soon(&app, &peer) {
        Some("Wait a few seconds before posting again.".to_string())
    } else if let Some(e) = call_error(&call) {
        Some(e)
    } else if text.is_empty() || text.chars().count() > TEXT_MAX {
        Some(format!("A spot is 1 to {TEXT_MAX} characters."))
    } else {
        None
    };
    if error.is_none() {
        sign(
            &app,
            "game.spot",
            json!({"session": s.id, "call": call, "text": text}),
        )
        .await?;
    }
    let body = spot_form(s, &*app.played(s)?, error.as_deref());
    Ok(page(&app, "/spot", "Spots", &body, false)
        .await
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EXAMPLE};
    use crate::game::MIN;
    use crate::testing::FakeCore;

    async fn body(r: Response) -> String {
        let b = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(b.to_vec()).unwrap()
    }

    /// A check-in site "a" whose LAN is 10.54.12.0/29 and whose box is 10.54.12.2, with a
    /// game running.
    /// with a public site and a fox too, started `ago` minutes ago
    async fn site_from(ago: i64) -> (Arc<App>, FakeCore, tempfile::TempDir) {
        let now = ridgewarden_client::util::now_ms();
        let poll = json!({"node": "a", "ok": true, "via": "lan", "lat": 35.0, "lon": -120.0});
        let start = json!({"name": "Meeting", "teams": ["Red", "Blue"], "sites": ["a"],
            "word_sites": ["park"], "foxes": [{"name": "Fox 1"}]});
        let core = FakeCore::start(
            "a",
            &[
                ("node.poll", poll, now - ago * MIN),
                ("game.start", start, now - ago * MIN),
            ],
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let token = dir.path().join("local.token");
        std::fs::write(&token, "secret").unwrap();
        let mut cfg: Config = toml::from_str(EXAMPLE).unwrap();
        cfg.data_dir = dir.path().into();
        cfg.core.url = core.url.clone();
        cfg.core.token_file = token;
        cfg.admins = vec![core.key.clone()];
        cfg.lan = vec!["10.54.12.0/29".into()];
        let app = Arc::new(App::new(cfg, true).unwrap());
        ridgewarden_client::follow(&app.core, &app.db, crate::PROJECTIONS)
            .await
            .unwrap();
        (app, core, dir)
    }

    async fn site() -> (Arc<App>, FakeCore, tempfile::TempDir) {
        site_from(1).await
    }

    async fn post(
        app: &Arc<App>,
        from: &str,
        origin: Option<&str>,
        call: &str,
        team: &str,
    ) -> Response {
        let mut h = headers(&[("host", "10.54.12.2:8797")]);
        if let Some(o) = origin {
            h.insert(ORIGIN, o.parse().unwrap());
        }
        let form = CheckinForm {
            call: call.into(),
            team: team.into(),
        };
        let p = peer(&format!("{from}:50000"), "10.54.12.2:8797");
        checkin_post(State(app.clone()), ConnectInfo(p), h, Form(form))
            .await
            .unwrap_or_else(|e| panic!("{:#}", e.0))
    }

    #[tokio::test]
    async fn check_ins_are_taken_only_from_the_lan_and_signed_by_the_core() {
        let (app, core, _dir) = site().await;
        let get = |from: &str| {
            let p = peer(&format!("{from}:50000"), "10.54.12.2:8797");
            checkin_page(State(app.clone()), ConnectInfo(p))
        };
        let far = get("10.99.0.5")
            .await
            .unwrap_or_else(|e| panic!("{:#}", e.0))
            .0;
        assert!(
            far.contains("isn't on a's LAN") && !far.contains("<form"),
            "{far}"
        );
        let near = get("10.54.12.3")
            .await
            .unwrap_or_else(|e| panic!("{:#}", e.0))
            .0;
        assert!(near.contains("<form"), "{near}");

        let own = body(post(&app, "10.54.12.2", None, "N0AAA", "Red").await).await;
        assert!(own.contains("game's own box"), "{own}");
        assert_eq!(core.appended("game.checkin"), 0);

        let ok = body(
            post(
                &app,
                "10.54.12.3",
                Some("http://10.54.12.2:8797"),
                "n0aaa",
                "Red",
            )
            .await,
        )
        .await;
        assert!(ok.contains("Counted: captured for Red, 10 points"), "{ok}");
        assert_eq!(core.appended("game.checkin"), 1);

        let again = body(post(&app, "10.54.12.3", None, "N0BBB", "Blue").await).await;
        assert!(again.contains("Wait a few seconds"), "{again}");
        let switch = body(post(&app, "10.54.12.4", None, "N0AAA", "Blue").await).await;
        assert!(switch.contains("N0AAA plays for Red"), "{switch}");
        let junk = body(post(&app, "10.54.12.5", None, "<b>1", "Red").await).await;
        assert!(
            junk.contains("&lt;B&gt;1 doesn't look like a callsign"),
            "{junk}"
        );
        let r = post(
            &app,
            "10.54.12.6",
            Some("http://evil.example"),
            "N0CCC",
            "Red",
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(core.appended("game.checkin"), 1, "nothing else was signed");
    }

    fn peer(remote: &str, local: &str) -> Peer {
        Peer {
            remote: remote.parse().unwrap(),
            local: Some(local.parse().unwrap()),
        }
    }

    #[test]
    fn only_lan_devices_other_than_this_box_may_check_in() {
        let lan = [Net::parse("10.54.12.0/29").unwrap()];
        assert!(on_lan(&lan, &peer("10.54.12.5:50000", "10.54.12.2:8797")));
        assert!(
            !on_lan(&lan, &peer("10.54.12.2:50000", "10.54.12.2:8797")),
            "the Pi itself"
        );
        assert!(
            !on_lan(&lan, &peer("10.99.1.5:50000", "10.54.12.2:8797")),
            "across the mesh"
        );
        assert!(!on_lan(&lan, &peer("127.0.0.1:50000", "127.0.0.1:8797")));
        assert!(!on_lan(&[], &peer("10.54.12.5:50000", "10.54.12.2:8797")));
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn only_direct_posts_from_our_own_page_are_taken() {
        let ok = |p: &[(&'static str, &str)]| direct_from_our_page(&headers(p));
        assert!(ok(&[("host", "tst-pi.local.mesh:8797")]));
        assert!(ok(&[
            ("host", "10.54.12.2:8797"),
            ("origin", "http://10.54.12.2:8797")
        ]));
        assert!(ok(&[("host", "[fe80::1]:8797")]));
        assert!(ok(&[("host", "tst-pi:8797")]));
        assert!(!ok(&[]), "no Host");
        assert!(
            !ok(&[
                ("host", "tst-pi.local.mesh:8797"),
                ("origin", "http://evil.example")
            ]),
            "another site's form"
        );
        assert!(
            !ok(&[
                ("host", "rebind.evil.example:8797"),
                ("origin", "http://rebind.evil.example:8797")
            ]),
            "DNS rebinding"
        );
        assert!(
            !ok(&[("host", "10.54.12.2:8797"), ("x-forwarded-for", "10.9.9.9")]),
            "through a proxy"
        );
    }

    fn from(ip: &str) -> ConnectInfo<Peer> {
        ConnectInfo(peer(&format!("{ip}:50000"), "10.54.12.2:8797"))
    }

    fn ok<T>(r: Result<T, Fail>) -> T {
        r.unwrap_or_else(|e| panic!("{:#}", e.0))
    }

    async fn text(r: Result<Response, Fail>) -> String {
        body(ok(r)).await
    }

    fn here() -> HeaderMap {
        headers(&[("host", "tst-pi.local.mesh:8797")])
    }

    #[tokio::test]
    async fn code_words_are_taken_from_anywhere_and_wait_for_their_reveal() {
        let (app, core, _dir) = site().await;
        let form = |target: &str, word: &str| WordForm {
            call: "n0aaa".into(),
            team: "Red".into(),
            target: target.into(),
            word: word.into(),
        };
        // across the mesh, not on any LAN
        let r = ok(word_post(
            State(app.clone()),
            from("10.99.0.5"),
            here(),
            Form(form("site:park", "otter kite 01")),
        )
        .await);
        let b = body(r).await;
        assert!(
            b.contains("Logged. It counts if the word was right: waiting"),
            "{b}"
        );
        assert_eq!(core.appended("game.word"), 1);

        let r = ok(word_post(
            State(app.clone()),
            from("10.99.0.6"),
            here(),
            Form(form("site:elsewhere", "otter kite 01")),
        )
        .await);
        assert!(body(r).await.contains("Pick where you heard it"));
        let r = ok(word_post(
            State(app.clone()),
            from("10.99.0.7"),
            here(),
            Form(form("fox:Fox 1", "x")),
        )
        .await);
        assert!(body(r).await.contains("look like a code word"));
        assert_eq!(core.appended("game.word"), 1);

        // a word that has been revealed already is refused before it is signed
        let now = ridgewarden_client::util::now_ms();
        let session = app.sessions().unwrap().remove(0).id;
        let reveal =
            json!({"session": session, "site": "park", "slot": 0, "word": "CEDAR MESA 77"});
        core.add("game.reveal", reveal, now);
        ridgewarden_client::follow(&app.core, &app.db, crate::PROJECTIONS)
            .await
            .unwrap();
        let r = ok(word_post(
            State(app.clone()),
            from("10.99.0.8"),
            here(),
            Form(form("site:park", "cedar mesa 77")),
        )
        .await);
        assert!(body(r).await.contains("already been revealed"));
        let mut nodigit = form("site:park", "otter kite 01");
        nodigit.call = "HELLO".into();
        let r = ok(word_post(State(app.clone()), from("10.99.0.9"), here(), Form(nodigit)).await);
        assert!(body(r).await.contains("doesn't look like a callsign"));
        assert_eq!(core.appended("game.word"), 1);
    }

    #[tokio::test]
    async fn bearings_and_spots_are_signed_and_shown() {
        let (app, core, _dir) = site().await;
        let hunt = |deg: &str| HuntForm {
            call: "N0BBB".into(),
            fox: "Fox 1".into(),
            from: "a".into(),
            at: String::new(),
            deg: deg.into(),
        };
        let b = text(
            hunt_post(
                State(app.clone()),
                from("10.99.0.5"),
                here(),
                Form(hunt("400")),
            )
            .await,
        )
        .await;
        assert!(b.contains("0 to 359"), "{b}");
        let b = text(
            hunt_post(
                State(app.clone()),
                from("10.99.0.6"),
                here(),
                Form(hunt("45")),
            )
            .await,
        )
        .await;
        assert!(b.contains("45&deg;"), "{b}");
        assert_eq!(core.appended("game.bearing"), 1);

        let spot = SpotForm {
            call: "N0BBB".into(),
            text: "<b>heading</b> to the park".into(),
        };
        let b =
            text(spot_post(State(app.clone()), from("10.99.0.7"), here(), Form(spot)).await).await;
        assert!(b.contains("&lt;b&gt;heading&lt;/b&gt; to the park"), "{b}");
        assert_eq!(core.appended("game.spot"), 1);
    }

    #[tokio::test]
    async fn the_admins_site_reveals_each_word_once_its_slot_is_over() {
        let (app, core, _dir) = site_from(40).await;
        assert_eq!(
            words::reveal_due(&app).await.unwrap(),
            0,
            "no seed, not the admin's site"
        );
        words::seed(&words::seed_path(&app.cfg.data_dir)).unwrap();
        // 40 minutes in with 15-minute words: two slots over, for the park and the fox
        assert_eq!(words::reveal_due(&app).await.unwrap(), 4);
        assert_eq!(words::reveal_due(&app).await.unwrap(), 0);
        assert_eq!(core.appended("game.reveal"), 4);
    }

    #[test]
    fn typed_locations_parse() {
        assert_eq!(parse_pos("35.3, -120.66"), Pos::new(35.3, -120.66));
        assert_eq!(parse_pos("35.3 -120.66"), Pos::new(35.3, -120.66));
        assert_eq!(parse_pos("35.3"), None);
        assert_eq!(parse_pos("135, 0"), None);
    }
}
