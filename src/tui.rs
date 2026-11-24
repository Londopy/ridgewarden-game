//! `ridgewarden-game top`: the game live in a terminal, for an operator on a headless Pi
//! over SSH: the scores, who holds each site and how long it can hold on, the plays as
//! they come in, the fox hunt and the players. It reads the app's own JSON API, the same
//! scores the page shows, so it needs none of the app's files and runs as any user.

use std::collections::BTreeMap;
use std::io;
use std::time::Duration;

use anyhow::Result;
use ridgewarden_client::tui::ratatui::Frame;
use ridgewarden_client::tui::ratatui::crossterm::event::{KeyCode, KeyEvent};
use ridgewarden_client::tui::ratatui::layout::{Constraint, Layout, Rect};
use ridgewarden_client::tui::ratatui::style::{Color as Ansi, Modifier, Style};
use ridgewarden_client::tui::ratatui::text::{Line, Span};
use ridgewarden_client::tui::ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use ridgewarden_client::tui::{
    self, Chrome, Fetch, Notice, Refresh, Theme, View, move_key, tab_key, word_pill, word_table,
};
use ridgewarden_client::ui::Color;
use ridgewarden_client::util::{human_duration, now_ms, short_time};

use crate::chains::Chain;
use crate::game::{Bearing, Fox, MIN, Target};
use crate::score::{Judged, Played, PlayerScore, chain_rate};
use crate::web::{Listed, PALETTE, team_colors};

const TABS: [&str; 5] = ["Scores", "Sites", "Plays", "Foxes", "Players"];
const HELP: &str = "q quit  1-5 Tab pages  Up Down PgUp PgDn move  Enter more  r reload";
const BACK: &str = "q quit  Esc back  Up Down next row  1-5 Tab pages  r reload";
/// Where the game's page answers on its own box, with the packaged config.
pub const URL: &str = "http://127.0.0.1:8797";
const NO_GAME: &str =
    "No game yet. Once an admin starts one with `ridgewarden-game start`, it shows here.";

/// Run `top` against the game's page at `url` until the user quits, reading it again every
/// `every`.
pub fn run(url: &str, every: Duration) -> Result<()> {
    let base = url.trim_end_matches('/').to_string();
    // the first view of a long game after a restart is worked out from the start, which
    // can take most of a minute on a Pi
    let fetch = Fetch::new(Duration::from_secs(60))?;
    let from = base.clone();
    let refresh = Refresh::spawn(every, async move || load(&fetch, &from).await)?;
    let mut top = Top::new(base, Theme::from_env(), refresh);
    tui::run(&mut top, Duration::from_millis(250))?;
    Ok(())
}

/// A good read of the game's page.
struct Snap {
    /// when it was read
    at: i64,
    /// None before the first game
    game: Option<Played>,
}

async fn load(fetch: &Fetch, base: &str) -> Result<Snap, String> {
    let game = read(fetch, base).await.map_err(|e| hint(e.to_string()))?;
    Ok(Snap { at: now_ms(), game })
}

/// A missing page or JSON this binary can't read usually means `top` and the app it reads
/// are different versions, so say so.
fn hint(e: String) -> String {
    if e.ends_with(": HTTP 404") || e.ends_with(": unexpected JSON") {
        format!("{e} (is the app there the same version of ridgewarden-game as this one?)")
    } else {
        e
    }
}

/// The game the page shows (the one running, else the latest), or None before the first.
async fn read(fetch: &Fetch, base: &str) -> Result<Option<Played>> {
    // the list tells "no game yet" apart from a page that isn't there
    let games: Vec<Listed> = fetch.json(&format!("{base}/api/games")).await?;
    if games.is_empty() {
        return Ok(None);
    }
    Ok(Some(fetch.json(&format!("{base}/api/game")).await?))
}

struct Top {
    base: String,
    theme: Theme,
    refresh: Refresh<Result<Snap, String>>,
    snap: Option<Snap>,
    /// why the latest read failed, if it did; the last good one stays on screen
    err: Option<String>,
    tab: usize,
    /// each page's selected row
    rows: [TableState; TABS.len()],
    /// whether the selected row's detail is open in place of its page
    detail: bool,
    /// rows the last list drawn had room for, for PgUp and PgDn
    page: usize,
}

impl Top {
    fn new(base: String, theme: Theme, refresh: Refresh<Result<Snap, String>>) -> Self {
        Self {
            base,
            theme,
            refresh,
            snap: None,
            err: None,
            tab: 0,
            rows: Default::default(),
            detail: false,
            page: 0,
        }
    }

    fn game(&self) -> Option<&Played> {
        self.snap.as_ref()?.game.as_ref()
    }

    /// What tells each row of a page apart, in the order shown.
    fn keys(&self, tab: usize) -> Vec<String> {
        let Some(p) = self.game() else {
            return vec![];
        };
        match tab {
            1 => p.session.all_sites(),
            2 => p.checkins.iter().rev().map(|c| c.id.clone()).collect(),
            3 => p.session.foxes.iter().map(|f| f.name.clone()).collect(),
            4 => p.players.iter().map(|x| x.call.clone()).collect(),
            _ => vec![],
        }
    }

    fn picked(&self, tab: usize) -> Option<String> {
        let i = self.rows[tab].selected()?;
        self.keys(tab).into_iter().nth(i)
    }

    /// Take a read. Each page's selection stays on the row it was on wherever that row has
    /// moved, since new plays come in at the top; a different game starts afresh.
    fn take(&mut self, read: Result<Snap, String>) {
        let s = match read {
            Ok(s) => s,
            Err(e) => {
                self.err = Some(e);
                return;
            }
        };
        let id = |g: Option<&Played>| g.map(|p| p.session.id.clone());
        let same = id(self.game()) == id(s.game.as_ref());
        let picked: Vec<Option<String>> = (0..TABS.len()).map(|t| self.picked(t)).collect();
        self.snap = Some(s);
        self.err = None;
        if !same {
            self.rows = Default::default();
            self.detail = false;
            return;
        }
        for (t, key) in picked.into_iter().enumerate() {
            let keys = self.keys(t);
            let at = key.and_then(|k| keys.iter().position(|x| *x == k));
            let kept = at.or(self.rows[t]
                .selected()
                .map(|i| i.min(keys.len().saturating_sub(1))));
            self.rows[t].select(kept.filter(|_| !keys.is_empty()));
        }
    }

