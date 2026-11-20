# ridgewarden-game

[![CI](https://github.com/Londopy/ridgewarden-game/actions/workflows/ci.yml/badge.svg)](https://github.com/Londopy/ridgewarden-game/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Londopy/ridgewarden-game)](https://github.com/Londopy/ridgewarden-game/releases/latest)
[![changelog](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Londopy/ridgewarden-game/gh-pages/changelog-badge.json)](CHANGELOG.md)
[![License](https://img.shields.io/github/license/Londopy/ridgewarden-game)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange)](https://www.rust-lang.org)
[![Linux](https://img.shields.io/badge/platform-Linux%20(Pi%20%7C%20x86__64)-555)](#building-from-source)

**Capture the Site: a territory game played on a real AREDN mesh.**

Sites are territory and live RF links are the roads between them. Teams visit sites in person and check in on each site's own network. A team scores for every chain of its sites joined by links that are up in the mesh's own link data, so when a link drops, the chain breaks and its points stop. Keeping the network healthy is how you win. Fox hunts, public sites taken with code words heard on air, seasons and awards run on the same signed log.

It is a training and network familiarisation exercise for licensed operators and the people learning alongside them: finding the sites, learning which links hold the mesh together, and seeing what one dropped link does to everything behind it. Awards are a record of what people did; there are no prizes. Before running it in public, talk it over with the AREDN forum and the people who look after your mesh's sites.

It needs a [ridgewarden](https://github.com/Londopy/ridgewarden) core running on the same box. The core polls the AREDN node, signs the game's events and syncs them with the other sites; this app reads the core's event feed. Spec sheet 05 of the AREDN app suite.

## How a game works

An admin starts a game from the command line, naming the teams, the check-in sites, any public sites and any foxes. The start event is signed by the admin's core and reaches every site through the cores' sync, so every site's page shows the same game.

Players identify with their callsign and a team; there are no accounts or passwords. There are three ways to play:

- **Check in at a site.** Connect a phone or laptop to that node's LAN, open the site's game page (`http://<pi>:8797/checkin`), and enter callsign and team. The site's app checks that the request really comes from a device on the node's LAN, then has its own core sign the check-in. Other sites believe a check-in only if it is signed by the core that polls that node over its LAN, and only if they trust that core.
- **Code words at public sites.** Where players can't reach the node's LAN (a park, a public lookout), someone at the site sends a code word on air that changes every 15 minutes. Players type what they heard on the code word page (`/word`) of any site. The admin's app reveals each word in a signed event once its slot is over; claims made before the reveal with the right word count then. Sharing a word only helps for the rest of its slot.
- **Fox hunts.** A fox is a hidden transmitter sending its own rotating code words the same way. Finding it and sending its word is the proof. Hunters share bearings on the hunt page (`/hunt`), which draws them on the map for everyone, the way POTA hunters share spots.

Spectators open the game page on any site (`http://<pi>:8797/`). It shows the scores, a map of the sites and links with each team's chains in its color and the hunt's bearings, the sites held, the foxes, every player, every check-in and code word with what it earned or why it didn't count, spots, and a debrief of every link that dropped during the game and whose chains it broke. The page reloads itself every minute, and a small script refreshes it every 15 seconds if scripts are allowed. Spectators only read; nothing they do is transmitted.

Anyone playing can post a spot (`/spot`): where they're headed, a site they're on air at, a link that looks shaky. Spots earn no points.

## Scoring

Every number below is a setting in `[rules]`. The admin's settings are copied into the start event, so all sites score a game the same way.

1. **Capture.** A check-in or code word at a site your team doesn't hold captures it for your team and earns 10 points. Your team's second capture of the same site earns half that, the third a quarter, and so on, rounded down (`capture_points`, `repeat_factor`), so play spreads across sites instead of two teams trading one site back and forth. A check-in at a site your team already holds earns nothing.
2. **Chains.** Every minute after the start, each chain earns 1 point per site in it (`chain_points`). A chain is two or more of a team's sites joined by links that are up, directly or through relay nodes that aren't game sites. Only links on some path between two of the team's sites count as part of the chain; relay branches and loops that lead nowhere are left off. A site held by nobody or by another team breaks the chain. A lone site earns nothing.
3. **Decay.** A link is up at a given minute if a recent poll of either end lists the other. One poll without it is treated as a hiccup, so it counts as gone after two polls in a row without it (the same rule as Link Doctor), or when neither end has been polled for 10 minutes (`fresh_s`). Only `RF` links count by default (`link_types`). When a link drops, every chain through it breaks at once and its points stop until it is back.
4. **Builder bonus.** A new link in a chain earns 2 more points a minute (`builder_points`). A link is new if one of its ends was polled on an earlier day without it, for 90 days after it first showed up (`builder_window_days`). It earns the bonus only once it has shown up in link data on 3 different UTC days (`builder_days`), so a link that appears for an afternoon doesn't count. The app keeps the days each node and link were seen for as long as it runs; when it is first installed it can only look back as far as its core's 30 days of polls.
5. **Code words.** One try per word per player. A word heard just before it changed still counts for 2 minutes into the next slot. A claim made after the word was revealed doesn't count, since anyone could read it off the page by then.
6. **Encounters.** When players from two teams check in at the same site within 10 minutes of each other (`encounter_min`), both get 5 points (`encounter_points`). Both sides of the meeting get the credit, and each callsign can earn it once per site.
7. **Foxes.** Finding a fox earns 20 points (`fox_points`), once per fox per player. Whoever runs the fox gets `encounter_points` for each finder, so hiding a good fox pays too.
8. **Travel time.** A player's two check-ins at different sites must be far enough apart in time for the distance between them at 100 km/h (`max_speed_kmh`), using the locations the nodes report. A check-in that's too fast doesn't count. The first is a warning; after 3 (`strikes`) the player is out of the game. Sites with no location set aren't checked, and foxes never are.
9. **Team.** A player keeps the team of their first counted check-in for the whole game, unless an admin put them on a team with `ridgewarden-game teams --player`. Once anyone is on the roster, the game is closed: only rostered callsigns can play.

Check-ins outside the game's time, at sites that aren't in the game, or for a team that doesn't exist are shown on the page and don't count.

Every site works the scores out from the same signed events, so they agree once the events have reached them all, as long as the cores' clocks are right (a Pi has no clock of its own; give it NTP or a GPS clock). The builder bonus also depends on how long each site's app has been watching the mesh, so a site installed last week can count fewer links as new. Minutes already scored are kept and only worked out again when late data could change them, so a long game stays quick. A result is frozen once the app has read its core's feed to the end at least an hour after the game ended, so it stays the same after the core prunes the polls it was scored from.

## Seasons, awards and the archive

An admin opens a season with `ridgewarden-game season start NAME` and closes it with `season end`; games started while it is open count towards it. The archive (`/games`) lists every game with its players and winner, `/seasons` shows each season's standings by callsign and by team name, and `/player?call=N0CALL` shows one player's games, sites and awards.

Awards are earned by doing, across every game, in the style of POTA's:

| Award | Tiers | For |
|---|---|---|
| Site visitor | 3, 10, 25 | counted at that many different sites |
| Regular | 3, 10, 25 | playing that many games |
| Fox finder | 1, 5, 20 | finding that many foxes |
| Meet and greet | 5, 25 | meeting players from other teams that many times |
| Chain gang | 3, 5, 8 | being on a team that held a chain of that many sites |
| Builder's friend | 1 | being on a team that scored on a new link |

`/stats` shows the numbers that say whether the game is doing its job: games played, different players, the game with the most players and how many games reached 10, and the nodes and links that appeared on the mesh after the app started watching.

## Install

On a Raspberry Pi or Linux box that already runs the [ridgewarden core](https://github.com/Londopy/ridgewarden):

```sh
curl -fsSL https://github.com/Londopy/ridgewarden-game/releases/latest/download/install.sh | sudo sh
```

Or take the `.deb` from the [releases page](https://github.com/Londopy/ridgewarden-game/releases/latest): `sudo apt install ./ridgewarden-game_<version>_arm64.deb` (`_amd64` on a PC). Either way the download is checked against the release key your core already trusts, `/etc/ridgewarden/game.toml` is written if missing, and the service starts once the core is set up. The page is on port 8797.

Install it at every site that should show the game, and at every check-in site. Only sites that are safe and legal for players to visit can be in a game: many nodes are on private land, so `start` refuses any site not listed in `safe_sites`.

Every release also has each download as a `.torrent`, with its GitHub download as the web seed, and signed `SHA256SUMS.txt` checksums.

The service runs as its own user, `ridgewarden-game`, in the core's `ridgewarden` group, so it can sign events through the core but can't read the station's key. Run commands that touch the app's data as that user, so files keep the right owner: `sudo -u ridgewarden-game ridgewarden-game --config /etc/ridgewarden/game.toml <command>`.

## Usage

```sh
ridgewarden-game init --config /etc/ridgewarden/game.toml
ridgewarden-game run  --config /etc/ridgewarden/game.toml
```

Admins run the game from their own site, whose core signs the events:

```sh
# a 90-minute game with two teams and two check-in sites
ridgewarden-game start "November meeting" --teams Red,Blue --sites tst-cuesta,tst-madonna --minutes 90

# a hunt night: a public site taken by code word, and two foxes, one run by N0FOX
ridgewarden-game start "Hunt night" --teams Red,Blue --sites tst-cuesta \
    --word-sites tst-paso --fox "Fox 1=N0FOX" --fox "Fox 2" --minutes 120

# print the running game's code words again
ridgewarden-game words

# put players on teams, take one off, or rename the teams
ridgewarden-game teams --player N0CALL=Red --player N0TEST=Blue
ridgewarden-game teams --drop N0TEST
ridgewarden-game teams --teams Red,Blue,Green

# end it, saying where the foxes were so the map can show them (this also works
# for a day after a game that ran out by itself)
ridgewarden-game end --fox "Fox 1=35.301,-120.662"

# seasons
ridgewarden-game season start "Winter 2027"
ridgewarden-game season end
```

A game started without `--minutes` ends by itself after a week. `start` warns about any check-in site that no core polls over its LAN yet, since nobody could check in there. Changing the team list during a game drops the check-ins of a team that is no longer listed; set the teams before play starts.

When a game has public sites or foxes, `start` writes every code word for every slot to `words-<id>.csv` in the data folder, with each slot's start and end time in UTC. Hand each site's lines to whoever is on air there, and load each fox's lines into its transmitter; keep the file private until each word is due. The seed the words come from is `codewords.key` in the same folder, readable only by its owner; only the site that started a game can reveal its words, so keep that site's game app running until the game is over.

Pages: `/` the live game (`/?s=<id>` for an earlier one), `/checkin`, `/word`, `/hunt` and `/spot` for players, `/rules` the rules with the running game's numbers, `/games`, `/seasons`, `/awards`, `/player` and `/stats` for the archive, and `/api/game` the scores as JSON.

## Configuration

`/etc/ridgewarden/game.toml` (`init` writes a commented one):

```toml
data_dir = "/var/lib/ridgewarden-game"
listen = "0.0.0.0:8797"

# public keys of the stations that may start and end games and set teams;
# use the same list on every site (`ridgewarden whoami` prints a station's key)
admins = ["<64 hex digits>"]

# this node's LAN; check-ins at this site are taken only from devices on it
lan = ["10.54.12.0/29"]

# nodes that are safe and legal for players to visit; only needed on admins' sites
safe_sites = ["tst-cuesta", "tst-madonna", "tst-paso"]

[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"   # lets this app sign posts and admin events

[rules]
chain_points = 1
capture_points = 10
repeat_factor = 0.5
encounter_points = 5
encounter_min = 10
builder_points = 2
builder_days = 3
builder_window_days = 90
fox_points = 20
word_min = 15
max_speed_kmh = 100
strikes = 3
link_types = ["RF"]
fresh_s = 600

[site_names]
"tst-cuesta" = "Cuesta Peak"
```

`lan` has to be set by hand. AREDN's `/a/sysinfo` lists the node's interfaces with their addresses but not the LAN's size, and the core doesn't pass interface data on, so the app can't work it out. Copy the LAN network and its size from the node's setup page, such as `10.54.12.0/29`. Leave `lan` empty at sites that aren't check-in points.

A site's app takes posts (check-ins, code words, bearings, spots) only if it can read the core's token; without it the site still shows the game. A check-in has to reach the app directly from the player's device. Don't put the pages behind a reverse proxy: it would hide where posts came from. Posts that arrive with forwarding headers, or from a proxy on the same box, are refused; a proxy on another LAN machine that adds no headers would make every device behind it look local, so don't run one. Requests from the game's own box are refused too, so someone logged in to the Pi from afar can't check in.

The app believes only events from stations the core trusts: its own key, plus the keys listed under `[trust]` in `ridgewarden.toml`. Game starts, team changes, ends, seasons and code word reveals count only from keys in `admins`.

## Limits

- Callsigns aren't authenticated. Anyone on a check-in site's LAN, or anyone who can reach a site's code word page, can play under someone else's callsign, which can lock that callsign to the wrong team or use up its travel-time strikes, or under made-up callsigns, each of which can earn encounter and fox points once. Every check-in and code word is listed on the game page with its site and time, so a referee can spot it. For a game with strangers, put every player on the roster with `teams --player` first: that closes the game to everyone else.
- A check-in counts at a site if it is signed by a core that polls that node over its LAN. That relies on the trusted cores being honest about which node they poll, the same trust every link and location on the page already rests on.
- Proof of presence is as good as the LAN: anyone who can join a node's LAN, by cable or the node's LAN Wi-Fi, can check in there. A LAN access point with a long range or no password widens the site.
- A code word proves someone heard it, or heard it from someone who did, during its slot. Shorter slots (`word_min`) narrow that. A claim waiting for its reveal is in the log, word included, so a player who reads the log can copy it for the rest of that slot. Code word claims are scored only after the word is revealed, up to a slot and two minutes later. A claim's time is the clock of the core that signed it; a site refuses words that were already revealed, but a trusted core with a wrong clock could still misdate claims.
- Sending the code words on air is up to people or hardware at the site: the app writes the schedule; it doesn't drive a transmitter. A fox needs a transmitter that can be loaded with the schedule's words, such as a voice keyer or a fox controller that plays recorded audio.
- Travel-time checks need each node's location set on its setup page. Bearings are entered by hand: the page can't read a phone's location over plain HTTP.
- Scoring is worked out on the page request. After a restart, the first view of a long game works it out from the start once, six hours of polls at a time; for a week-long game on a large mesh that can take most of a minute on a Pi.
- Not done here, because they aren't software: the success measure of a new ham who says the game helped them get licensed (ask them), and the spec's open questions about which nodes are safe to visit and the game's name and art.

## Building from source

This repo builds against the core's shared client crate, so check the core out next to it:

```sh
git clone https://github.com/Londopy/ridgewarden
git clone https://github.com/Londopy/ridgewarden-game
cd ridgewarden-game
cargo build --release
cargo test
```

To try it without a mesh, run the core's `ridgewarden-sim` with `examples/slo-demo.toml`, point a core at one of its nodes with the crawl on, and set `lan = ["127.0.0.0/8"]`. Then check in with `curl --interface 127.0.0.5 -d "call=N0CALL&team=Red" http://127.0.0.1:8797/checkin`, and take links down with the simulator's `/_ctl/link` endpoint to watch chains break.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
