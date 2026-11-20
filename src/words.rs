//! Rotating code words for public sites and foxes. The admin's app keeps a secret seed;
//! each target's word for each slot of `word_min` minutes is derived from it. Someone at
//! the site, or the fox transmitter, sends the current word on air; a player who hears it
//! types it in on any site's page. After each slot ends, the admin's app reveals that
//! slot's words in signed events, and claims made before the reveal count.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ridgewarden_client::util::{now_ms, short_time};
use serde_json::json;

use crate::App;
use crate::game::{self, MIN, Session, Target};

/// A word heard just before it changed still counts for this long into the next slot.
pub const GRACE: i64 = 2 * MIN;

/// Short words that are hard to mishear on a weak signal.
const WORDS: [&str; 128] = [
    "ACORN", "ANVIL", "APPLE", "ARROW", "BADGE", "BAKER", "BANJO", "BARREL", "BASIN", "BEACON",
    "BEAVER", "BISON", "BOULDER", "BRIDGE", "BUCKET", "BUTTON", "CABIN", "CACTUS", "CAMEL",
    "CANDLE", "CANYON", "CARGO", "CASTLE", "CEDAR", "CHALK", "CHERRY", "CIRCLE", "CLOVER",
    "COBALT", "COMET", "COPPER", "CORAL", "COTTON", "COYOTE", "CRANE", "CREEK", "DAISY", "DELTA",
    "DESERT", "DOLPHIN", "DRAGON", "EAGLE", "EMBER", "FALCON", "FERRY", "FIDDLE", "FOREST",
    "FOSSIL", "GARDEN", "GARNET", "GLACIER", "GOPHER", "GRANITE", "GRAVEL", "HAMMER", "HARBOR",
    "HAWK", "HAZEL", "HERON", "HICKORY", "HOLLOW", "HONEY", "ISLAND", "JACKET", "JIGSAW",
    "JUNIPER", "KETTLE", "KITE", "LADDER", "LAGOON", "LANTERN", "LAUREL", "LEMON", "LIZARD",
    "LOCKET", "MAGNET", "MAPLE", "MARBLE", "MEADOW", "MESA", "MITTEN", "MOOSE", "NUTMEG", "OCEAN",
    "OLIVE", "ORBIT", "OTTER", "PADDLE", "PEBBLE", "PELICAN", "PEPPER", "PILOT", "PINE", "PLANET",
    "POCKET", "POPPY", "PRAIRIE", "PUMPKIN", "QUARRY", "RABBIT", "RAVEN", "RIVER", "ROCKET",
    "SADDLE", "SALMON", "SHOVEL", "SIGNAL", "SPRUCE", "SUMMIT", "TABLE", "THUNDER", "TIMBER",
    "TOMATO", "TRUMPET", "TULIP", "TUNNEL", "TURTLE", "VALLEY", "VELVET", "VIOLIN", "WAGON",
    "WALNUT", "WILLOW", "WINDMILL", "WIZARD", "YARROW", "ZEBRA", "ZIPPER",
];

pub type Seed = [u8; 32];

/// The word for one target and slot, e.g. "OTTER LANTERN 42".
pub fn word(seed: &Seed, session: &str, target: &Target, slot: i64) -> String {
    let h = blake3::keyed_hash(
        seed,
        format!("{session}|{}|{slot}", target.key()).as_bytes(),
    );
    let b = h.as_bytes();
    format!(
        "{} {} {:02}",
        WORDS[b[0] as usize % WORDS.len()],
        WORDS[b[1] as usize % WORDS.len()],
        u16::from_le_bytes([b[2], b[3]]) % 100
    )
}