    fn status(&self, now: i64) -> Span<'static> {
        let Some(s) = &self.snap else {
            return Span::styled("loading", self.theme.mute());
        };
        let age = format!("updated {} ago", human_duration(now - s.at));
        match &self.err {
            None => Span::styled(age, self.theme.mute()),
            Some(_) => Span::styled(format!("page not answering, {age}"), self.theme.bad()),
        }
    }

    fn render(&mut self, f: &mut Frame, now: i64) {
        let title = match self.game() {
            Some(p) => format!("ridgewarden-game top  {}", p.session.name),
            None => "ridgewarden-game top".into(),
        };
        let chrome = Chrome {
            title: &title,
            status: self.status(now),
            tabs: &TABS,
            tab: self.tab,
            help: if self.detail { BACK } else { HELP },
        };
        let Some(mut area) = chrome.draw(f, &self.theme) else {
            return;
        };
        let theme = &self.theme;
        if let (Some(e), Some(_)) = (&self.err, &self.snap) {
            let msg = format!("Reading failed, so this is from earlier: {e}");
            let lines = vec![Line::styled(msg, theme.bad())];
            let [banner, rest] = Layout::vertical([
                Constraint::Length(rows_for(&lines, area.width)),
                Constraint::Fill(1),
            ])
            .areas(area);
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), banner);
            area = rest;
        }
        let Some(s) = &self.snap else {
            let msg;
            let n = match &self.err {
                Some(e) => {
                    msg = format!(
                        "Can't read {e}. If ridgewarden-game isn't running, start it with: sudo systemctl start ridgewarden-game. If its page is on another address than {}, give that with --url.",
                        self.base
                    );
                    Notice::Error(&msg)
                }
                None => Notice::Loading,
            };
            f.render_widget(n.paragraph(theme), area);
            return;
        };
        let Some(p) = &s.game else {
            f.render_widget(Notice::Empty(NO_GAME).paragraph(theme), area);
            return;
        };
        // countdowns run on the app's clock, which may be another box's: its time when it
        // scored the game, plus however long ago that was read here
        let c = Ctx::new(theme, p, p.at + (now - s.at).max(0));
        let rows = &mut self.rows[self.tab];
        let open = rows.selected().filter(|_| self.detail);
        match (self.tab, open) {
            (0, _) => scores(&c, f, area),
            (1, None) => self.page = sites(&c, f, area, rows),
            (1, Some(i)) => site_detail(&c, f, area, i),
            (2, None) => self.page = plays(&c, f, area, rows),
            (2, Some(i)) => play_detail(&c, f, area, i),
            (3, None) => self.page = foxes(&c, f, area, rows),
            (3, Some(i)) => fox_detail(&c, f, area, i),
            (_, None) => self.page = players(&c, f, area, rows),
            (_, Some(i)) => player_detail(&c, f, area, i),
        }
    }
}

impl View for Top {
    fn tick(&mut self) -> io::Result<()> {
        if let Some(read) = self.refresh.latest()? {
            self.take(read);
        }
        Ok(())
    }

    fn key(&mut self, k: KeyEvent) -> bool {
        if let Some(t) = tab_key(&k, self.tab, TABS.len()) {
            self.tab = t;
            self.detail = false;
            return true;
        }
        let len = self.keys(self.tab).len();
        match k.code {
            KeyCode::Char('r') => self.refresh.again(),
            KeyCode::Esc | KeyCode::Backspace => self.detail = false,
            KeyCode::Enter if len > 0 => {
                let rows = &mut self.rows[self.tab];
                if rows.selected().is_none() {
                    rows.select(Some(0));
                }
                self.detail = !self.detail;
            }
            _ => {
                move_key(&mut self.rows[self.tab], &k, len, self.page);
            }
        }
        true
    }

    fn draw(&mut self, f: &mut Frame) {
        self.render(f, now_ms());
    }
}

/// What every page draws from.
struct Ctx<'a> {
    theme: &'a Theme,
    p: &'a Played,
    now: i64,
    teams: BTreeMap<String, Style>,
}

impl<'a> Ctx<'a> {
    fn new(theme: &'a Theme, p: &'a Played, now: i64) -> Self {
        Self {
            theme,
            p,
            now,
            teams: team_styles(theme, &p.session.teams),
        }
    }

    /// A team's name in its color.
    fn team(&self, t: &str) -> Span<'static> {
        let style = self.teams.get(t).copied().unwrap_or_default();
        Span::styled(format!(" {t} "), style)
    }

    fn mute(&self, s: impl Into<String>) -> Span<'static> {
        Span::styled(s.into(), self.theme.mute())
    }

    fn heading(&self, s: &'static str) -> Line<'static> {
        Line::styled(s, self.theme.heading())
    }

    fn over(&self) -> bool {
        self.p.session.end <= self.now
    }
}

/// Each team's style: the page's team color behind its name, as the nearest of the
/// terminal's own colors, or just bold where colors are off.
fn team_styles(theme: &Theme, teams: &[String]) -> BTreeMap<String, Style> {
    team_colors(teams)
        .into_iter()
        .map(|(t, hex)| (t, team_style(theme, hex)))
        .collect()
}

fn team_style(theme: &Theme, hex: &str) -> Style {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    if !theme.color {
        return bold;
    }
    let name = PALETTE
        .iter()
        .find(|(_, h)| *h == hex)
        .map_or("", |(n, _)| *n);
    let (fg, bg) = match name {
        "red" => (Ansi::White, Ansi::Red),
        "blue" => (Ansi::White, Ansi::Blue),
        "green" => (Ansi::Black, Ansi::Green),
        "orange" => (Ansi::Black, Ansi::Yellow),
        "teal" => (Ansi::Black, Ansi::Cyan),
        "magenta" => (Ansi::White, Ansi::Magenta),
        "brown" => (Ansi::White, Ansi::DarkGray),
        _ => (Ansi::Black, Ansi::Gray),
    };
    bold.fg(fg).bg(bg)
}

/// The page's pill for a play: the points it earned, waiting for its code word to be
/// revealed, or not counted.
fn result(j: &Judged) -> (Color, String) {
    match (j.counted, j.pending) {
        (true, _) => (Color::Green, format!("+{}", j.points)),
        (false, true) => (Color::Unknown, "waiting".into()),
        (false, false) => (Color::Yellow, "not counted".into()),
    }
}

/// "14:32", in UTC like the page.
fn hm(ms: i64) -> String {
    short_time(ms)[11..16].to_string()
}

/// "1 capture", "2 captures".
fn count(n: impl Into<u64>, one: &str, many: &str) -> String {
    match n.into() {
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    }
}

/// "24 min left", rounded up to the minute as a countdown is, or "any moment" once it's due.
fn left(ms: i64) -> String {
    if ms > 0 {
        format!("{} left", human_duration((ms + MIN - 1) / MIN * MIN))
    } else {
        "any moment".into()
    }
}

fn right(s: impl ToString) -> Cell<'static> {
    Cell::from(Line::from(s.to_string()).right_aligned())
}

/// Rows `lines` take wrapped to `width`, with one to spare where words move down a row.
fn rows_for(lines: &[Line], width: u16) -> u16 {
    let w = width.max(1);
    let n: u16 = lines
        .iter()
        .map(|l| (l.width() as u16).div_ceil(w).max(1))
        .sum();
    if n > lines.len() as u16 { n + 1 } else { n }
}

/// Lines of text over a table, wrapped, and the area left under them.
fn above(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) -> Rect {
    let [head, _, rest] = Layout::vertical([
        Constraint::Length(rows_for(&lines, area.width)),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), head);
    rest
}

/// The page's line about the game's time.
fn game_line(c: &Ctx) -> Line<'static> {
    let s = &c.p.session;
    let (state, text) = if c.over() {
        (
            word_pill(c.theme, "OVER", Color::Unknown),
            format!(
                "Ran {} to {} ({}). Final scores.",
                short_time(s.start),
                short_time(s.end),
                human_duration(s.end - s.start)
            ),
        )
    } else {
        (
            word_pill(c.theme, "LIVE", Color::Green),
            format!(
                "Started {}, ends {} ({}).",
                short_time(s.start),
                short_time(s.end),
                left(s.end - c.now)
            ),
        )
    };
    Line::from(vec![state, Span::raw(" "), Span::raw(text)])
}

