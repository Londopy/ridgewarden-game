//! The pages players post from: check-ins on a site's LAN, code words heard on air and
//! fox codes, bearings on a fox, and spots. The page's script signs every post with the
//! player's own key; this site checks that signature, then its core signs the post too.

use std::fmt::Write as _;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Form;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HOST, ORIGIN};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use ridgewarden_client::event::valid_call;
use ridgewarden_client::util::{esc, now_ms, short_time};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::App;
use crate::config::Net;
use crate::game::{self, DEG_MAX, MIN, POS_MAX, Session, TEXT_MAX, Target, current};
use crate::score::Played;
use crate::web::{Fail, Peer, STYLE, page, status_line};
use crate::words;
use crate::world::Pos;

/// One post per address this often.
const POST_GAP: Duration = Duration::from_secs(10);
/// A page is only good for posting this long after it was made, so a signed post someone
/// caught on the LAN can't be held back and sent much later.
const FORM_TTL: i64 = 30 * MIN;

const PLAYER_JS: &str = include_str!("player.js");

/// The script that signs players' posts.
pub async fn player_js() -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (CACHE_CONTROL, "no-cache"),
        ],
        PLAYER_JS,
    )
}

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

/// Why `call` can't post in this game: it isn't a callsign, or the game has a roster and
/// it isn't on it.
fn call_error(s: &Session, call: &str) -> Option<String> {
    if !valid_call(call) || !call.bytes().any(|b| b.is_ascii_digit()) {
        return Some(format!("{} doesn't look like a callsign.", esc(call)));
    }
    (!s.roster.is_empty() && !s.roster.contains_key(call))
        .then(|| format!("{} isn't on this game's roster.", esc(call)))
}

