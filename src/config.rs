//! `/etc/ridgewarden/game.toml`.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ridgewarden_client::util::{norm_node, valid_node};
use serde::{Deserialize, Serialize};

use crate::game::Rules;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub core: CoreCfg,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// public keys (hex) of the stations that may start and end games and set teams;
    /// keep the same list on every site
    #[serde(default)]
    pub admins: Vec<String>,
    /// this node's LAN, e.g. ["10.54.12.0/29"]; check-ins are taken only from devices on
    /// it. Empty means nobody can check in at this site.
    #[serde(default)]
    pub lan: Vec<String>,
    /// nodes that are safe and legal for players to visit; `start` refuses any other site
    #[serde(default)]
    pub safe_sites: Vec<String>,
    /// how games started from this site are scored
    #[serde(default)]
    pub rules: Rules,
    /// friendly site names: node name -> label, e.g. "tst-cuesta" = "Cuesta Peak"
    #[serde(default)]
    pub site_names: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CoreCfg {
    #[serde(default = "d_core_url")]
    pub url: String,
    /// the core's `local.token`; needed to take check-ins and for the admin commands
    #[serde(default = "d_token")]
    pub token_file: PathBuf,
}

fn d_core_url() -> String {
    "http://127.0.0.1:8790".into()
}
fn d_token() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/local.token")
}
fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden-game")
}
fn d_listen() -> SocketAddr {
    "0.0.0.0:8797".parse().unwrap()
}

impl Default for CoreCfg {
    fn default() -> Self {
        Self {
            url: d_core_url(),
            token_file: d_token(),
        }
    }
}

/// An IP network such as 10.54.12.0/29.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Net {
    addr: IpAddr,
    prefix: u8,
}

impl Net {
    pub fn parse(s: &str) -> Result<Self> {
        let (a, p) = s
            .trim()
            .split_once('/')
            .with_context(|| format!("{s:?} needs a prefix length, like 10.54.12.0/29"))?;
        let addr: IpAddr = a
            .parse()
            .with_context(|| format!("{a:?} is not an address"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix: u8 = p
            .parse()
            .ok()
            .filter(|p| *p <= max)
            .with_context(|| format!("{p:?} is not a prefix length for {a}"))?;
        Ok(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        // an IPv4 client on a dual-stack socket shows up as ::ffff:a.b.c.d
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(n), IpAddr::V4(i)) => {
                let mask = u32::MAX.checked_shl(32 - self.prefix as u32).unwrap_or(0);
                u32::from(n) & mask == u32::from(i) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(i)) => {
                let mask = u128::MAX.checked_shl(128 - self.prefix as u32).unwrap_or(0);
                u128::from(n) & mask == u128::from(i) & mask
            }
            _ => false,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        for a in &mut cfg.admins {
            *a = a.to_ascii_lowercase();
        }
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(a) = self
            .admins
            .iter()
            .find(|a| a.len() != 64 || !a.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            bail!(
                "admins: {a:?} is not a public key (64 hex digits, as `ridgewarden whoami` prints)"
            );
        }
        self.lan_nets()?;
        if let Some(n) = self.safe_sites.iter().find(|n| !valid_node(&norm_node(n))) {
            bail!("safe_sites: {n:?} is not a node name");
        }
        self.rules.validate().context("in [rules]")
    }

    pub fn lan_nets(&self) -> Result<Vec<Net>> {
        self.lan
            .iter()
            .map(|s| Net::parse(s).context("in lan"))
            .collect()
    }

    /// Whether a node is on the list of sites players may visit.
    pub fn is_safe(&self, node: &str) -> bool {
        self.safe_sites.iter().any(|n| norm_node(n) == node)
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("game.db")
    }

    pub fn label(&self, node: &str) -> String {
        self.site_names
            .iter()
            .find(|(k, _)| norm_node(k) == node)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| node.to_string())
    }
}

pub const EXAMPLE: &str = r#"# ridgewarden-game configuration
data_dir = "/var/lib/ridgewarden-game"
listen = "0.0.0.0:8797"

# Public keys of the stations that may start and end games and set teams. Use the same
# list on every site; game events from anyone else are ignored.
admins = []

# This node's LAN. Check-ins at this site are taken only from devices on it. AREDN's
# sysinfo doesn't report the LAN's size, so set it from the node's setup page, e.g.
# ["10.54.12.0/29"]. Leave it empty if this site isn't a check-in point.
lan = []

# Nodes that are safe and legal for players to visit: sites the mesh group runs, or public
# places whose owners agree. Many nodes are on private land; `start` refuses any site not here.
# Only needed on the admins' sites.
safe_sites = []

[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"

# How games started from this site are scored. They are copied into the start event, so
# every site scores the game the same way whatever its own settings say.
[rules]
chain_points = 1         # per minute, for each site in a chain of two or more
capture_points = 10      # for taking a site
repeat_factor = 0.5      # each repeat capture of the same site by one team is worth this much of the last
encounter_points = 5     # to both players when two teams check in at one site close in time
encounter_min = 10
builder_points = 2       # extra per minute for each new link in a chain
builder_days = 3         # distinct days a new link must have been seen before it earns that
builder_window_days = 90 # a link stops counting as new this many days after it first showed up
fox_points = 20          # for finding a fox; whoever runs it gets encounter_points per finder
word_min = 15            # how long each code word lasts at public sites
max_speed_kmh = 100      # faster than this between two check-ins is an impossible hop
strikes = 3              # impossible hops before a player is out of the game
link_types = ["RF"]      # link types that join sites; add "DTD" to count cabled links
fresh_s = 600            # link data older than this counts as unknown, and unknown doesn't score
hold_min = 30            # a held site goes back to nobody once every link at it is down this long

[site_names]
# "tst-cuesta" = "Cuesta Peak"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let c: Config = toml::from_str(EXAMPLE).unwrap();
        c.validate().unwrap();
        assert_eq!(c.listen.port(), 8797);
        assert_eq!(c.rules, Rules::default());
    }

    #[test]
    fn lan_membership() {
        let n = Net::parse("10.54.12.0/29").unwrap();
        assert!(n.contains("10.54.12.1".parse().unwrap()));
        assert!(n.contains("10.54.12.7".parse().unwrap()));
        assert!(!n.contains("10.54.12.8".parse().unwrap()));
        assert!(!n.contains("10.1.2.3".parse().unwrap()));
        assert!(n.contains("::ffff:10.54.12.5".parse().unwrap()));
        assert!(
            Net::parse("0.0.0.0/0")
                .unwrap()
                .contains("8.8.8.8".parse().unwrap())
        );
        assert!(Net::parse("10.54.12.0").is_err());
        assert!(Net::parse("10.54.12.0/33").is_err());
    }

    #[test]
    fn admin_keys_must_be_keys() {
        let mut c: Config = toml::from_str(EXAMPLE).unwrap();
        c.admins = vec!["N0CALL".into()];
        assert!(c.validate().is_err());
        c.admins = vec!["ab".repeat(32)];
        c.validate().unwrap();
    }
}