fn scores(c: &Ctx, f: &mut Frame, area: Rect) {
    let chains: Vec<Line> = if c.p.chains.is_empty() {
        vec![Line::from(c.mute(
            "None right now: a chain needs two of a team's sites joined by links that are up.",
        ))]
    } else {
        c.p.chains.iter().map(|ch| chain_line(c, ch)).collect()
    };
    let [
        time,
        _,
        board,
        _,
        chains_head,
        chains_list,
        _,
        plays_head,
        latest,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(c.p.teams.len() as u16 + 1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(rows_for(&chains, area.width)),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    f.render_widget(game_line(c), time);
    f.render_widget(board_table(c), board);
    f.render_widget(c.heading("Chains now"), chains_head);
    let chains = Paragraph::new(chains).wrap(Wrap { trim: false });
    f.render_widget(chains, chains_list);
    f.render_widget(c.heading("Latest plays"), plays_head);
    if c.p.checkins.is_empty() {
        f.render_widget(
            Line::from(c.mute("No check-ins or code words yet.")),
            latest,
        );
    } else {
        let all = c.p.checkins.iter().rev();
        draw_plays(c, f, latest, all, &mut TableState::default());
    }
}

/// The page's scoreboard.
fn board_table(c: &Ctx) -> Table<'static> {
    let rows = c.p.teams.iter().map(|t| {
        Row::new([
            Cell::from(c.team(&t.team)),
            right(t.total),
            right(format!("{}/min", t.rate)),
            right(t.chain),
            right(t.builder),
            right(t.capture),
            right(t.encounter),
            right(t.fox),
            right(t.held),
        ])
    });
    let header = Row::new([
        Cell::from("Team"),
        right("Points"),
        right("Now"),
        right("Chains"),
        right("New links"),
        right("Captures"),
        right("Encounters"),
        right("Foxes"),
        right("Held"),
    ])
    .style(c.theme.heading());
    let widths = [12, 6, 6, 6, 9, 8, 10, 5, 4].map(Constraint::Length);
    Table::new(rows, widths).header(header)
}

/// A chain's sites and what it earns, as the page lists them.
fn chain_text(c: &Ctx, ch: &Chain) -> String {
    let (chain, builder) = chain_rate(&c.p.session.rules, ch);
    let sites: Vec<&str> = ch.sites.iter().map(String::as_str).collect();
    let new = match ch.new_links.len() {
        0 => String::new(),
        1 => ", 1 new link".into(),
        n => format!(", {n} new links"),
    };
    format!(
        "{} ({} sites, {} points a minute{new})",
        sites.join(" - "),
        ch.sites.len(),
        chain + builder
    )
}

fn chain_line(c: &Ctx, ch: &Chain) -> Line<'static> {
    Line::from(vec![
        c.team(&ch.team),
        Span::raw(format!(" {}", chain_text(c, ch))),
    ])
}

/// Where a play was aimed, as the page says it.
fn target(j: &Judged) -> String {
    match &j.target {
        Target::Site(n) if j.by_word => format!("{n} (word)"),
        Target::Site(n) => n.clone(),
        Target::Fox(f) => format!("fox {f}"),
    }
}

/// Plays as the page lists them, each led by its result.
fn draw_plays<'a>(
    c: &Ctx,
    f: &mut Frame,
    area: Rect,
    list: impl Iterator<Item = &'a Judged>,
    rows: &mut TableState,
) {
    let list: Vec<&Judged> = list.collect();
    let results: Vec<(Color, String)> = list.iter().map(|&j| result(j)).collect();
    let items = list.iter().zip(&results).map(|(j, (color, word))| {
        let cells = vec![
            Cell::from(hm(j.ts)),
            Cell::from(j.call.clone()),
            Cell::from(c.team(&j.team)),
            Cell::from(target(j)),
            Cell::from(j.note.clone()),
        ];
        (Some((word.as_str(), *color)), cells)
    });
    // tight, so the note, which says what happened, keeps as much room as it can
    let widths = [
        Constraint::Length(5),
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(16),
        Constraint::Fill(1),
    ];
    let header = ["UTC", "Callsign", "Team", "Where", "Note"];
    let table = word_table(c.theme, &header, &widths, items);
    f.render_stateful_widget(table, area, rows);
}

/// How holding a site stands now.
#[derive(Debug, PartialEq)]
enum Hold {
    /// held, with a link up at it
    Linked,
    /// held with every link at it down: it goes back to nobody at this time unless one
    /// comes up first
    Until(i64),
    /// held by nobody since this time, when its links stayed down
    Lost(i64),
    /// a LAN check-in site no core polls over its LAN, so nobody can take it
    Dead,
}

/// None for a site nobody has taken yet, and for a held one once the game is over.
fn hold(p: &Played, site: &str, now: i64) -> Option<Hold> {
    if p.holders.contains_key(site) {
        if p.session.end <= now {
            return None;
        }
        if p.up.iter().any(|(a, b)| a == site || b == site) {
            return Some(Hold::Linked);
        }
        let since = p.linked.get(site).copied().unwrap_or(p.at);
        return Some(Hold::Until(since + p.session.rules.hold_min * MIN));
    }
    if p.dead_sites.iter().any(|n| n == site) {
        return Some(Hold::Dead);
    }
    p.lost
        .iter()
        .rev()
        .find(|l| l.site == site)
        .map(|l| Hold::Lost(l.ts))
}

fn hold_cell(c: &Ctx, h: Option<Hold>) -> Cell<'static> {
    match h {
        None => Cell::from(""),
        Some(Hold::Linked) => Cell::from("links up"),
        Some(Hold::Until(t)) => Cell::from(word_pill(c.theme, &left(t - c.now), Color::Yellow)),
        Some(Hold::Lost(t)) => Cell::from(c.mute(format!("lost at {}", hm(t)))),
        Some(Hold::Dead) => Cell::from(Span::styled("no core on its LAN", c.theme.bad())),
    }
}

/// "LAN", "code word", or both.
fn taken_by(c: &Ctx, site: &str) -> &'static str {
    let s = &c.p.session;
    let lan = s.sites.iter().any(|n| n == site);
    match (lan, s.word_sites.iter().any(|n| n == site)) {
        (true, true) => "LAN, word",
        (true, false) => "LAN",
        _ => "code word",
    }
}

/// The chain a site is in, if anyone holds it: "3 sites, 3/min", or "alone".
fn chain_of(c: &Ctx, site: &str) -> String {
    if !c.p.holders.contains_key(site) {
        return String::new();
    }
    match c.p.chains.iter().find(|ch| ch.sites.contains(site)) {
        Some(ch) => {
            let (chain, builder) = chain_rate(&c.p.session.rules, ch);
            format!("{} sites, {}/min", ch.sites.len(), chain + builder)
        }
        None => "alone".into(),
    }
}

/// Rows a table with a header line shows in `area`.
fn fits(area: Rect) -> usize {
    area.height.saturating_sub(1).into()
}

fn sites(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) -> usize {
    let list = c.p.session.all_sites();
    if list.is_empty() {
        let none = Notice::Empty("This game has no sites, only foxes.");
        f.render_widget(none.paragraph(c.theme), area);
        return 0;
    }
    let note = format!(
        "A held site goes back to nobody once every link at it has been down for {} min.",
        c.p.session.rules.hold_min
    );
    let area = above(f, area, vec![Line::from(c.mute(note))]);
    let items = list.iter().map(|n| {
        let held = match c.p.holders.get(n) {
            Some(t) => c.team(t),
            None => c.mute("nobody"),
        };
        Row::new([
            Cell::from(n.clone()),
            Cell::from(taken_by(c, n)),
            Cell::from(held),
            Cell::from(chain_of(c, n)),
            hold_cell(c, hold(c.p, n, c.now)),
        ])
    });
    let header =
        Row::new(["Site", "Taken by", "Held by", "Chain", "Hold"]).style(c.theme.heading());
    let widths = [
        Constraint::Length(16),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(15),
        Constraint::Fill(1),
    ];
    let table = Table::new(items, widths)
        .header(header)
        .row_highlight_style(c.theme.selected());
    f.render_stateful_widget(table, area, rows);
    fits(area)
}