/// Why a player can't play for `team`, in words for the form, or None if they can.
fn team_error(app: &App, s: &Session, call: &str, team: &str) -> anyhow::Result<Option<String>> {
    if let Some(e) = call_error(s, call) {
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

/// What the page's script adds to every post: the game the page was for, when it was
/// made, and the player's key and signature.
#[derive(Deserialize)]
pub struct Sig {
    session: String,
    t: String,
    pk: String,
    psig: String,
}

/// Check the player's signature on a post and add it to `payload`, or say why the post
/// can't be taken.
fn sig_error(
    app: &App,
    s: &Session,
    kind: &str,
    sig: &Sig,
    payload: &mut Value,
) -> anyhow::Result<Option<String>> {
    if sig.psig.is_empty() {
        return Ok(Some(
            "Posting needs JavaScript: your browser signs each post with your own key.".into(),
        ));
    }
    if sig.session != s.id {
        return Ok(Some(
            "This page was for another game. Reload it and try again.".into(),
        ));
    }
    let now = now_ms();
    let Some(t) = sig
        .t
        .parse::<i64>()
        .ok()
        .filter(|t| now - FORM_TTL <= *t && *t <= now + MIN)
    else {
        return Ok(Some(
            "This page is too old. Reload it and try again.".into(),
        ));
    };
    payload["session"] = json!(s.id);
    payload["t"] = json!(t);
    payload["pk"] = json!(sig.pk);
    payload["psig"] = json!(sig.psig);
    if game::signer(kind, payload).is_none() {
        return Ok(Some(
            "The signature doesn't match the post. Reload the page and try again.".into(),
        ));
    }
    let call = payload["call"].as_str().unwrap_or_default();
    let keys = game::keys(&app.db(), &app.cfg.admins)?;
    Ok(keys.get(call).filter(|k| **k != sig.pk).map(|_| {
        format!(
            "{} plays with another key. Use the passphrase you used before; if it's lost, an \
             admin can set your callsign's key to the one this passphrase makes: {}",
            esc(call),
            esc(&sig.pk)
        )
    }))
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

/// A drop-down; `attrs` is e.g. "name=team".
fn select(attrs: &str, options: &[(String, String)]) -> String {
    let mut b = format!("<select {attrs}>");
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
    select("name=team", &opts)
}

/// Callsign and passphrase. The passphrases have no name, so the browser never sends them.
/// Where a post can set the callsign's key (`binds`), the script asks for the passphrase
/// twice the first time, so a typo isn't kept for good. It can also suggest one.
fn player_inputs(call: &str, binds: bool) -> String {
    let again = if binds {
        "<label data-again hidden>Passphrase again<br><input type=password maxlength=200 \
         autocomplete=off></label>"
    } else {
        ""
    };
    format!(
        "<label>Callsign<br><input name=call value=\"{}\" required maxlength=20 \
         autocapitalize=characters autocomplete=off spellcheck=false></label>\
         <label>Passphrase<br><input type=password data-pass required minlength=12 \
         maxlength=200 autocomplete=off></label>{again}\
         <p data-suggest hidden><button type=button data-words=\"{}\">Suggest a passphrase\
         </button> <span class=\"mute small\" aria-live=polite></span></p>",
        esc(call),
        words::passphrase_words()
    )
}

#[derive(Deserialize)]
pub struct Call {
    call: String,
}

/// The key a callsign plays with, or nothing if it has none yet, so the page can ask for
/// the passphrase twice before a post that would set it.
pub async fn key_of(State(app): State<Arc<App>>, Query(q): Query<Call>) -> Result<String, Fail> {
    Ok(game::keys(&app.db(), &app.cfg.admins)?
        .remove(&q.call)
        .unwrap_or_default())
}

/// A player's form. The page's script fills in `pk` and `psig` before it goes.
fn signed_form(s: &Session, kind: &str, action: &str, fields: &str, button: &str) -> String {
    format!(
        "<form class=checkin method=post action=\"{action}\" data-sign=\"{kind}\">{fields}\
         <input type=hidden name=session value=\"{}\"><input type=hidden name=t value={}>\
         <input type=hidden name=pk><input type=hidden name=psig>\
         <button type=submit>{button}</button><p class=\"signing mute small\" aria-live=polite></p>\
         </form><noscript><div class=banner>Posting needs JavaScript: your browser signs each \
         post with your own key.</div></noscript><script src=\"/player.js\"></script>",
        esc(&s.id),
        now_ms()
    )
}

const KEY_NOTE: &str = "<p class=\"mute small\">Your browser makes your key from your \
    callsign and passphrase and signs the post with it; the passphrase never leaves this page. \
    The page comes over plain HTTP, though, so someone who can change traffic between you and \
    this node could swap its script and read the passphrase. Your first check-in on a site's \
    LAN sets your callsign's key for every game (in a game with a roster, your first post \
    does), so use the same passphrase each time. Anyone can try to guess it from the key, about \
    70 guesses a second on one desktop core and more with more cores or a graphics card, so use \
    12 characters or more, or the four words the page suggests. Players are licensed operators: \
    every post crosses the mesh's radio links.</p>";

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

/// The result of a check-in, code word or fox code, as the scorer judged it.
fn outcome(title: &str, played: &Played, id: &str, pk: &str) -> String {
    let mut b = format!("{STYLE}<h1>{}</h1>", esc(title));
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
    let _ = write!(
        b,
        "<p class=\"mute small\">Signed with your player key <code>{}</code>.</p>\
         <p><a href=\"/\">See the game</a></p>",
        esc(pk)
    );
    b
}

fn checkin_form(app: &App, here: &str, s: &Session, call: &str, error: Option<&str>) -> String {
    let title = format!("Check in at {}", app.cfg.label(here));
    let mut b = header(&title, s, error);
    let fields = format!(
        "{}<label>Team<br>{}</label><input type=hidden name=site value=\"{}\">",
        player_inputs(call, true),
        teams(s),
        esc(here)
    );
    b.push_str(&signed_form(
        s,
        "game.checkin",
        "/checkin",
        &fields,
        "Check in",
    ));
    b.push_str(
        "<p class=\"mute small\">You keep the team of your first check-in for the whole game. \
         Your callsign and team are signed by your key and by this site's core, and shown to \
         everyone watching.</p>",
    );
    b.push_str(KEY_NOTE);
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
    site: String,
    #[serde(flatten)]
    sig: Sig,
}

/// Why a check-in can't be taken, or None if it can; signs `payload` on the way.
fn checkin_error(
    app: &App,
    s: &Session,
    here: &str,
    f: &CheckinForm,
    payload: &mut Value,
) -> anyhow::Result<Option<String>> {
    if f.site != here {
        return Ok(Some(
            "This page was for another site. Reload it and try again.".into(),
        ));
    }
    if let Some(e) = sig_error(app, s, "game.checkin", &f.sig, payload)? {
        return Ok(Some(e));
    }
    team_error(app, s, &f.call, &f.team)
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
    let mut payload = json!({"call": f.call, "team": f.team, "site": f.site});
    let error = if too_soon(&app, &peer) {
        Some("Wait a few seconds before posting again.".to_string())
    } else {
        checkin_error(&app, s, &here, &f, &mut payload)?
    };
    if let Some(e) = error {
        let body = checkin_form(&app, &here, s, &f.call, Some(&e));
        return Ok(page(&app, "/checkin", "Check in", &body, false)
            .await
            .into_response());
    }
    let id = sign(&app, "game.checkin", payload).await?;
    tracing::info!("check-in by {} for {} at {here}", f.call, f.team);
    let body = outcome(&app.cfg.label(&here), &*app.played(s)?, &id, &f.sig.pk);
    Ok(page(&app, "/checkin", "Checked in", &body, false)
        .await
        .into_response())
}

/// Public sites, and the foxes, whose codes are typed on the same form.
fn word_targets(s: &Session) -> Vec<Target> {
    let foxes = s.foxes.iter().map(|f| Target::Fox(f.name.clone()));
    s.word_targets().into_iter().chain(foxes).collect()
}

fn word_form(app: &App, s: &Session, call: &str, error: Option<&str>) -> String {
    let mut b = header("Code word", s, error);
    let targets: Vec<(String, String)> = word_targets(s)
        .iter()
        .map(|t| {
            let label = match t {
                Target::Site(n) => app.cfg.label(n),
                Target::Fox(f) => format!("found fox {f}"),
            };
            (t.key(), label)
        })
        .collect();
    if targets.is_empty() {
        b.push_str("<p>This game has no public sites or foxes, so there are no code words.</p>");
        return b;
    }
    let fields = format!(
        "{}<label>Team<br>{}</label><label>Heard at, or fox found<br>{}</label>\
         <label>Code word or fox code<br><input name=word required maxlength=64 \
         autocapitalize=characters autocomplete=off spellcheck=false \
         placeholder=\"OTTER LANTERN 42\"></label>",
        player_inputs(call, !s.roster.is_empty()),
        teams(s),
        select("name=target", &targets)
    );
    b.push_str(&signed_form(s, "game.word", "/word", &fields, "Send"));
    let _ = write!(
        b,
        "<p class=\"mute small\">Public sites send a code word on air that changes every {} \
         minutes. Type it in from any site's page. You get one try per word; it counts once the \
         word is revealed, a few minutes after it changes, if it was right and you sent it \
         before then.</p><p class=\"mute small\">A fox has one-time codes left with it, on a card \
         or with whoever runs it. Take one when you find the fox and type it here. Each code \
         counts once, for whoever sends it first.</p>{KEY_NOTE}",
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
    #[serde(flatten)]
    sig: Sig,
}

/// Why a code word or fox code can't be taken, or None if it can; signs `payload` on
/// the way. A word already revealed, or a code that isn't the fox's, is refused before
/// it is logged.
fn word_error(
    app: &App,
    s: &Session,
    f: &WordForm,
    target: Option<&Target>,
    payload: &mut Value,
) -> anyhow::Result<Option<String>> {
    let Some(t) = target else {
        return Ok(Some(
            "Pick where you heard it, or the fox you found, from the list.".into(),
        ));
    };
    if let Err(e) = words::plausible(&f.word) {
        return Ok(Some(esc(&e.to_string())));
    }
    match t {
        Target::Site(_) if words::revealed(&game::reveals(&app.db(), s)?, t, &f.word) => {
            return Ok(Some(
                "That word has already been revealed, so it no longer counts.".into(),
            ));
        }
        Target::Fox(fox)
            if !s
                .fox(fox)
                .is_some_and(|x| x.codes.contains(&words::code_hash(&s.nonce, fox, &f.word))) =>
        {
            return Ok(Some(format!(
                "That isn't one of {}'s codes. Check it against the card.",
                esc(fox)
            )));
        }
        _ => {}
    }
    if let Some(e) = sig_error(app, s, "game.word", &f.sig, payload)? {
        return Ok(Some(e));
    }
    team_error(app, s, &f.call, &f.team)
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
    let target = word_targets(s).into_iter().find(|t| t.key() == f.target);
    let mut payload = json!({"call": f.call, "team": f.team, "word": f.word});
    match &target {
        Some(Target::Site(n)) => payload["site"] = json!(n),
        Some(Target::Fox(n)) => payload["fox"] = json!(n),
        None => {}
    }
    let error = if too_soon(&app, &peer) {
        Some("Wait a few seconds before posting again.".to_string())
    } else {
        word_error(&app, s, &f, target.as_ref(), &mut payload)?
    };
    if error.is_some() {
        let body = word_form(&app, s, &f.call, error.as_deref());
        return Ok(page(&app, "/word", "Code word", &body, false)
            .await
            .into_response());
    }
    let id = sign(&app, "game.word", payload).await?;
    let body = outcome("Code word", &*app.played(s)?, &id, &f.sig.pk);
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
    // the script reads the position straight from the choice, so it can sign it
    let mut from = vec![(String::new(), "the location typed below".to_string())];
    from.extend(
        p.positions
            .iter()
            .map(|(n, at)| (format!("{:.5},{:.5}", at.lat, at.lon), app.cfg.label(n))),
    );
    let _ = write!(
        b,
        "<p>Share a bearing on a fox, the way POTA hunters share spots: it shows on the map as \
         a line from where you took it, so other hunters can cross it with theirs. Finding a \
         fox is proven with one of the codes left with it, on the \
         <a href=\"/word\">code word page</a>.</p>"
    );
    let fields = format!(
        "{}<label>Fox<br>{}</label><label>Taken from<br>{}</label><label>or latitude, \
         longitude<br><input data-at maxlength=40 placeholder=\"35.300, -120.660\"></label>\
         <label>Bearing, degrees from true north<br><input name=deg required inputmode=decimal \
         maxlength=8></label><input type=hidden name=lat><input type=hidden name=lon>",
        player_inputs("", !s.roster.is_empty()),
        select("name=fox", &foxes),
        select("data-from", &from)
    );
    b.push_str(&signed_form(
        s,
        "game.bearing",
        "/hunt",
        &fields,
        "Share bearing",
    ));
    let _ = write!(b, "{KEY_NOTE}<h2>Bearings so far</h2>{}", bearings_list(p));
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
    lat: String,
    lon: String,
    deg: String,
    #[serde(flatten)]
    sig: Sig,
}

/// Why a bearing can't be taken, or None if it can; signs `payload` on the way.
fn bearing_error(
    app: &App,
    s: &Session,
    f: &HuntForm,
    payload: &mut Value,
) -> anyhow::Result<Option<String>> {
    if let Some(e) = call_error(s, &f.call) {
        return Ok(Some(e));
    }
    if s.fox(&f.fox).is_none() {
        return Ok(Some("Pick a fox from the list.".into()));
    }
    if game::num(payload, "lat", POS_MAX)
        .zip(game::num(payload, "lon", POS_MAX))
        .and_then(|(lat, lon)| Pos::new(lat, lon))
        .is_none()
    {
        return Ok(Some(
            "Pick a site, or type a latitude and longitude like 35.300, -120.660.".into(),
        ));
    }
    if !game::num(payload, "deg", DEG_MAX).is_some_and(|d| (0.0..360.0).contains(&d)) {
        return Ok(Some(
            "The bearing is degrees from true north, 0 to 359.".into(),
        ));
    }
    sig_error(app, s, "game.bearing", &f.sig, payload)
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
    let mut payload =
        json!({"call": f.call, "fox": f.fox, "lat": f.lat, "lon": f.lon, "deg": f.deg});
    let error = if too_soon(&app, &peer) {
        Some("Wait a few seconds before posting again.".to_string())
    } else {
        bearing_error(&app, s, &f, &mut payload)?
    };
    if error.is_none() {
        sign(&app, "game.bearing", payload).await?;
    }
    let body = hunt_form(&app, s, &*app.played(s)?, error.as_deref());
    Ok(page(&app, "/hunt", "Fox hunt", &body, false)
        .await
        .into_response())
}

fn spot_form(s: &Session, p: &Played, error: Option<&str>) -> String {
    let mut b = header("Spots", s, error);
    b.push_str(
        "<p>Tell everyone what's happening: where you're headed, a site you're on air at, a \
         link that looks shaky. Spots earn no points.</p>",
    );
    let fields = format!(
        "{}<label>Spot<br><input name=text required maxlength={TEXT_MAX}></label>",
        player_inputs("", !s.roster.is_empty())
    );
    b.push_str(&signed_form(s, "game.spot", "/spot", &fields, "Post spot"));
    let _ = write!(
        b,
        "{KEY_NOTE}<h2>Latest spots</h2>{}",
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
    #[serde(flatten)]
    sig: Sig,
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
    let mut payload = json!({"call": f.call, "text": f.text});
    let error = if too_soon(&app, &peer) {
        Some("Wait a few seconds before posting again.".to_string())
    } else if let Some(e) = call_error(s, &f.call) {
        Some(e)
    } else if f.text.trim().is_empty() || f.text.chars().count() > TEXT_MAX {
        Some(format!("A spot is 1 to {TEXT_MAX} characters."))
    } else {
        sig_error(&app, s, "game.spot", &f.sig, &mut payload)?
    };
    if error.is_none() {
        sign(&app, "game.spot", payload).await?;
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
    use crate::testing::{FakeCore, player_key, signed};
    use axum::extract::FromRequest;
    use ed25519_dalek::SigningKey;

    async fn body(r: Response) -> String {
        let b = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(b.to_vec()).unwrap()
    }

    /// A check-in site "a" whose LAN is 10.54.12.0/29 and whose box is 10.54.12.2, with a
    /// game running that also has a public site and a fox with codes "AAAA-BBBB-CCCC" and
    /// "DDDD-EEEE-FFFF", started `ago` minutes ago.
    async fn site_from(ago: i64) -> (Arc<App>, FakeCore, tempfile::TempDir) {
        let now = ridgewarden_client::util::now_ms();
        let poll = json!({"node": "a", "ok": true, "via": "lan", "lat": 35.0, "lon": -120.0});
        let codes: Vec<String> = ["AAAA-BBBB-CCCC", "DDDD-EEEE-FFFF"]
            .iter()
            .map(|c| words::code_hash("ab", "Fox 1", c))
            .collect();
        let start = json!({"name": "Meeting", "teams": ["Red", "Blue"], "sites": ["a"],
            "word_sites": ["park"], "nonce": "ab", "foxes": [{"name": "Fox 1", "codes": codes}]});
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

    fn session(app: &App) -> String {
        app.sessions().unwrap().remove(0).id
    }

    /// What the page's script sends for a post of `fields`, signed with `key`.
    fn sig(app: &App, kind: &str, fields: Value, key: &SigningKey) -> Sig {
        let (session, t) = (session(app), now_ms());
        let mut p = fields;
        p["session"] = json!(session);
        p["t"] = json!(t);
        let p = signed(kind, p, key);
        Sig {
            session,
            t: t.to_string(),
            pk: p["pk"].as_str().unwrap().into(),
            psig: p["psig"].as_str().unwrap().into(),
        }
    }

    fn checkin(app: &App, call: &str, team: &str, key: &SigningKey) -> CheckinForm {
        let fields = json!({"call": call, "team": team, "site": "a"});
        CheckinForm {
            call: call.into(),
            team: team.into(),
            site: "a".into(),
            sig: sig(app, "game.checkin", fields, key),
        }
    }

    async fn post(app: &Arc<App>, from: &str, origin: Option<&str>, f: CheckinForm) -> String {
        let mut h = headers(&[("host", "10.54.12.2:8797")]);
        if let Some(o) = origin {
            h.insert(ORIGIN, o.parse().unwrap());
        }
        let p = peer(&format!("{from}:50000"), "10.54.12.2:8797");
        let r = checkin_post(State(app.clone()), ConnectInfo(p), h, Form(f))
            .await
            .unwrap_or_else(|e| panic!("{:#}", e.0));
        body(r).await
    }

    #[tokio::test]
    async fn check_ins_are_taken_only_from_the_lan_and_signed_by_player_and_core() {
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
        assert!(near.contains("data-sign=\"game.checkin\""), "{near}");
        assert!(near.contains("<script src=\"/player.js\">"));

        let aaa = player_key("N0AAA");
        let own = post(
            &app,
            "10.54.12.2",
            None,
            checkin(&app, "N0AAA", "Red", &aaa),
        )
        .await;
        assert!(own.contains("game's own box"), "{own}");

        let mut unsigned = checkin(&app, "N0AAA", "Red", &aaa);
        unsigned.sig.psig = String::new();
        let b = post(&app, "10.54.12.3", None, unsigned).await;
        assert!(b.contains("needs JavaScript"), "{b}");
        let mut forged = checkin(&app, "N0AAA", "Red", &aaa);
        forged.team = "Blue".into();
        let b = post(&app, "10.54.12.4", None, forged).await;
        assert!(b.contains("doesn't match the post"), "{b}");
        assert_eq!(core.appended("game.checkin"), 0);

        let ok = post(
            &app,
            "10.54.12.5",
            Some("http://10.54.12.2:8797"),
            checkin(&app, "N0AAA", "Red", &aaa),
        )
        .await;
        assert!(ok.contains("Counted: captured for Red, 10 points"), "{ok}");
        assert!(ok.contains(&crate::testing::pk(&aaa)), "{ok}");
        assert_eq!(core.appended("game.checkin"), 1);

        let again = post(
            &app,
            "10.54.12.5",
            None,
            checkin(&app, "N0BBB", "Blue", &aaa),
        )
        .await;
        assert!(again.contains("Wait a few seconds"), "{again}");
        let switch = post(
            &app,
            "10.54.12.6",
            None,
            checkin(&app, "N0AAA", "Blue", &aaa),
        )
        .await;
        assert!(switch.contains("N0AAA plays for Red"), "{switch}");
        // someone else, with their own passphrase, trying to play as N0AAA
        let other = SigningKey::from_bytes(&[5; 32]);
        let taken = post(
            &app,
            "10.54.12.7",
            None,
            checkin(&app, "N0AAA", "Red", &other),
        )
        .await;
        assert!(taken.contains("N0AAA plays with another key"), "{taken}");
        let junk = post(
            &app,
            "10.54.12.1",
            None,
            checkin(&app, "<b>1", "Red", &other),
        )
        .await;
        assert!(
            junk.contains("&lt;b&gt;1 doesn't look like a callsign"),
            "{junk}"
        );
        let h = headers(&[
            ("host", "10.54.12.2:8797"),
            ("origin", "http://evil.example"),
        ]);
        let p = peer("10.54.12.3:50000", "10.54.12.2:8797");
        let f = checkin(&app, "N0CCC", "Red", &player_key("N0CCC"));
        let r = checkin_post(State(app.clone()), ConnectInfo(p), h, Form(f))
            .await
            .unwrap_or_else(|e| panic!("{:#}", e.0));
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert_eq!(core.appended("game.checkin"), 1, "nothing else was signed");
    }

    #[tokio::test]
    async fn forms_arrive_with_the_scripts_fields() {
        let req = axum::http::Request::post("/checkin")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(
                "call=N0AAA&team=Red&site=a&session=s1&t=1700000000000&pk=ab&psig=cd",
            ))
            .unwrap();
        let Form(f) = Form::<CheckinForm>::from_request(req, &()).await.unwrap();
        assert_eq!((f.call.as_str(), f.site.as_str()), ("N0AAA", "a"));
        assert_eq!(
            (
                f.sig.session.as_str(),
                f.sig.t.as_str(),
                f.sig.psig.as_str()
            ),
            ("s1", "1700000000000", "cd")
        );
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

    async fn send(app: &Arc<App>, ip: &str, f: WordForm) -> String {
        text(word_post(State(app.clone()), from(ip), here(), Form(f)).await).await
    }

    fn word(app: &App, call: &str, target: &str, typed: &str) -> WordForm {
        let mut fields = json!({"call": call, "team": "Red", "word": typed});
        if let Some((k, v)) = target.split_once(':') {
            fields[k] = json!(v);
        }
        WordForm {
            call: call.into(),
            team: "Red".into(),
            target: target.into(),
            word: typed.into(),
            sig: sig(app, "game.word", fields, &player_key(call)),
        }
    }

    #[tokio::test]
    async fn code_words_are_taken_from_anywhere_and_wait_for_their_reveal() {
        let (app, core, _dir) = site().await;
        // across the mesh, not on any LAN
        let b = send(
            &app,
            "10.99.0.5",
            word(&app, "N0AAA", "site:park", "OTTER KITE 01"),
        )
        .await;
        assert!(
            b.contains("Logged. It counts if the word was right: waiting"),
            "{b}"
        );
        assert_eq!(core.appended("game.word"), 1);

        let b = send(
            &app,
            "10.99.0.6",
            word(&app, "N0AAA", "site:elsewhere", "OTTER KITE 01"),
        )
        .await;
        assert!(b.contains("Pick where you heard it"), "{b}");
        let b = send(&app, "10.99.0.7", word(&app, "N0AAA", "fox:Fox 1", "x")).await;
        assert!(b.contains("look like a code word"), "{b}");
        assert_eq!(core.appended("game.word"), 1);

        // a word that has been revealed already is refused before it is signed
        let now = ridgewarden_client::util::now_ms();
        let reveal =
            json!({"session": session(&app), "site": "park", "slot": 0, "word": "CEDAR MESA 77"});
        core.add("game.reveal", reveal, now);
        ridgewarden_client::follow(&app.core, &app.db, crate::PROJECTIONS)
            .await
            .unwrap();
        let b = send(
            &app,
            "10.99.0.8",
            word(&app, "N0AAA", "site:park", "CEDAR MESA 77"),
        )
        .await;
        assert!(b.contains("already been revealed"), "{b}");
        let b = send(
            &app,
            "10.99.0.9",
            word(&app, "HELLO", "site:park", "OTTER KITE 01"),
        )
        .await;
        assert!(b.contains("doesn't look like a callsign"), "{b}");
        assert_eq!(core.appended("game.word"), 1);
    }

    #[tokio::test]
    async fn a_fox_code_counts_once_and_a_wrong_one_is_not_logged() {
        let (app, core, _dir) = site().await;
        let b = send(
            &app,
            "10.99.0.5",
            word(&app, "N0AAA", "fox:Fox 1", "AAAA-BBBB-CCCX"),
        )
        .await;
        assert!(b.contains("isn't one of Fox 1's codes"), "{b}");
        assert_eq!(core.appended("game.word"), 0);
        let b = send(
            &app,
            "10.99.0.6",
            word(&app, "N0AAA", "fox:Fox 1", "AAAA BBBB CCCC"),
        )
        .await;
        assert!(b.contains("Counted: found Fox 1, 20 points"), "{b}");
        let b = send(
            &app,
            "10.99.0.7",
            word(&app, "N0BBB", "fox:Fox 1", "AAAA-BBBB-CCCC"),
        )
        .await;
        assert!(b.contains("that code was already used"), "{b}");
        assert_eq!(core.appended("game.word"), 2);
    }

    #[tokio::test]
    async fn bearings_and_spots_are_signed_and_shown() {
        let (app, core, _dir) = site().await;
        let bbb = player_key("N0BBB");
        let hunt = |deg: &str| {
            let fields = json!({"call": "N0BBB", "fox": "Fox 1", "lat": "35.00000",
                "lon": "-120.00000", "deg": deg});
            HuntForm {
                call: "N0BBB".into(),
                fox: "Fox 1".into(),
                lat: "35.00000".into(),
                lon: "-120.00000".into(),
                deg: deg.into(),
                sig: sig(&app, "game.bearing", fields, &bbb),
            }
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
                Form(hunt("45.0")),
            )
            .await,
        )
        .await;
        assert!(
            b.contains("45&deg;") && b.contains("35.0000, -120.0000"),
            "{b}"
        );
        assert_eq!(core.appended("game.bearing"), 1);
        let page = ok(hunt_page(State(app.clone())).await).0;
        assert!(
            page.contains("value=\"35.00000,-120.00000\">a</option>"),
            "{page}"
        );

        let spot = |text: &str| SpotForm {
            call: "N0BBB".into(),
            text: text.into(),
            sig: sig(
                &app,
                "game.spot",
                json!({"call": "N0BBB", "text": text}),
                &bbb,
            ),
        };
        let b = text(
            spot_post(
                State(app.clone()),
                from("10.99.0.7"),
                here(),
                Form(spot("<b>heading</b> to the park")),
            )
            .await,
        )
        .await;
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
        // 40 minutes in with 15-minute words: two slots over at the park; the fox has codes
        assert_eq!(words::reveal_due(&app).await.unwrap(), 2);
        assert_eq!(words::reveal_due(&app).await.unwrap(), 0);
        assert_eq!(core.appended("game.reveal"), 2);
    }

    /// A form's fields in page order, then the hidden ones the page adds.
    fn fields(pairs: &[(&str, &str)], session: &str, t: i64) -> Value {
        let t = t.to_string();
        let hidden = [
            ("session", session),
            ("t", t.as_str()),
            ("pk", ""),
            ("psig", ""),
        ];
        pairs
            .iter()
            .chain(&hidden)
            .map(|(k, v)| json!([k, v]))
            .collect()
    }

    /// What a browser running player.js posts for each request; see tests/sign.mjs.
    fn browser(requests: Value) -> Vec<Value> {
        let dir = env!("CARGO_MANIFEST_DIR");
        let mut node = std::process::Command::new("node")
            .arg(format!("{dir}/tests/sign.mjs"))
            .arg(format!("{dir}/src/player.js"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("these tests run src/player.js under Node, so `node` has to be on the PATH");
        let mut input = node.stdin.take().unwrap();
        std::io::Write::write_all(&mut input, requests.to_string().as_bytes()).unwrap();
        drop(input);
        let out = node.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    async fn post_spot(app: &Arc<App>, ip: &str, v: &Value) -> String {
        let f = serde_json::from_value(v.clone()).unwrap();
        text(spot_post(State(app.clone()), from(ip), here(), Form(f)).await).await
    }

    async fn post_hunt(app: &Arc<App>, ip: &str, v: &Value) -> String {
        let f = serde_json::from_value(v.clone()).unwrap();
        text(hunt_post(State(app.clone()), from(ip), here(), Form(f)).await).await
    }

    #[tokio::test]
    async fn posts_signed_by_the_pages_script_are_taken() {
        let (app, core, _dir) = site().await;
        let (s, t) = (session(&app), now_ms());
        let spot = |call: &str, text: &str| fields(&[("call", call), ("text", text)], &s, t);
        // the same passphrase typed in full-width characters makes the same key
        let wide = "\u{ff43}\u{ff4f}\u{ff52}\u{ff52}\u{ff45}\u{ff43}\u{ff54}\u{3000}\
                    \u{ff48}\u{ff4f}\u{ff52}\u{ff53}\u{ff45}\u{3000}\u{ff11}\u{ff12}";
        let posted = browser(json!([
            {"kind": "game.spot", "pass": "correct horse 12",
             "fields": spot(" n0aaa ", "  to Pe\u{f1}asco, \"north\" side \\ 5 min  ")},
            {"kind": "game.spot", "pass": wide, "fields": spot("N0AAA", "x")},
            {"kind": "game.spot", "pass": "short pass", "fields": spot("N0AAA", "x")},
            {"kind": "game.spot", "pass": "a wrong horse 99", "bound": "ab".repeat(32),
             "fields": spot("N0AAA", "x")},
            {"kind": "game.word", "pass": "battery staple 34", "fields": fields(&[
                ("call", "n0bbb"), ("team", "Red"), ("target", "fox:Fox 1"),
                ("word", "aaaa bbbb cccc")], &s, t)},
            {"kind": "game.bearing", "pass": "battery staple 34", "at": "35.3, -120.66",
             "fields": fields(&[("call", "N0BBB"), ("fox", "Fox 1"), ("deg", "45"),
                ("lat", ""), ("lon", "")], &s, t)},
            {"kind": "game.checkin", "pass": "third horse 56",
             "fields": fields(&[("call", "N0CCC"), ("team", "Blue"), ("site", "a")], &s, t)},
        ]));
        assert_eq!(posted[1]["pk"], posted[0]["pk"]);
        assert_eq!(
            posted[2]["error"],
            "Use a passphrase of at least 12 characters."
        );
        assert!(
            posted[3]["error"]
                .as_str()
                .is_some_and(|e| e.contains("doesn't make the key N0AAA plays with")),
            "{}",
            posted[3]
        );

        let b = post_spot(&app, "10.99.0.5", &posted[0]).await;
        assert!(
            b.contains("to Pe\u{f1}asco") && !b.contains("doesn't match"),
            "{b}"
        );
        assert_eq!(core.appended("game.spot"), 1);
        let word = serde_json::from_value(posted[4].clone()).unwrap();
        let b = send(&app, "10.99.0.6", word).await;
        assert!(b.contains("Counted: found Fox 1"), "{b}");
        let b = post_hunt(&app, "10.99.0.7", &posted[5]).await;
        assert!(
            b.contains("45&deg;") && b.contains("35.3000, -120.6600"),
            "{b}"
        );
        let checkin = serde_json::from_value(posted[6].clone()).unwrap();
        let b = post(&app, "10.54.12.3", None, checkin).await;
        assert!(b.contains("Counted: captured for Blue"), "{b}");
    }

    #[tokio::test]
    async fn the_server_refuses_what_the_script_signed_for_something_else() {
        let (app, core, _dir) = site().await;
        let (s, now) = (session(&app), now_ms());
        let pass = "correct horse 12";
        let spot = |session: &str, t: i64| {
            json!({"kind": "game.spot", "pass": pass,
                "fields": fields(&[("call", "N0AAA"), ("text", "here")], session, t)})
        };
        let hunt = |lat: &str, deg: &str| {
            json!({"kind": "game.bearing", "call": "N0AAA", "pass": pass, "raw": {"session": s,
                "t": now, "call": "N0AAA", "fox": "Fox 1", "lat": lat, "lon": "-120.66",
                "deg": deg}})
        };
        let posted = browser(json!([
            spot(&s, now - 31 * MIN),
            spot(&s, now + 2 * MIN),
            spot("another-game", now),
            {"kind": "game.checkin", "pass": pass,
             "fields": fields(&[("call", "N0AAA"), ("team", "Red"), ("site", "b")], &s, now)},
            spot(&s, now),
            hunt("35.3000000000001", "45"),
            hunt("35.3", "45.0000001"),
        ]));
        for (i, ip, says) in [
            (0, "10.99.0.1", "This page is too old"),
            (1, "10.99.0.2", "This page is too old"),
            (2, "10.99.0.3", "This page was for another game"),
        ] {
            let b = post_spot(&app, ip, &posted[i]).await;
            assert!(b.contains(says), "{i}: {b}");
        }
        let checkin = serde_json::from_value(posted[3].clone()).unwrap();
        let b = post(&app, "10.54.12.3", None, checkin).await;
        assert!(b.contains("This page was for another site"), "{b}");

        // a good signature is good only for the kind and spelling it was made with
        let v = &posted[4];
        let logged = json!({"call": v["call"], "text": v["text"], "session": s, "t": now,
            "pk": v["pk"], "psig": v["psig"]});
        assert_eq!(
            game::signer("game.spot", &logged).as_deref(),
            v["pk"].as_str()
        );
        for other in ["game.word", "game.bearing", "game.checkin"] {
            assert_eq!(game::signer(other, &logged), None, "{other}");
        }
        let mut shouted = v.clone();
        shouted["psig"] = json!(v["psig"].as_str().unwrap().to_uppercase());
        let b = post_spot(&app, "10.99.0.4", &shouted).await;
        assert!(b.contains("doesn't match"), "{b}");

        // fields longer than the page writes aren't logged, whoever signed them
        for (i, ip, says) in [
            (5, "10.99.0.5", "Pick a site, or type a latitude"),
            (6, "10.99.0.6", "The bearing is degrees"),
        ] {
            let mut v = posted[i].clone();
            v["t"] = json!(now.to_string());
            let b = post_hunt(&app, ip, &v).await;
            assert!(b.contains(says), "{i}: {b}");
        }
        for kind in ["game.spot", "game.checkin", "game.bearing"] {
            assert_eq!(core.appended(kind), 0, "{kind}");
        }
    }

    #[tokio::test]
    async fn a_roster_keeps_strangers_from_taking_callsigns() {
        let (app, core, _dir) = site().await;
        let s = session(&app);
        let teams = json!({"session": s, "teams": ["Red", "Blue"], "roster": {"N0AAA": "Red"}});
        core.add("game.teams", teams, now_ms());
        ridgewarden_client::follow(&app.core, &app.db, crate::PROJECTIONS)
            .await
            .unwrap();
        let t = now_ms();
        let spot = |call: &str, pass: &str| {
            json!({"kind": "game.spot", "pass": pass,
                "fields": fields(&[("call", call), ("text", "here")], &s, t)})
        };
        let posted = browser(json!([
            spot("N0VIC", "a stranger's pass"),
            spot("N0AAA", "correct horse 12"),
            spot("N0AAA", "another horse 34"),
        ]));
        let b = post_spot(&app, "10.99.0.1", &posted[0]).await;
        assert!(b.contains("N0VIC isn't on this game's roster"), "{b}");
        post_spot(&app, "10.99.0.2", &posted[1]).await;
        assert_eq!(core.appended("game.spot"), 1);
        let keys = game::keys(&app.db(), &app.cfg.admins).unwrap();
        assert_eq!(keys.get("N0VIC"), None);
        assert_eq!(
            keys.get("N0AAA").map(String::as_str),
            posted[1]["pk"].as_str()
        );
        let b = post_spot(&app, "10.99.0.3", &posted[2]).await;
        assert!(b.contains("N0AAA plays with another key"), "{b}");
        assert_eq!(core.appended("game.spot"), 1);
    }
}
