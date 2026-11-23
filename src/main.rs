use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ridgewarden_client::event::valid_call;
use ridgewarden_client::util::now_ms;
use serde_json::{Value, json};

use ridgewarden_game::config::{Config, EXAMPLE};
use ridgewarden_game::game::{self, Session, Start, check_teams};
use ridgewarden_game::web::{self, Peer};
use ridgewarden_game::words;
use ridgewarden_game::world::{Pos, World};
use ridgewarden_game::{App, PROJECTIONS, spawn_loops};

#[derive(Parser)]
#[command(
    version,
    about = "Capture the Site, a radio-topology game for AREDN meshes (a ridgewarden app)"
)]
struct Cli {
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/ridgewarden/game.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config
    Init,
    /// Follow the core, take check-ins and serve the map and leaderboard
    Run,
    #[command(flatten)]
    Admin(Admin),
}

/// Commands for admins; each is signed by this site's core.
#[derive(Subcommand)]
enum Admin {
    /// Start a game (admins only)
    Start {
        /// what to call it, e.g. "November meeting"
        name: String,
        /// team names, separated by commas
        #[arg(long, value_delimiter = ',', required = true)]
        teams: Vec<String>,
        /// sites taken by checking in on the node's LAN, separated by commas
        #[arg(long, value_delimiter = ',')]
        sites: Vec<String>,
        /// public sites, taken with a code word sent on air there, separated by commas
        #[arg(long, value_delimiter = ',')]
        word_sites: Vec<String>,
        /// a fox: NAME, or NAME=CALL for whoever runs it; repeat for more
        #[arg(long)]
        fox: Vec<String>,
        /// one-time codes to leave with each fox: one for every hunter who may find it
        #[arg(long, default_value_t = 30)]
        codes: usize,
        /// end it by itself after this many minutes (default: a week)
        #[arg(long)]
        minutes: Option<i64>,
    },
    /// Change the running game's teams, or put players on a team (admins only)
    Teams {
        /// the new team names, separated by commas; omit to keep them
        #[arg(long, value_delimiter = ',')]
        teams: Vec<String>,
        /// CALL=Team, puts a player on a team; repeat for more
        #[arg(long = "player")]
        players: Vec<String>,
        /// take a player off the roster
        #[arg(long)]
        drop: Vec<String>,
    },
    /// End the running game (admins only)
    End {
        /// NAME=LAT,LON: where a fox was, shown on the map now the hunt is over
        #[arg(long)]
        fox: Vec<String>,
    },
    /// Print the running game's code words and fox codes again
    Words,
    /// Set the key a callsign plays with, for a player who lost their passphrase or whose
    /// callsign someone else used first (admins only)
    PlayerKey {
        call: String,
        /// the key their passphrase makes, as the game page shows it after they post
        key: String,
    },
    /// Open or close a season; games started while one is open count towards it
    Season {
        #[command(subcommand)]
        cmd: SeasonCmd,
    },
}

#[derive(Subcommand)]
enum SeasonCmd {
    /// Open a season
    Start { name: String },
    /// Close the open season
    End,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ridgewarden_game=info,ridgewarden_client=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => {
            if cli.config.exists() {
                bail!(
                    "{} already exists; not overwriting it",
                    cli.config.display()
                );
            }
            if let Some(d) = cli.config.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&cli.config, EXAMPLE)?;
            println!("Wrote {}", cli.config.display());
            Ok(())
        }
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(run(cfg))
        }
        Cmd::Admin(cmd) => {
            let app = App::new(Config::load(&cli.config)?, true)?;
            tokio::runtime::Runtime::new()?.block_on(admin(&app, cmd, &cli.config))
        }
    }
}

