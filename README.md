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

An admin starts a game from the command line, naming 2 to 4 teams, the check-in sites, any public sites and any foxes. The start event is signed by the admin's core and reaches every site through the cores' sync, so every site's page shows the same game.

Every post crosses the mesh's radio links, so there is one kind of player: a licensed operator playing under their own callsign. Spectators only watch.

A player posts with their callsign, a passphrase and a team. The passphrase never leaves the browser: the page turns it into the player's own key and signs every check-in, code word, bearing and spot with it. A callsign's first check-in on a site's LAN sets its key for every game after (in a game with a roster, its first post does), and an admin can set another with `player-key`. The page wants a passphrase of 12 characters or more, asks for it twice before a post that would set the key, and can suggest one: four random words with two digits each. There's no account to make, and no password is kept anywhere: the log holds only each callsign's public key. There are three ways to play:

- **Check in at a site.** Connect a phone or laptop to that node's LAN, open the site's game page (`http://<pi>:8797/checkin`), and enter callsign, passphrase and team. The site's app checks the player's signature and that the request really comes from a device on the node's LAN, then has its own core sign the check-in too. Other sites believe a check-in only if it is signed by the core that polls that node over its LAN, and only if they trust that core.
- **Code words at public sites.** Where players can't reach the node's LAN (a park, a public lookout), someone at the site sends a code word on air that changes every 15 minutes. Players type what they heard on the code word page (`/word`) of any site. The admin's app reveals each word in a signed event once its slot is over; claims made before the reveal with the right word count then. Sharing a word only helps for the rest of its slot.
- **Fox hunts.** A fox is a hidden transmitter with one-time codes left at it, on a card or with whoever runs it. A hunter who finds it takes a code and sends it from the code word page; each code counts once, for whoever sends it first, so passing yours on gives your find away. Hunters share bearings on the hunt page (`/hunt`), which draws them on the map for everyone, the way POTA hunters share spots.

Spec sheet 05 asks for fox finds proven by a signed beacon code word. A word sent on air only proves that someone heard the fox, which hunters can do from far off, and anyone who hears it can pass it on, so a find is proven with a code you can only get at the fox instead. The cost: someone has to print each fox's codes and leave them with it, and a finder can still carry off spare codes for friends, so leave about one per expected hunter (`--codes`) and count what's left afterwards.

Spectators open the game page on any site (`http://<pi>:8797/`). It shows the scores, a map of the sites and links with each team's chains in its color and the hunt's bearings, the sites held, the foxes, every player, every check-in and code word with what it earned or why it didn't count, spots, and a debrief of every link that dropped during the game and whose chains it broke, and of the sites lost when their links stayed down. The page reloads itself every minute, and a small script refreshes it every 15 seconds if scripts are allowed. Spectators only read; nothing they do is transmitted.

Anyone playing can post a spot (`/spot`): where they're headed, a site they're on air at, a link that looks shaky. Spots earn no points.

## Scoring

Every number below is a setting in `[rules]`. The admin's settings are copied into the start event, so all sites score a game the same way.