/// Letters and digits only, upper case, so "otter-lantern 4 2" matches "OTTER LANTERN 42".
pub fn normalize(w: &str) -> String {
    w.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// The slot a moment falls in, counted from the start of the session.
pub fn slot_of(s: &Session, ts: i64) -> i64 {
    (ts - s.start).div_euclid(s.rules.word_min * MIN)
}

pub fn slot_start(s: &Session, slot: i64) -> i64 {
    s.start + slot * s.rules.word_min * MIN
}

fn slots(s: &Session) -> std::ops::Range<i64> {
    0..slot_of(s, s.end - 1) + 1
}

pub fn seed_path(data_dir: &Path) -> PathBuf {
    data_dir.join("codewords.key")
}

pub fn load_seed(path: &Path) -> Result<Option<Seed>> {
    match std::fs::read(path) {
        Ok(b) => {
            let seed: Seed = b
                .try_into()
                .map_err(|_| anyhow::anyhow!("{} is not 32 bytes", path.display()))?;
            Ok(Some(seed))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// This site's seed, made the first time it's needed. Whoever has it can work out every
/// word in advance, so it stays readable by its owner only.
pub fn seed(path: &Path) -> Result<Seed> {
    if let Some(s) = load_seed(path)? {
        return Ok(s);
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("no randomness for the seed: {e}"))?;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, seed).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(seed)
}

/// Every word of a session, one line per target and slot: what the admin hands to the
/// people at public sites and loads into the fox transmitters.
pub fn schedule(seed: &Seed, s: &Session) -> String {
    let mut out = String::from("target,from_utc,to_utc,word\n");
    for t in s.word_targets() {
        let name = match &t {
            Target::Site(n) => n.clone(),
            Target::Fox(n) => format!("fox {n}"),
        };
        for k in slots(s) {
            let from = slot_start(s, k);
            let to = slot_start(s, k + 1).min(s.end);
            let _ = writeln!(
                out,
                "{name},{},{},{}",
                short_time(from),
                short_time(to),
                word(seed, &s.id, &t, k)
            );
        }
    }
    out
}

/// What a claim made at `ts` with `typed` comes to, given the reveals so far.
#[derive(Debug, PartialEq)]
pub enum Check {
    Right,
    Wrong,
    /// the word for its slot hasn't been revealed yet; it will be after this time
    Waiting(i64),
    /// it was made after the word had already been revealed
    Late,
}

/// Check a claim against the revealed words of its slot, and of the slot before if it
/// came within `GRACE` of the change.
pub fn check(
    s: &Session,
    reveals: &std::collections::BTreeMap<(String, i64), (String, i64)>,
    target: &Target,
    ts: i64,
    typed: &str,
) -> Check {
    let k = slot_of(s, ts);
    let mut slots = vec![k];
    if k > 0 && ts - slot_start(s, k) < GRACE {
        slots.push(k - 1);
    }
    let typed = normalize(typed);
    let mut waiting = None;
    for slot in slots {
        match reveals.get(&(target.key(), slot)) {
            Some((w, at)) if normalize(w) == typed => {
                return if ts < *at { Check::Right } else { Check::Late };
            }
            Some(_) => {}
            None => waiting = Some(slot_start(s, slot + 1) + GRACE),
        }
    }
    match waiting {
        Some(t) => Check::Waiting(t),
        None => Check::Wrong,
    }
}

/// Reveal the words of every slot that has ended, for the games this station started.
/// Runs on the admin's own site, the only one with the seed.
pub async fn reveal_due(app: &App) -> Result<usize> {
    let Some(seed) = load_seed(&seed_path(&app.cfg.data_dir))? else {
        return Ok(0);
    };
    let Some(me) = app.me().await else {
        return Ok(0);
    };
    let now = now_ms();
    let mut due = vec![];
    for s in app.sessions()?.into_iter().filter(|s| s.key == me.key) {
        if s.word_targets().is_empty() || now > s.end + game::DAY {
            continue;
        }
        let done = game::reveals(&app.db(), &s)?;
        for t in s.word_targets() {
            for k in slots(&s).filter(|k| slot_start(&s, k + 1).min(s.end) + GRACE <= now) {
                if !done.contains_key(&(t.key(), k)) {
                    due.push(reveal_payload(&s, &t, k, word(&seed, &s.id, &t, k)));
                }
            }
        }
    }
    for p in &due {
        app.core.append("game.reveal", p.clone()).await?;
    }
    if !due.is_empty() {
        // read them back now, so the next round doesn't reveal them again
        ridgewarden_client::follow(&app.core, &app.db, crate::PROJECTIONS).await?;
    }
    Ok(due.len())
}

fn reveal_payload(s: &Session, t: &Target, slot: i64, word: String) -> serde_json::Value {
    let mut p = json!({"session": s.id, "slot": slot, "word": word});
    match t {
        Target::Site(n) => p["site"] = json!(n),
        Target::Fox(n) => p["fox"] = json!(n),
    }
    p
}

/// Whether a word was already revealed for this target. A core whose clock is behind
/// would otherwise stamp a claim for a revealed word as made before the reveal.
pub fn revealed(
    reveals: &std::collections::BTreeMap<(String, i64), (String, i64)>,
    target: &Target,
    typed: &str,
) -> bool {
    let (key, typed) = (target.key(), normalize(typed));
    reveals
        .iter()
        .any(|((k, _), (w, _))| *k == key && normalize(w) == typed)
}

/// A typed word must look like one of ours before it's signed and logged.
pub fn plausible(typed: &str) -> Result<()> {
    let n = normalize(typed);
    if n.len() < 4 || n.len() > 40 || typed.len() > 64 {
        bail!("that doesn't look like a code word: two words and a number, like OTTER LANTERN 42");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::Rules;
    use std::collections::{BTreeMap, BTreeSet};

    fn session() -> Session {
        Session {
            id: "s1".into(),
            name: "Hunt".into(),
            by: "N0CALL".into(),
            key: "k".into(),
            start: 0,
            end: 60 * MIN,
            ended: false,
            teams: vec!["Red".into(), "Blue".into()],
            roster: BTreeMap::new(),
            sites: vec![],
            word_sites: vec!["park".into()],
            foxes: vec![],
            season: None,
            rules: Rules::default(),
        }
    }

    #[test]
    fn words_differ_by_slot_target_and_seed_and_read_back_loosely() {
        let t = Target::Site("park".into());
        let a = word(&[1; 32], "s1", &t, 0);
        assert_eq!(a, word(&[1; 32], "s1", &t, 0));
        let others: BTreeSet<String> = [
            word(&[1; 32], "s1", &t, 1),
            word(&[2; 32], "s1", &t, 0),
            word(&[1; 32], "s1", &Target::Fox("park".into()), 0),
            word(&[1; 32], "s2", &t, 0),
        ]
        .into_iter()
        .collect();
        assert!(!others.contains(&a) && others.len() == 4);
        assert_eq!(
            normalize(&a.to_lowercase().replace(' ', "-")),
            normalize(&a)
        );
    }

    #[test]
    fn claims_check_against_their_own_slot_with_grace() {
        let s = session();
        let t = Target::Site("park".into());
        let mut rv = BTreeMap::new();
        // slot 0 is minutes 0 to 15
        assert_eq!(
            check(&s, &rv, &t, 5 * MIN, "OTTER KITE 01"),
            Check::Waiting(17 * MIN)
        );
        rv.insert((t.key(), 0), ("OTTER KITE 01".to_string(), 17 * MIN));
        assert_eq!(check(&s, &rv, &t, 5 * MIN, "otter kite 0 1"), Check::Right);
        assert_eq!(check(&s, &rv, &t, 5 * MIN, "OTTER KITE 02"), Check::Wrong);
        // a minute into slot 1, the old word still counts once slot 0 is out
        assert_eq!(check(&s, &rv, &t, 16 * MIN, "OTTER KITE 01"), Check::Right);
        assert_eq!(
            check(&s, &rv, &t, 17 * MIN, "OTTER KITE 01"),
            Check::Waiting(32 * MIN),
            "the grace ends two minutes into the slot"
        );
        assert_eq!(
            check(&s, &rv, &t, 18 * MIN, "OTTER KITE 01"),
            Check::Waiting(32 * MIN)
        );
        // typed in after the reveal, when anyone could have read it off the page
        rv.insert((t.key(), 1), ("CEDAR MESA 77".to_string(), 32 * MIN));
        assert_eq!(
            check(&s, &rv, &t, 16 * MIN + 1, "OTTER KITE 01"),
            Check::Right
        );
        assert_eq!(check(&s, &rv, &t, 18 * MIN, "OTTER KITE 01"), Check::Wrong);
        let mut late = rv.clone();
        late.insert((t.key(), 1), ("CEDAR MESA 77".to_string(), 20 * MIN));
        assert_eq!(check(&s, &late, &t, 25 * MIN, "CEDAR MESA 77"), Check::Late);
    }

    #[test]
    fn typed_words_are_bounded_and_revealed_ones_refused() {
        assert!(plausible("otter lantern 42").is_ok());
        assert!(plausible("ox").is_err());
        assert!(plausible(&format!("OTTER{}LANTERN 42", " ".repeat(1000))).is_err());
        let t = Target::Site("park".into());
        let mut rv = BTreeMap::new();
        rv.insert((t.key(), 3), ("OTTER LANTERN 42".to_string(), 0));
        assert!(revealed(&rv, &t, "otter-lantern 4 2"));
        assert!(!revealed(
            &rv,
            &Target::Fox("park".into()),
            "otter lantern 42"
        ));
    }

    #[test]
    fn the_schedule_covers_every_slot_of_the_game() {
        let s = session();
        let csv = schedule(&[7; 32], &s);
        assert_eq!(
            csv.lines().count(),
            1 + 4,
            "header and four 15-minute slots"
        );
        let t = Target::Site("park".into());
        assert!(csv.contains(&word(&[7; 32], "s1", &t, 3)));
    }
}