/// Who holds a site and how that stands, in words.
fn held_line(c: &Ctx, site: &str) -> Line<'static> {
    let by = |verb: &str, team: &str, rest: String| {
        Line::from(vec![
            Span::raw(verb.to_string()),
            c.team(team),
            Span::raw(rest),
        ])
    };
    match (c.p.holders.get(site), hold(c.p, site, c.now)) {
        (Some(t), Some(Hold::Until(at))) => by(
            "Held by",
            t,
            format!(
                "with every link at it down: back to nobody at {} UTC ({}) unless one comes up.",
                hm(at),
                left(at - c.now)
            ),
        ),
        (Some(t), Some(_)) => by("Held by", t, "with a link up.".into()),
        (Some(t), None) => by("Held by", t, "at the end.".into()),
        (None, Some(Hold::Dead)) => {
            Line::from("Nobody can take it: no core polls this node over its LAN.")
        }
        (None, Some(Hold::Lost(at))) => {
            let team = c.p.lost.iter().rev().find(|l| l.site == site);
            let team = team.map_or("", |l| l.team.as_str());
            by(
                "Lost by",
                team,
                format!("at {} UTC when its links stayed down.", hm(at)),
            )
        }
        (None, _) => Line::from("Nobody has taken it yet."),
    }
}

/// The site's links the page knows of, up or down.
fn links_line(c: &Ctx, site: &str) -> Line<'static> {
    let links: Vec<String> =
        c.p.known
            .iter()
            .filter_map(|e| {
                let other = match (e.0 == site, e.1 == site) {
                    (true, _) => &e.1,
                    (_, true) => &e.0,
                    _ => return None,
                };
                let state = if c.p.up.contains(e) { "up" } else { "down" };
                Some(format!("{other} {state}"))
            })
            .collect();
    if links.is_empty() {
        return Line::from(c.mute("No links seen at it lately."));
    }
    Line::from(format!("Links: {}.", links.join(", ")))
}

fn site_detail(c: &Ctx, f: &mut Frame, area: Rect, i: usize) {
    let list = c.p.session.all_sites();
    let Some(site) = list.get(i) else {
        return;
    };
    let how = match taken_by(c, site) {
        "LAN" => "a check-in site, taken on its LAN",
        "code word" => "a public site, taken with the code word heard on air there",
        _ => "taken on its LAN or with the code word heard on air there",
    };
    let chain = match c.p.chains.iter().find(|ch| ch.sites.contains(site)) {
        Some(ch) => Line::from(vec![
            c.team(&ch.team),
            Span::raw(format!("chain: {}", chain_text(c, ch))),
        ]),
        None => Line::from(c.mute("In no chain right now.")),
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(site.clone(), c.theme.heading()),
            Span::raw(format!(": {how}.")),
        ]),
        held_line(c, site),
        chain,
        links_line(c, site),
    ];
    let area = above(f, area, lines);
    let here: Vec<&Judged> =
        c.p.checkins
            .iter()
            .rev()
            .filter(|j| matches!(&j.target, Target::Site(n) if n == site))
            .collect();
    titled_plays(c, f, area, "Plays here, newest first", here);
}

/// A heading and a table of plays under it, or a note that there are none.
fn titled_plays(c: &Ctx, f: &mut Frame, area: Rect, title: &'static str, list: Vec<&Judged>) {
    let [head, rest] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
    f.render_widget(c.heading(title), head);
    if list.is_empty() {
        f.render_widget(Line::from(c.mute("None yet.")), rest);
    } else {
        draw_plays(c, f, rest, list.into_iter(), &mut TableState::default());
    }
}

fn plays(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) -> usize {
    if c.p.checkins.is_empty() {
        let none = Notice::Empty("No check-ins or code words yet.");
        f.render_widget(none.paragraph(c.theme), area);
        return 0;
    }
    draw_plays(c, f, area, c.p.checkins.iter().rev(), rows);
    fits(area)
}

fn play_detail(c: &Ctx, f: &mut Frame, area: Rect, i: usize) {
    let Some(j) = c.p.checkins.iter().rev().nth(i) else {
        return;
    };
    let how = match &j.target {
        Target::Fox(fox) => format!("With one of {fox}'s one-time codes."),
        Target::Site(_) if j.by_word => "With the code word heard on air there.".into(),
        Target::Site(_) => "A check-in on the site's LAN, countersigned by its core.".into(),
    };
    let (color, word) = result(j);
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} {}", short_time(j.ts), j.call),
                c.theme.heading(),
            ),
            Span::raw(" for"),
            c.team(&j.team),
            Span::raw(format!("at {}", target(j))),
        ]),
        Line::from(how),
        Line::from(""),
        Line::from(vec![
            word_pill(c.theme, &word, color),
            Span::raw(format!(" {}", j.note)),
        ]),
        Line::from(""),
        Line::from(c.mute(format!("Event {}", j.id))),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// The plays that found a fox, oldest first.
fn finds<'a>(p: &'a Played, fox: &'a str) -> impl Iterator<Item = &'a Judged> {
    p.checkins
        .iter()
        .filter(move |j| j.counted && matches!(&j.target, Target::Fox(f) if f == fox))
}

/// Where a fox was, once an admin has said.
fn fox_at(c: &Ctx, x: &Fox) -> Span<'static> {
    match x.at {
        Some(a) => Span::raw(format!("{:.4}, {:.4}", a.lat, a.lon)),
        None => c.mute("once an admin ends the game"),
    }
}

fn bearings_table<'a>(c: &Ctx, list: impl Iterator<Item = &'a Bearing>) -> Table<'static> {
    let rows = list.map(|b| {
        Row::new([
            Cell::from(hm(b.ts)),
            Cell::from(b.call.clone()),
            Cell::from(b.fox.clone()),
            Cell::from(format!("{:.4}, {:.4}", b.from.lat, b.from.lon)),
            right(format!("{:.0} deg", b.deg)),
        ])
    });
    let header = Row::new([
        Cell::from("UTC"),
        Cell::from("Callsign"),
        Cell::from("Fox"),
        Cell::from("From"),
        right("Bearing"),
    ])
    .style(c.theme.heading());
    let widths = [5, 10, 12, 19, 8].map(Constraint::Length);
    Table::new(rows, widths).header(header)
}