1. **Capture.** A check-in or code word at a site your team doesn't hold captures it for your team and earns 10 points. Your team's second capture of the same site earns half that, the third a quarter, and so on, rounded down (`capture_points`, `repeat_factor`), so play spreads across sites instead of two teams trading one site back and forth. A check-in at a site your team already holds earns nothing.
2. **Chains.** Every minute after the start, each chain earns 1 point per site in it (`chain_points`). A chain is two or more of a team's sites joined by links that are up, directly or through relay nodes that aren't game sites. Only links on some path between two of the team's sites count as part of the chain; relay branches and loops that lead nowhere are left off. A site held by nobody or by another team breaks the chain. A lone site earns nothing.
3. **Dropped links.** A link is up at a given minute if a recent poll of either end lists the other. One poll without it is treated as a hiccup, so it counts as gone after two polls in a row without it (the same rule as Link Doctor), or when neither end has been polled for 10 minutes (`fresh_s`). Only `RF` links count by default (`link_types`). When a link drops, every chain through it breaks at once and its points stop until it is back.
4. **Holding a site.** A site stays your team's only while it has a link up. Once every link at it has been down for 30 minutes (`hold_min`), it goes back to nobody and has to be captured again; the game page lists it as lost.
5. **Builder bonus.** A new link in a chain earns 2 more points a minute (`builder_points`). A link is new if one of its ends was polled on an earlier day without it, for 90 days after it first showed up (`builder_window_days`). It earns the bonus only once it has shown up in link data on 3 different UTC days (`builder_days`), so a link that appears for an afternoon doesn't count. The app keeps the days each node and link were seen for as long as it runs; when it is first installed it can only look back as far as its core's 30 days of polls.
6. **Code words.** One try per word per player. A word heard just before it changed still counts for 2 minutes into the next slot. A claim made after the word was revealed doesn't count, since anyone could read it off the page by then.
7. **Encounters.** When players from two teams check in at the same site within 10 minutes of each other (`encounter_min`), both get 5 points (`encounter_points`). Both sides of the meeting get the credit, and each callsign can earn it once per site.
8. **Foxes.** Sending one of a fox's codes earns 20 points (`fox_points`), once per fox per player; a second find of the same fox doesn't use up a code. Whoever runs the fox gets `encounter_points` for each finder, so hiding a good fox pays too.
9. **Travel time.** A player's two check-ins at different sites must be far enough apart in time for the distance between them at 100 km/h (`max_speed_kmh`), using the locations the nodes report. A check-in that's too fast doesn't count. The first is a warning; after 3 (`strikes`) the player is out of the game. Sites with no location set aren't checked, and foxes never are.
10. **Team.** A player keeps the team of their first counted check-in for the whole game, unless an admin put them on a team with `ridgewarden-game teams --player`. Once anyone is on the roster, the game is closed: only rostered callsigns can play.

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

# a hunt night: a public site taken by code word, and two foxes, one run by N0FOX,
# each with 20 one-time codes (30 if --codes is left out)
ridgewarden-game start "Hunt night" --teams Red,Blue --sites tst-cuesta \
    --word-sites tst-paso --fox "Fox 1=N0FOX" --fox "Fox 2" --codes 20 --minutes 120

# print the running game's code words and fox codes again
ridgewarden-game words

# give a callsign another key, for a player who lost their passphrase; the page shows
# the key a passphrase makes when a post is refused
ridgewarden-game player-key N0CALL <64 hex digits>

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

When a game has public sites, `start` writes every code word for every slot to `words-<id>.csv` in the data folder, with each slot's start and end time in UTC. Hand each site's lines to whoever is on air there, and keep the file private until each word is due. When it has foxes, `start` writes each fox's one-time codes to `codes-<id>.csv`: print them and leave each fox's codes with it. The start event carries only hashes of the codes, with a fresh nonce for each game; a claim carries the code itself (see Limits). The seed the words and codes come from is `codewords.key` in the same folder, readable only by its owner; only the site that started a game can reveal its words, so keep that site's game app running until the game is over.

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
hold_min = 30

