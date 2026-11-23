# Changelog

All notable changes to ridgewarden-game are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is what appears on its GitHub release page. Validated in CI by [patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

### Added

- Each player signs their posts with their own key, made in the browser from their callsign and a passphrase that never leaves the page. A callsign's first check-in on a site's LAN sets its key for every game (in a game with a roster, its first post does); an admin can set another with `player-key`, and the player's page shows it.
- The page wants a passphrase of 12 characters or more, asks for it twice before a post that would set a callsign's key, and can suggest four random words with two digits each.
- A held site goes back to nobody once every link at it has been down for `hold_min` minutes (30 by default), and the game page lists the sites lost.

### Changed

- Every site in a game has to run this version: older sites can't read the new start events, and games with 5 to 8 teams no longer load.
- Fox finds are proven with one-time codes of 16 characters left with the fox (`start --codes`, written to `codes-<id>.csv`) instead of code words sent on air. Each code counts once, for whoever sends it first.
- A game has 2 to 4 teams, down from up to 8.
- Once a game has a roster, the sites refuse posts from callsigns not on it.
- Posting needs JavaScript, since the page signs each post.
- The words and codes files are readable only by their owner, like the seed they come from.

### Fixed

- The Fox finder award said "found 1 foxes".

## [0.1.0] - 2026-11-20

### Added

- Runs as its own system user, `ridgewarden-game`, in the core's `ridgewarden` group: it can sign events through the core but not read the station key.
- Capture the Site for one game at a time: admins start and end a game and set teams with signed events, from keys listed in `admins`.
- Check-ins on a site's own page, accepted only from devices on that node's LAN and signed by that site's core. Other sites count them only from the core that polls that node over its LAN.
- Chains scored every minute from links that are up in the mesh's own link data, through relay nodes (only the links on paths between a team's sites), and broken as soon as a link drops. Scored minutes are kept and only redone when late data could change them, so long games stay quick.
- Builder bonus for new links, once they have shown up in link data on several days, for 90 days after they first appear.
- Diminishing points for repeat captures of one site, and encounter points for both players when two teams meet at a site.
- Travel-time limits between check-ins, with a warning first and three strikes.
- Live map and leaderboard for spectators that works as a plain refreshing page, plus the rules, past games and `/api/game`.
- Scoring rules travel in the start event, so every site scores a game the same way, and a game's result is frozen an hour after it ends.
- Public sites taken with rotating code words heard on air and typed in on any site's page; the admin's app reveals each word in a signed event once its slot is over, and `start` writes every word to a schedule file.
- Fox hunts: hidden transmitters send their own code words, finders and whoever runs the fox both score, and bearings shared on the hunt page show on the map.
- Spots, for telling everyone where you're headed or what's happening.
- Seasons with standings by callsign and team, an archive of every game, a page per player, and POTA-style awards for sites visited, games played, foxes found, encounters, chains held and new links.
- A numbers page: players per game, games with 10 or more players, and the nodes and links the mesh grew by while the app watched.
- A debrief on each game's page: every link that dropped, for how long, and whose chains it broke.
- `safe_sites` in the config; `start` refuses sites that aren't on it.
- A roster closes the game: once an admin puts anyone on it, only rostered callsigns can play.

[Unreleased]: https://github.com/Londopy/ridgewarden-game/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Londopy/ridgewarden-game/releases/tag/v0.1.0