fn foxes(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) -> usize {
    let p = c.p;
    if p.session.foxes.is_empty() {
        let none = Notice::Empty("No foxes in this game.");
        f.render_widget(none.paragraph(c.theme), area);
        return 0;
    }
    let [list, _, head, bearings] = Layout::vertical([
        Constraint::Length(p.session.foxes.len().min(8) as u16 + 1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    let items = p.session.foxes.iter().map(|x| {
        let found = finds(p, &x.name).count();
        Row::new([
            Cell::from(x.name.clone()),
            Cell::from(x.call.clone().unwrap_or_default()),
            right(found),
            right(x.codes.len().saturating_sub(found)),
            right(p.bearings.iter().filter(|b| b.fox == x.name).count()),
            Cell::from(fox_at(c, x)),
        ])
    });
    let header = Row::new([
        Cell::from("Fox"),
        Cell::from("Run by"),
        right("Finds"),
        right("Codes left"),
        right("Bearings"),
        Cell::from("Where it was"),
    ])
    .style(c.theme.heading());
    let widths = [
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Fill(1),
    ];
    let table = Table::new(items, widths)
        .header(header)
        .row_highlight_style(c.theme.selected());
    f.render_stateful_widget(table, list, rows);
    f.render_widget(c.heading("Latest bearings"), head);
    if p.bearings.is_empty() {
        let none = "None yet. Hunters share them on the hunt page (/hunt).";
        f.render_widget(Line::from(c.mute(none)), bearings);
    } else {
        f.render_widget(bearings_table(c, p.bearings.iter().rev()), bearings);
    }
    fits(list)
}

fn fox_detail(c: &Ctx, f: &mut Frame, area: Rect, i: usize) {
    let Some(x) = c.p.session.foxes.get(i) else {
        return;
    };
    let found: Vec<&Judged> = finds(c.p, &x.name).collect();
    let run_by = match &x.call {
        Some(call) => format!(", run by {call}"),
        None => String::new(),
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(x.name.clone(), c.theme.heading()),
            Span::raw(run_by),
        ]),
        Line::from(format!(
            "Found by {}; {} of its {} one-time codes are left.",
            count(found.len() as u64, "hunter", "hunters"),
            x.codes.len().saturating_sub(found.len()),
            x.codes.len()
        )),
        Line::from(vec![Span::raw("Where it was: "), fox_at(c, x)]),
    ];
    let area = above(f, area, lines);
    let shown = found.len().min(6);
    let more = found.len() - shown;
    // the heading and the table's own header, then the finders
    let [finders, rest_of_them, _, rest] = Layout::vertical([
        Constraint::Length(shown as u16 + 2),
        Constraint::Length(u16::from(more > 0)),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    titled_plays(c, f, finders, "Found by", found[..shown].to_vec());
    if more > 0 {
        let others = c.mute(format!("and {more} more"));
        f.render_widget(Line::from(others), rest_of_them);
    }
    let [head, list] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(rest);
    f.render_widget(c.heading("Bearings on it, newest first"), head);
    let mine = c.p.bearings.iter().rev().filter(|b| b.fox == x.name);
    f.render_widget(bearings_table(c, mine), list);
}

/// The page's note on a player's impossible hops.
fn strikes(c: &Ctx, x: &PlayerScore) -> String {
    match x.strikes {
        0 => String::new(),
        n if n >= c.p.session.rules.strikes => "out: too many impossible hops".into(),
        1 => "1 impossible hop".into(),
        n => format!("{n} impossible hops"),
    }
}

fn players(c: &Ctx, f: &mut Frame, area: Rect, rows: &mut TableState) -> usize {
    if c.p.players.is_empty() {
        let none = Notice::Empty("Nobody has checked in yet.");
        f.render_widget(none.paragraph(c.theme), area);
        return 0;
    }
    let items = c.p.players.iter().map(|x| {
        // someone who only runs a fox has no team
        let (team, note) = if x.team.is_empty() {
            (Span::raw(""), c.mute("runs a fox"))
        } else {
            (c.team(&x.team), Span::raw(strikes(c, x)))
        };
        Row::new([
            Cell::from(x.call.clone()),
            Cell::from(team),
            right(x.points),
            right(x.captures),
            right(x.encounters),
            right(x.finds),
            Cell::from(note),
        ])
    });
    let header = Row::new([
        Cell::from("Callsign"),
        Cell::from("Team"),
        right("Points"),
        right("Captures"),
        right("Encounters"),
        right("Foxes"),
        Cell::from("Notes"),
    ])
    .style(c.theme.heading());
    let widths = [
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Fill(1),
    ];
    let table = Table::new(items, widths)
        .header(header)
        .row_highlight_style(c.theme.selected());
    f.render_stateful_widget(table, area, rows);
    fits(area)
}

fn player_detail(c: &Ctx, f: &mut Frame, area: Rect, i: usize) {
    let Some(x) = c.p.players.get(i) else {
        return;
    };
    let sites: Vec<&str> = x.sites.iter().map(String::as_str).collect();
    let call = Span::styled(x.call.clone(), c.theme.heading());
    let who = if x.team.is_empty() {
        Line::from(vec![
            call,
            Span::raw(format!(" runs a fox: {} points.", x.points)),
        ])
    } else {
        Line::from(vec![
            call,
            Span::raw(" plays for"),
            c.team(&x.team),
            Span::raw(format!("with {} points.", x.points)),
        ])
    };
    let mut lines = vec![
        who,
        Line::from(format!(
            "{}, {} and {}.",
            count(x.captures, "capture", "captures"),
            count(x.encounters, "encounter", "encounters"),
            count(x.finds, "fox found", "foxes found")
        )),
        Line::from(if sites.is_empty() {
            "Counted at no site yet.".to_string()
        } else {
            format!("Counted at {}.", sites.join(", "))
        }),
    ];
    let note = strikes(c, x);
    if !note.is_empty() {
        lines.push(Line::styled(format!("{note}."), c.theme.bad()));
    }
    let area = above(f, area, lines);
    let theirs: Vec<&Judged> =
        c.p.checkins
            .iter()
            .rev()
            .filter(|j| j.call == x.call)
            .collect();
    titled_plays(c, f, area, "Plays, newest first", theirs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use ridgewarden_client::tui::MIN_SIZE;
    use ridgewarden_client::tui::ratatui::Terminal;
    use ridgewarden_client::tui::ratatui::backend::TestBackend;
    use ridgewarden_client::tui::ratatui::buffer::Buffer;
    use ridgewarden_client::tui::ratatui::crossterm::event::KeyModifiers;
    use serde_json::json;

    use crate::config::{Config, EXAMPLE};
    use crate::game::DAY;
    use crate::testing::{FakeCore, G, T0};
    use crate::web::{Peer, router};
    use crate::{App, PROJECTIONS};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A view whose first read, made at `at`, found `game`.
    fn top(game: Option<Played>, at: i64) -> Top {
        let refresh = Refresh::spawn(Duration::from_secs(3600), async || {
            Err("not read in this test".to_string())
        })
        .unwrap();
        let mut t = Top::new(URL.into(), Theme { color: false }, refresh);
        t.take(Ok(Snap { at, game }));
        t
    }

    fn text(b: &Buffer) -> String {
        let rows = (0..b.area.height).map(|y| {
            let row: String = (0..b.area.width).map(|x| b[(x, y)].symbol()).collect();
            row.trim_end().to_string()
        });
        rows.collect::<Vec<_>>().join("\n")
    }

    /// A `w` by `h` terminal after one draw at `now`.
    fn frame(t: &mut Top, w: u16, h: u16, now: i64) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| t.render(f, now)).unwrap();
        term.backend().buffer().clone()
    }

    fn screen(t: &mut Top, w: u16, h: u16, now: i64) -> String {
        text(&frame(t, w, h, now))
    }

    /// Red holds a and b, joined through the relay r. Blue holds c, whose only link went
    /// down after +21. N0CCC's hop from c to a came far too fast.
    fn game(now: i64) -> Played {
        let mut g = G::new();
        g.polls(T0 - 10 * MIN, T0 + 60 * MIN, "a", &["r"]);
        g.polls(T0 - 10 * MIN, T0 + 60 * MIN, "r", &["b"]);
        g.polls(T0 - 10 * MIN, T0 + 20 * MIN, "c", &["r"]);
        g.polls(T0 + 21 * MIN, T0 + 60 * MIN, "c", &[]);
        let s = g.start(T0, json!({"minutes": 120}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + MIN, &s, "N0BBB", "Red", "b");
        g.checkin(T0 + 2 * MIN, &s, "N0CCC", "Blue", "c");
        g.checkin(T0 + 3 * MIN, &s, "N0CCC", "Blue", "a");
        g.played(now)
    }

    #[test]
    fn sites_show_who_holds_them_and_how_long_they_can_hold_on() {
        let now = T0 + 30 * MIN;
        let p = game(now);
        assert_eq!(hold(&p, "a", now), Some(Hold::Linked));
        // c's link was last up at +21, and a site holds on for 30 minutes after that
        assert_eq!(hold(&p, "c", now), Some(Hold::Until(T0 + 51 * MIN)));
        assert_eq!(left(21 * MIN), "21 min left");
        assert_eq!(left(0), "any moment");
        let theme = Theme { color: false };
        let c = Ctx::new(&theme, &p, now);
        assert_eq!(chain_of(&c, "a"), "2 sites, 2/min");
        assert_eq!(chain_of(&c, "c"), "alone");
        assert_eq!(taken_by(&c, "c"), "LAN");
        assert_eq!(
            hold(&p, "a", p.session.end),
            None,
            "nothing counts down after the end"
        );

        let later = T0 + 52 * MIN;
        assert_eq!(
            hold(&game(later), "c", later),
            Some(Hold::Lost(T0 + 51 * MIN))
        );

        let mut g = G::new();
        let s = g.start(T0, json!({"sites": ["a", "d"]}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        let p = g.played(T0 + 5 * MIN);
        assert_eq!(
            hold(&p, "d", T0 + 5 * MIN),
            Some(Hold::Dead),
            "no core polls d over its LAN"
        );
        assert_eq!(hold(&p, "b", T0 + 5 * MIN), None, "not taken yet");
    }

    #[test]
    fn plays_and_teams_wear_the_pages_colors() {
        let p = game(T0 + 30 * MIN);
        let results: Vec<(Color, String)> = p.checkins.iter().map(result).collect();
        assert_eq!(results[0], (Color::Green, "+10".into()));
        assert_eq!(results[3], (Color::Yellow, "not counted".into()));
        let mut waiting = p.checkins[0].clone();
        (waiting.counted, waiting.pending) = (false, true);
        assert_eq!(result(&waiting), (Color::Unknown, "waiting".into()));

        let teams = ["Blue".to_string(), "Hilltoppers".into(), "Red".into()];
        let colored = team_styles(&Theme { color: true }, &teams);
        assert_eq!(colored["Red"].bg, Some(Ansi::Red));
        assert_eq!(colored["Blue"].bg, Some(Ansi::Blue));
        assert_eq!(
            colored["Hilltoppers"].bg,
            Some(Ansi::Green),
            "the first color left, as on the page"
        );
        for s in team_styles(&Theme { color: false }, &teams).values() {
            assert_eq!((s.fg, s.bg), (None, None));
            assert!(s.add_modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn keys_switch_pages_and_open_the_selected_row() {
        let now = T0 + 30 * MIN;
        let mut t = top(Some(game(now)), now);
        assert!(t.key(key(KeyCode::Enter)));
        assert!(!t.detail, "the scores page has no rows to open");
        t.key(key(KeyCode::Char('3')));
        assert_eq!(t.tab, 2);
        t.key(key(KeyCode::Enter));
        assert_eq!(
            (t.rows[2].selected(), t.detail),
            (Some(0), true),
            "Enter opens the first row"
        );
        let detail = screen(&mut t, 80, 24, now);
        assert!(detail.contains("too fast"), "the newest play: {detail}");
        assert_eq!(
            detail.lines().last(),
            Some(BACK),
            "the help line says how back out"
        );
        t.key(key(KeyCode::Down));
        assert!(t.detail, "Up and Down move to the next row's detail");
        assert!(screen(&mut t, 80, 24, now).contains("captured for Blue"));
        t.key(key(KeyCode::Esc));
        assert!(!t.detail);
        assert_eq!(screen(&mut t, 80, 24, now).lines().last(), Some(HELP));
        t.key(key(KeyCode::Enter));
        t.key(key(KeyCode::Backspace));
        assert!(!t.detail, "Backspace backs out too");
        t.key(key(KeyCode::Enter));
        t.key(key(KeyCode::Tab));
        assert_eq!(
            (t.tab, t.detail),
            (3, false),
            "a new page starts on its list"
        );
    }

    /// Tick `t` until its latest read is the one made at `at`.
    fn until_read(t: &mut Top, at: i64) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while t.snap.as_ref().map(|s| s.at) != Some(at) {
            assert!(std::time::Instant::now() < deadline, "read {at} never came");
            t.tick().unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn r_reads_again_at_once() {
        let mut n = 0;
        let refresh = Refresh::spawn(Duration::from_secs(3600), async move || {
            n += 1;
            Ok(Snap { at: n, game: None })
        })
        .unwrap();
        let mut t = Top::new(URL.into(), Theme { color: false }, refresh);
        until_read(&mut t, 1);
        assert!(t.key(key(KeyCode::Char('r'))));
        until_read(&mut t, 2);
    }

    #[test]
    fn page_keys_move_by_the_rows_the_list_shows() {
        let now = T0 + 40 * MIN;
        let mut g = G::new();
        let s = g.start(T0, json!({"minutes": 120}));
        for m in 1..=30 {
            g.checkin(T0 + m * MIN, &s, "N0AAA", "Red", "a");
        }
        let mut t = top(Some(g.played(now)), now);
        t.key(key(KeyCode::Char('3')));
        let plays = screen(&mut t, 80, 24, now);
        // 21 rows under the title, tabs and help line, less the table's header
        assert_eq!(t.page, 20, "{plays}");
        let mut go = |code| {
            t.key(key(code));
            t.rows[2].selected()
        };
        assert_eq!(go(KeyCode::PageDown), Some(19), "the end of the first page");
        assert_eq!(
            go(KeyCode::PageDown),
            Some(29),
            "no further than the last play"
        );
        assert_eq!(go(KeyCode::PageUp), Some(9));
    }

    #[test]
    fn the_plays_list_leads_with_each_result() {
        let now = T0 + 30 * MIN;
        let mut t = top(Some(game(now)), now);
        t.key(key(KeyCode::Char('3')));
        let plays = screen(&mut t, 80, 24, now);
        assert_eq!(
            row(&plays, "State"),
            ["State", "UTC", "Callsign", "Team", "Where", "Note"]
        );
        assert_eq!(
            row(&plays, "not counted")[..6],
            ["not", "counted", "00:03", "N0CCC", "Blue", "a"],
            "{plays}"
        );
        let counted = plays.lines().filter(|l| l.trim_start().starts_with("+10 "));
        assert_eq!(counted.count(), 3, "{plays}");
    }

    #[test]
    fn a_missing_page_or_strange_json_hints_at_another_version() {
        let skew = hint(format!("{URL}/api/games: HTTP 404"));
        assert!(
            skew.ends_with("(is the app there the same version of ridgewarden-game as this one?)")
        );
        assert!(hint(format!("{URL}/api/game: unexpected JSON")).contains("same version"));
        let down = format!("{URL}/api/games: could not connect");
        assert_eq!(hint(down.clone()), down);
    }

    #[test]
    fn the_selection_stays_on_its_row_as_new_plays_come_in() {
        let now = T0 + 30 * MIN;
        let mut g = G::new();
        let s = g.start(T0, json!({"minutes": 120}));
        g.checkin(T0 + MIN, &s, "N0AAA", "Red", "a");
        g.checkin(T0 + 2 * MIN, &s, "N0BBB", "Blue", "b");
        let mut t = top(Some(g.played(now)), now);
        t.tab = 2;
        t.rows[2].select(Some(1));
        let picked = t.picked(2);
        assert_eq!(picked.as_deref(), t.keys(2).get(1).map(String::as_str));
        g.checkin(T0 + 20 * MIN, &s, "N0CCC", "Red", "c");
        t.take(Ok(Snap {
            at: now,
            game: Some(g.played(now)),
        }));
        assert_eq!(t.rows[2].selected(), Some(2), "one newer play above it now");
        assert_eq!(t.picked(2), picked);

        t.detail = true;
        let mut other = G::new();
        other.start(T0 + DAY, json!({"name": "Next week"}));
        t.take(Ok(Snap {
            at: now,
            game: Some(other.played(T0 + DAY + MIN)),
        }));
        assert_eq!(
            (t.rows[2].selected(), t.detail),
            (None, false),
            "another game starts afresh"
        );
    }

    #[test]
    fn every_page_fits_80_by_24() {
        let now = T0 + 30 * MIN;
        let mut t = top(Some(game(now)), now - 2000);
        let scores = screen(&mut t, 80, 24, now);
        assert!(
            scores.contains("ridgewarden-game top  Test game"),
            "{scores}"
        );
        assert!(scores.contains("updated 2 s ago"));
        assert!(scores.contains(" LIVE "));
        assert!(scores.contains("(1 h left)"), "{scores}");
        // 20 for two captures, then 30 minutes of a two-site chain
        assert!(scores.contains(" Red             80  2/min"), "{scores}");
        assert!(
            scores.contains("a - b (2 sites, 2 points a minute)"),
            "{scores}"
        );
        t.tab = 1;
        let sites = screen(&mut t, 80, 24, now);
        let c = sites
            .lines()
            .find(|l| l.starts_with("c "))
            .unwrap_or_default();
        assert!(
            c.contains("Blue") && c.contains("alone") && c.ends_with("21 min left"),
            "{sites}"
        );
        t.key(key(KeyCode::Char('j')));
        t.key(key(KeyCode::Enter));
        let a = screen(&mut t, 80, 24, now);
        assert!(a.contains("Held by Red with a link up."), "{a}");
        assert!(a.contains("Links: r up."), "{a}");
        t.key(key(KeyCode::Char('5')));
        let players = screen(&mut t, 80, 24, now);
        assert!(players.contains("1 impossible hop"), "{players}");
        t.tab = 3;
        assert!(screen(&mut t, 80, 24, now).contains("No foxes in this game."));
        for tab in 0..TABS.len() {
            t.tab = tab;
            let small = screen(&mut t, MIN_SIZE.0, MIN_SIZE.1, now);
            assert!(!small.contains("Make the terminal"), "{small}");
        }
        assert!(screen(&mut t, MIN_SIZE.0 - 1, 24, now).contains("Make the terminal at least"));
    }

    #[test]
    fn the_view_says_when_it_is_loading_empty_or_cut_off() {
        let now = T0 + 30 * MIN;
        let mut t = top(None, now);
        t.snap = None;
        let loading = screen(&mut t, 80, 24, now);
        assert!(
            loading.contains("Loading...") && loading.contains("loading"),
            "{loading}"
        );
        let refused = format!("{URL}/api/games: could not connect");
        t.take(Err(refused.clone()));
        let down = screen(&mut t, 80, 24, now);
        assert!(down.contains("ERROR"), "{down}");
        assert!(down.contains(&format!("Can't read {refused}.")), "{down}");
        assert!(down.contains("sudo systemctl start"), "{down}");

        let mut t = top(None, now);
        assert!(screen(&mut t, 80, 24, now).contains("No game yet."));
        let mut t = top(Some(game(now)), now - 5000);
        t.take(Err(refused.clone()));
        let stale = screen(&mut t, 80, 24, now);
        assert!(stale.contains("Reading failed, so this is from earlier: http://"));
        assert!(
            stale.replace('\n', " ").contains(": could not connect"),
            "the reason, wrapped: {stale}"
        );
        assert!(
            stale.contains("page not answering, updated 5 s ago"),
            "{stale}"
        );
        assert!(
            stale.contains("Chains now"),
            "the last good read stays: {stale}"
        );
    }

    #[test]
    fn lines_read_like_the_page() {
        let p = game(T0 + 30 * MIN);
        let theme = Theme { color: false };
        let line = |l: Line| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        let live = line(game_line(&Ctx::new(&theme, &p, T0 + 30 * MIN)));
        assert_eq!(
            live,
            format!(
                " LIVE  Started {}, ends {} (1 h left).",
                short_time(T0),
                short_time(T0 + 120 * MIN)
            )
        );
        let over = line(game_line(&Ctx::new(&theme, &p, T0 + 120 * MIN)));
        assert!(
            over.starts_with(" OVER  Ran ") && over.ends_with("(2 h). Final scores."),
            "{over}"
        );
        assert_eq!(hm(T0 + 14 * 60 * MIN + 32 * MIN), "14:32");
        assert_eq!(count(1u32, "capture", "captures"), "1 capture");
        assert_eq!(count(0u32, "capture", "captures"), "0 captures");
        let c = Ctx::new(&theme, &p, T0 + 30 * MIN);
        let mut x = p.players[0].clone();
        let notes = [0, 1, 2, 3].map(|n| {
            x.strikes = n;
            strikes(&c, &x)
        });
        assert_eq!(
            notes,
            [
                "",
                "1 impossible hop",
                "2 impossible hops",
                "out: too many impossible hops"
            ],
            "the page's words"
        );
        let mut ch = p.chains[0].clone();
        ch.new_links = ch.links.iter().take(1).cloned().collect();
        assert_eq!(
            chain_text(&c, &ch),
            "a - b (2 sites, 4 points a minute, 1 new link)"
        );
    }

    async fn serve(app: Arc<App>) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let svc = router(app).into_make_service_with_connect_info::<Peer>();
        tokio::spawn(async move { axum::serve(l, svc).await });
        url
    }

    #[tokio::test]
    async fn it_reads_the_list_of_games_then_the_game() {
        let core = FakeCore::start("a", &[]).await;
        let dir = tempfile::tempdir().unwrap();
        let mut cfg: Config = toml::from_str(EXAMPLE).unwrap();
        cfg.data_dir = dir.path().into();
        cfg.core.url = core.url.clone();
        cfg.admins = vec![core.key.clone()];
        let app = Arc::new(App::new(cfg, false).unwrap());
        let url = serve(app.clone()).await;
        let fetch = Fetch::new(Duration::from_secs(10)).unwrap();
        assert!(read(&fetch, &url).await.unwrap().is_none(), "no game yet");

        let start = json!({"name": "Hill day", "teams": ["Red", "Blue"], "sites": ["a"]});
        let at = now_ms();
        core.add("game.start", start, at);
        ridgewarden_client::follow(&app.core, &app.db, PROJECTIONS)
            .await
            .unwrap();
        let p = read(&fetch, &url).await.unwrap().expect("the game");
        assert_eq!((p.session.name.as_str(), p.teams.len()), ("Hill day", 2));
        let games: serde_json::Value = fetch.json(&format!("{url}/api/games")).await.unwrap();
        let week = crate::game::MINUTES_MAX * MIN;
        let listed = json!([{"id": p.session.id, "name": "Hill day", "start": at,
            "end": at + week, "ended": false}]);
        assert_eq!(
            games, listed,
            "a game started without --minutes runs a week"
        );

        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        // Windows takes a couple of seconds to give up on a closed port
        let patient = Fetch::new(Duration::from_secs(20)).unwrap();
        let err = load(&patient, &closed).await.err().unwrap();
        assert_eq!(err, format!("{closed}/api/games: could not connect"));
    }

    /// What `/api/game` returned mid-game on the simulated mesh, kept as it came.
    const LIVE: &str = include_str!("../tests/fixtures/game.json");

    /// The words of the first row on a screen that starts with `start`, after any spaces.
    fn row(screen: &str, start: &str) -> Vec<String> {
        let line = screen
            .lines()
            .find(|l| l.trim_start().starts_with(start))
            .unwrap_or_default();
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn a_live_games_json_fills_every_page() {
        let p: Played = serde_json::from_str(LIVE).unwrap();
        let now = p.at;
        // paso's only link went down at 00:11, after it last had one up at 00:10:32
        assert_eq!(
            hold(&p, "tst-paso", now),
            Some(Hold::Until(p.linked["tst-paso"] + 30 * MIN))
        );
        assert_eq!(hold(&p, "tst-madonna", now), Some(Hold::Linked));
        let mut t = top(Some(p), now);
        let scores = screen(&mut t, 80, 24, now);
        assert!(
            scores.contains("ridgewarden-game top  Saturday hunt"),
            "{scores}"
        );
        assert_eq!(
            row(&scores, "Blue"),
            ["Blue", "106", "2/min", "51", "0", "30", "5", "20", "3"],
            "{scores}"
        );
        assert_eq!(
            row(&scores, "Red"),
            ["Red", "97", "2/min", "62", "0", "30", "5", "0", "2"]
        );
        assert!(scores.contains("tst-cuesta - tst-lopez (2 sites, 2 points a minute)"));
        let newest = [
            "+15",
            "00:06",
            "N0CALL-6",
            "Blue",
            "tst-madonna",
            "captured",
            "for",
            "Blue;",
            "met",
            "N0",
        ];
        assert_eq!(row(&scores, "+15"), newest, "the latest play, cut to fit");
        t.tab = 1;
        let sites = screen(&mut t, 80, 24, now);
        let paso = [
            "tst-paso", "code", "word", "Blue", "alone", "29", "min", "left",
        ];
        assert_eq!(row(&sites, "tst-paso"), paso, "{sites}");
        assert_eq!(
            row(&sites, "tst-madonna"),
            [
                "tst-madonna",
                "LAN",
                "Blue",
                "2",
                "sites,",
                "2/min",
                "links",
                "up"
            ]
        );
        t.tab = 3;
        let foxes = screen(&mut t, 80, 24, now);
        let fox = [
            "Fox", "1", "N0CALL-9", "1", "19", "2", "once", "an", "admin", "ends", "the", "game",
        ];
        assert_eq!(row(&foxes, "Fox 1"), fox, "{foxes}");
        assert!(foxes.contains("N0CALL-7   Fox 1        35.5000, -120.5800   150 deg"));
        t.key(key(KeyCode::Enter));
        let fox = screen(&mut t, 80, 24, now);
        assert!(
            fox.contains("Found by 1 hunter; 19 of its 20 one-time codes are left."),
            "{fox}"
        );
        assert_eq!(
            row(&fox, "+20")[..7],
            ["+20", "23:56", "N0CALL-6", "Blue", "fox", "Fox", "1"],
            "the finder: {fox}"
        );
        t.key(key(KeyCode::Char('5')));
        let players = screen(&mut t, 80, 24, now);
        assert_eq!(
            row(&players, "N0CALL-9"),
            ["N0CALL-9", "5", "0", "0", "0", "runs", "a", "fox"]
        );
        t.key(key(KeyCode::Enter));
        let top_scorer = screen(&mut t, 80, 24, now);
        assert!(
            top_scorer.contains("N0CALL-6 plays for Blue with 45 points."),
            "{top_scorer}"
        );
        assert!(top_scorer.contains("2 captures, 1 encounter and 1 fox found."));

        t.key(key(KeyCode::Char('1')));
        t.theme = Theme { color: true };
        let shot = frame(&mut t, 80, 24, now);
        if std::env::var("README_SHOTS").as_deref() == Ok("1") {
            let page = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/images/top.html");
            std::fs::write(page, html(&shot)).unwrap();
        }
    }

    /// Default text and background of the terminal the README's picture shows.
    const FG: &str = "#cccccc";
    const BG: &str = "#0c0c0c";

    /// A color as Windows Terminal's default scheme draws it.
    fn css(c: Ansi, default: &str) -> String {
        let hex = match c {
            Ansi::Black => "#0c0c0c",
            Ansi::Red => "#c50f1f",
            Ansi::Green => "#13a10e",
            Ansi::Yellow => "#c19c00",
            Ansi::Blue => "#0037da",
            Ansi::Magenta => "#881798",
            Ansi::Cyan => "#3a96dd",
            Ansi::Gray => "#cccccc",
            Ansi::DarkGray => "#767676",
            Ansi::LightRed => "#e74856",
            Ansi::LightGreen => "#16c60c",
            Ansi::LightYellow => "#f9f1a5",
            Ansi::LightBlue => "#3b78ff",
            Ansi::LightMagenta => "#b4009e",
            Ansi::LightCyan => "#61d6d6",
            Ansi::White => "#f2f2f2",
            Ansi::Rgb(r, g, b) => return format!("#{r:02x}{g:02x}{b:02x}"),
            Ansi::Reset | Ansi::Indexed(_) => default,
        };
        hex.to_string()
    }

    fn cell_css(c: &ridgewarden_client::tui::ratatui::buffer::Cell) -> String {
        let (mut fg, mut bg) = (css(c.fg, FG), css(c.bg, BG));
        if c.modifier.contains(Modifier::REVERSED) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if c.modifier.contains(Modifier::DIM) {
            fg = format!("color-mix(in srgb,{fg} 60%,{bg})");
        }
        let bold = if c.modifier.contains(Modifier::BOLD) {
            ";font-weight:bold"
        } else {
            ""
        };
        format!("color:{fg};background:{bg}{bold}")
    }

    /// A frame as an HTML page of colored monospace cells, for the README's screenshot.
    fn html(b: &Buffer) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "<!doctype html><meta charset=utf-8><title>ridgewarden-game top</title><style>\
             body{{margin:0;background:{BG}}}pre{{display:inline-block;margin:0;padding:14px 18px;\
             font:15px/1.35 'Cascadia Mono',Consolas,monospace;color:{FG};background:{BG}}}\
             </style><pre>"
        );
        for y in 0..b.area.height {
            let mut x = 0;
            while x < b.area.width {
                let style = cell_css(&b[(x, y)]);
                let mut run = String::new();
                while x < b.area.width && cell_css(&b[(x, y)]) == style {
                    run.push_str(b[(x, y)].symbol());
                    x += 1;
                }
                let _ = write!(
                    out,
                    "<span style=\"{style}\">{}</span>",
                    ridgewarden_client::util::esc(&run)
                );
            }
            out.push('\n');
        }
        out.push_str("</pre>");
        out
    }
}