[site_names]
"tst-cuesta" = "Cuesta Peak"
```

`lan` has to be set by hand. AREDN's `/a/sysinfo` lists the node's interfaces with their addresses but not the LAN's size, and the core doesn't pass interface data on, so the app can't work it out. Copy the LAN network and its size from the node's setup page, such as `10.54.12.0/29`. Leave `lan` empty at sites that aren't check-in points.

A site's app takes posts (check-ins, code words, bearings, spots) only if it can read the core's token; without it the site still shows the game. Posting needs JavaScript, since the page signs each post in the browser; everything else works without it. A check-in has to reach the app directly from the player's device. Don't put the pages behind a reverse proxy: it would hide where posts came from. Posts that arrive with forwarding headers, or from a proxy on the same box, are refused; a proxy on another LAN machine that adds no headers would make every device behind it look local, so don't run one. Requests from the game's own box are refused too, so someone logged in to the Pi from afar can't check in.

The app believes only events from stations the core trusts: its own key, plus the keys listed under `[trust]` in `ridgewarden.toml`. Game starts, team changes, ends, seasons and code word reveals count only from keys in `admins`.

## Limits

- So far the game has run only against the simulated mesh (`ridgewarden-sim`) with a core on one Windows desktop, and its pages only in headless Chromium at desktop and phone widths; the tests also run the page's script under Node. It hasn't been played on real nodes, from real phones on a node's LAN, or with real radios, foxes and Pis in the field.
- A callsign's key is set by its first check-in on a site's LAN, or, in a game with a roster, by its first post, so nobody can take a callsign from afar. Until then anyone can post spots, bearings and code words under it, and whoever first checks in with it on a LAN, or first posts with a rostered one, gets it; an admin can then move it with `player-key`. Nothing checks callsigns against a license database, so anyone can also play under made-up callsigns, each of which can earn encounter and fox points once. Every check-in and code word is listed on the game page with its site and time, and every spot and bearing with its time, so a referee can spot it. For a game with strangers, put every player on the roster with `teams --player` first: the sites then refuse posts from callsigns not on it.
- A player's key is made from their callsign and passphrase, and the key is public in the log, so anyone can guess at the passphrase offline. Each guess costs 65,536 rounds of SHA-512. Measured on one Windows desktop: the page makes a key in about 80 ms in headless Chromium, and OpenSSL tries about 67 passphrases a second on one core, so a list of 14 million common passwords takes one core about a day on average. More cores, or a graphics card (not measured), go much faster. The page refuses passphrases under 12 characters; the four words with two digits each that it suggests (about 54 bits) would take one core millions of years.
- The page and its signing script come over plain HTTP, like everything on the mesh, so anyone who can change traffic between a player's device and the node, on its LAN Wi-Fi or at a hop on the way, could swap the script and read the passphrase as it's typed.
- A check-in counts at a site if it is signed by a core that polls that node over its LAN. That relies on the trusted cores being honest about which node they poll, the same trust every link and location on the page already rests on.
- Proof of presence is as good as the LAN: anyone who can join a node's LAN, by cable or the node's LAN Wi-Fi, can check in there. A LAN access point with a long range or no password widens the site.
- A code word proves someone heard it, or heard it from someone who did, during its slot. Shorter slots (`word_min`) narrow that. A claim waiting for its reveal is in the log, word included, so a player who reads the log can copy it for the rest of that slot. Code word claims are scored only after the word is revealed, up to a slot and two minutes later. A claim's time is the clock of the core that signed it; a site refuses words that were already revealed, but a trusted core with a wrong clock could still misdate claims.
- A fox code claim carries the code as typed, in clear across the mesh, so a hop on the path can see it and send it first. A claim that doesn't count for another reason (not on the roster, out after strikes, the wrong team) leaves its code unspent and readable in the log for anyone to send. Which claim on a code came first follows the clock of the core that logged it.
- Sending the code words on air at public sites is up to people at the site: the app writes the schedule; it doesn't drive a transmitter. A fox can be any transmitter, since its proof is the codes left with it.
- A site counts as held only while the link data shows a link up at it. A site whose node no core polls or crawls, and whose neighbors aren't polled either, goes back to nobody 30 minutes after it is taken.
- Travel-time checks need each node's location set on its setup page. Bearings are entered by hand: the page can't read a phone's location over plain HTTP.
- Scoring is worked out on the page request. After a restart, the first view of a long game works it out from the start once, six hours of polls at a time; for a week-long game on a large mesh that can take most of a minute on a Pi.
- Not done here, because they aren't software: a first game at real sites with real players, the success measure of a new ham who says the game helped them get licensed (ask them), and the spec's open questions about which nodes are safe to visit and the game's name and art.

## Building from source

This repo builds against the core's shared client crate, so check the core out next to it:

```sh
git clone https://github.com/Londopy/ridgewarden
git clone https://github.com/Londopy/ridgewarden-game
cd ridgewarden-game
cargo build --release
cargo test
```

Some tests run the page's signing script under Node (`tests/sign.mjs`), so `cargo test` needs `node` on the PATH.

To try it without a mesh, run the core's `ridgewarden-sim` with `examples/slo-demo.toml`, point a core at one of its nodes with the crawl on, and in `game.toml` set `listen = "127.0.0.2:8797"` and `lan = ["127.0.0.0/8"]`. Then open `http://127.0.0.2:8797/checkin` in a browser on the same machine: it connects from 127.0.0.1, so the app takes it for a device on the node's LAN (tried on Windows). Posts need the browser, since the page signs them. Take links down with the simulator's `/_ctl/link` endpoint to watch chains break.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md), and add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
