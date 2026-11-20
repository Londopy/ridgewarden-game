# Changelog

All notable changes to ridgewarden-game are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The section for a version is
what appears on its GitHub release page. Validated in CI by
[patchnotes](https://github.com/Londopy/patchnotes).

## [Unreleased]

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

[Unreleased]: https://github.com/Londopy/ridgewarden-game/commits/main