async fn run(cfg: Config) -> Result<()> {
    let listen = cfg.listen;
    // the token lets this site sign check-ins, code words and reveals; without it the
    // site only shows the game
    let signs = cfg.core.token_file.exists();
    let app = Arc::new(App::new(cfg.clone(), signs)?);
    tracing::info!(
        "ridgewarden-game {} following {}, page on http://{listen}",
        env!("CARGO_PKG_VERSION"),
        app.core.base()
    );
    if !signs {
        tracing::info!(
            "no core token at {}, so this site shows the game but takes no posts",
            cfg.core.token_file.display()
        );
    } else if cfg.lan.is_empty() {
        tracing::info!("no lan set in the config, so this site takes no LAN check-ins");
    }
    spawn_loops(app.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    axum::serve(
        listener,
        web::router(app).into_make_service_with_connect_info::<Peer>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}

/// Check this station may run games, catch up with the feed, then do the command.
async fn admin(app: &App, cmd: Admin, config: &Path) -> Result<()> {
    let me = app
        .core
        .whoami()
        .await
        .context("asking the core who this station is")?;
    if !app.cfg.admins.contains(&me.key) {
        bail!(
            "this station's key {} is not in admins in {}; add it there on every site",
            me.key,
            config.display()
        );
    }
    ridgewarden_client::follow(&app.core, &app.db, PROJECTIONS)
        .await
        .context("reading the core's feed")?;
    let sessions = app.sessions()?;
    let running = game::current(&sessions, now_ms());
    match cmd {
        Admin::Start {
            name,
            teams,
            sites,
            word_sites,
            fox,
            codes,
            minutes,
        } => {
            if let Some(s) = running {
                bail!(
                    "{:?} is still running; end it first with `ridgewarden-game end`",
                    s.name
                );
            }
            let foxes = fox_list(&fox)?;
            let seasons = game::seasons(&app.db(), &app.cfg.admins)?;
            let season = game::open_season(&seasons).map(|s| s.id.clone());
            let payload = json!({"name": name, "teams": teams, "sites": sites,
                "word_sites": word_sites, "foxes": foxes, "season": season,
                "rules": app.cfg.rules, "minutes": minutes});
            start(app, payload, codes).await
        }
        Admin::Teams {
            teams,
            players,
            drop,
        } => {
            let s = running.context("no game is running")?;
            set_teams(app, s, teams, players, drop).await
        }
        Admin::End { fox } => {
            // a game that ran out by itself can still be told where its foxes were
            let recent = sessions
                .first()
                .filter(|s| !fox.is_empty() && now_ms() - s.end < game::DAY);
            let s = running.or(recent).context("no game is running")?;
            let at = fox_places(s, &fox)?;
            app.core
                .append("game.end", json!({"session": s.id, "foxes": at}))
                .await?;
            if running.is_some() {
                println!("Ended {}.", s.name);
            } else {
                println!("Recorded where the foxes of {} were.", s.name);
            }
            Ok(())
        }
        Admin::Words => {
            let s = running.context("no game is running")?;
            let path = words::seed_path(&app.cfg.data_dir);
            let seed = words::load_seed(&path)?.with_context(|| {
                format!(
                    "no code word seed at {}; only the site that started the game has it",
                    path.display()
                )
            })?;
            if !s.word_sites.is_empty() {
                print!("{}", words::schedule(&seed, s));
            }
            if !s.foxes.is_empty() {
                print!("{}", words::codes(&seed, s));
            }
            Ok(())
        }
        Admin::PlayerKey { call, key } => {
            let call = call.trim().to_ascii_uppercase();
            if !valid_call(&call) {
                bail!("{call:?} is not a callsign");
            }
            let key = key.trim().to_ascii_lowercase();
            if !game::valid_key(&key) {
                bail!("{key:?} is not a player key: 64 hex digits, as the game page shows it");
            }
            app.core
                .append("game.key", json!({"call": call, "key": key}))
                .await?;
            println!("{call} now plays with key {key}.");
            Ok(())
        }
        Admin::Season { cmd } => season(app, cmd).await,
    }
}

/// `NAME` or `NAME=CALL` for each fox.
fn fox_list(args: &[String]) -> Result<Vec<Value>> {
    args.iter()
        .map(|a| match a.split_once('=') {
            Some((name, call)) => {
                let call = call.trim().to_ascii_uppercase();
                if !valid_call(&call) {
                    bail!("{call:?} is not a callsign");
                }
                Ok(json!({"name": name.trim(), "call": call}))
            }
            None => Ok(json!({"name": a.trim()})),
        })
        .collect()
}

/// `NAME=LAT,LON` for each fox, checked against the game's foxes.
fn fox_places(s: &Session, args: &[String]) -> Result<BTreeMap<String, [f64; 2]>> {
    let mut out = BTreeMap::new();
    for a in args {
        let (name, at) = a
            .split_once('=')
            .with_context(|| format!("{a:?} should look like NAME=LAT,LON"))?;
        let name = name.trim();
        if s.fox(name).is_none() {
            bail!("this game has no fox called {name:?}");
        }
        let pos = at
            .split_once(',')
            .and_then(|(lat, lon)| Pos::new(lat.trim().parse().ok()?, lon.trim().parse().ok()?))
            .with_context(|| format!("{at:?} is not a latitude and longitude"))?;
        out.insert(name.to_string(), [pos.lat, pos.lon]);
    }
    Ok(out)
}

async fn start(app: &App, mut payload: Value, codes: usize) -> Result<()> {
    let has = |k: &str| payload[k].as_array().is_some_and(|a| !a.is_empty());
    let foxes = has("foxes");
    let seed = if foxes || has("word_sites") {
        // made before the game starts, so a broken data folder stops it early
        Some(words::seed(&words::seed_path(&app.cfg.data_dir))?)
    } else {
        None
    };
    if let Some(seed) = &seed
        && foxes
    {
        add_codes(&mut payload, seed, codes)?;
    }
    let st = Start::parse(&payload)?;
    let unsafe_sites: Vec<&String> = st
        .sites
        .iter()
        .chain(&st.word_sites)
        .filter(|n| !app.cfg.is_safe(n))
        .collect();
    if !unsafe_sites.is_empty() {
        bail!(
            "not in safe_sites in the config: {}. Only sites that are safe and legal for players \
             to visit can be in a game; add them there once that's agreed with their owners",
            unsafe_sites
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let world = World::places(&app.db())?;
    for n in st.sites.iter().filter(|n| !world.has_lan_core(n)) {
        println!(
            "Warning: no core polls {n} over its LAN yet, so nobody can check in there until one does."
        );
    }
    let ev = app.core.append("game.start", payload).await?;
    println!("Started {} ({}).", st.name, &ev.id[..8]);
    if let Some(seed) = seed {
        write_lists(app, &ev.id, &seed).await?;
    }
    Ok(())
}

/// Give each fox `n` one-time codes. Only their hashes go in the start event, with a
/// fresh nonce so no two games share a code.
fn add_codes(payload: &mut Value, seed: &words::Seed, n: usize) -> Result<()> {
    if !(1..=game::CODES_MAX).contains(&n) {
        bail!("--codes must be between 1 and {}", game::CODES_MAX);
    }
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("no randomness for the nonce: {e}"))?;
    let nonce = hex::encode(nonce);
    for f in payload["foxes"].as_array_mut().into_iter().flatten() {
        let name = f["name"].as_str().unwrap_or_default().to_string();
        let hashes: Vec<String> = (0..n)
            .map(|i| words::code_hash(&nonce, &name, &words::fox_code(seed, &nonce, &name, i)))
            .collect();
        f["codes"] = json!(hashes);
    }
    payload["nonce"] = json!(nonce);
    Ok(())
}

/// Save the new game's code words and fox codes next to the seed, for the admin to hand
/// out.
async fn write_lists(app: &App, id: &str, seed: &words::Seed) -> Result<()> {
    ridgewarden_client::follow(&app.core, &app.db, PROJECTIONS).await?;
    let s = app
        .sessions()?
        .into_iter()
        .find(|s| s.id == id)
        .context("the new game isn't in the feed yet; run `ridgewarden-game words` shortly")?;
    let path = |name: &str| app.cfg.data_dir.join(format!("{name}-{}.csv", &id[..8]));
    if !s.word_sites.is_empty() {
        let p = path("words");
        words::write_private(&p, words::schedule(seed, &s).as_bytes())?;
        println!(
            "Code words for every slot are in {}. Keep it private until each word is due on air.",
            p.display()
        );
    }
    if !s.foxes.is_empty() {
        let p = path("codes");
        words::write_private(&p, words::codes(seed, &s).as_bytes())?;
        println!(
            "Each fox's one-time codes are in {}. Print them and leave each fox's codes with it.",
            p.display()
        );
    }
    Ok(())
}

async fn set_teams(
    app: &App,
    s: &Session,
    teams: Vec<String>,
    players: Vec<String>,
    drop: Vec<String>,
) -> Result<()> {
    let teams = if teams.is_empty() {
        s.teams.clone()
    } else {
        teams
    };
    check_teams(&teams)?;
    let mut roster: BTreeMap<String, String> = s
        .roster
        .iter()
        .filter(|(_, t)| teams.contains(t))
        .map(|(c, t)| (c.clone(), t.clone()))
        .collect();
    for p in &players {
        let (call, team) = p
            .split_once('=')
            .with_context(|| format!("{p:?} should look like CALL=Team"))?;
        let (call, team) = (call.trim().to_ascii_uppercase(), team.trim());
        if !valid_call(&call) {
            bail!("{call:?} is not a callsign");
        }
        if !teams.iter().any(|t| t == team) {
            bail!("there is no team called {team:?}");
        }
        roster.insert(call, team.to_string());
    }
    for d in &drop {
        roster.remove(&d.trim().to_ascii_uppercase());
    }
    app.core
        .append(
            "game.teams",
            json!({"session": s.id, "teams": teams, "roster": roster}),
        )
        .await?;
    println!(
        "Teams for {}: {}; {} players on the roster.",
        s.name,
        teams.join(", "),
        roster.len()
    );
    Ok(())
}

async fn season(app: &App, cmd: SeasonCmd) -> Result<()> {
    let seasons = game::seasons(&app.db(), &app.cfg.admins)?;
    let open = game::open_season(&seasons);
    match cmd {
        SeasonCmd::Start { name } => {
            if let Some(s) = open {
                bail!(
                    "{:?} is still open; close it first with `ridgewarden-game season end`",
                    s.name
                );
            }
            let name = name.trim();
            if name.is_empty() || name.chars().count() > game::NAME_MAX {
                bail!("the name must be 1 to {} characters", game::NAME_MAX);
            }
            app.core
                .append("game.season", json!({"action": "start", "name": name}))
                .await?;
            println!("Opened season {name}.");
        }
        SeasonCmd::End => {
            let s = open.context("no season is open")?;
            app.core
                .append("game.season", json!({"action": "end", "season": s.id}))
                .await?;
            println!("Closed season {}.", s.name);
        }
    }
    Ok(())
}
