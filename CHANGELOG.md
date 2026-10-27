# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.5] - 2026-10-27

### Fixed

- Zombie / half-open sessions no longer block reconnect: same `client_id` always takes over (last writer wins), so a wedged registry slot cannot leave the public host on Traefik 404
- Server shutdown no longer deletes `zo-tunnel-*.yml` routers, avoiding a public 404 window (and permanent 404 if reconnect then hits "already registered")
- Traefik client route is rewritten on every successful register/takeover to repair a missing YAML

## [0.0.4] - 2026-09-06

### Fixed

- Intermittent 502 after a tunnel drop: a reconnecting client can take over a stale session after two missed heartbeats instead of waiting ~75s for the old session to die
- Traefik routes are no longer deleted on disconnect, so reconnects do not wait for a file-provider reload
- A healthy duplicate client with the same name still cannot kick a live session

## [0.0.3] - 2026-08-13

### Fixed

- Yamux sessions no longer drop under traffic: heartbeat I/O no longer blocks the connection driver

## [0.0.2] - 2026-07-16

### Added

- Pre-release checks (`scripts/pre-release.sh` and Release workflow `preflight` gate)
- Docker Hub deploy docs in README and GitHub Release notes

### Changed

- Client binaries: drop Intel macOS (`darwin-amd64`); Apple Silicon only
- Release workflow builds only after fmt/clippy/tests pass

### Removed

- Unused scripts: `build.sh`, `e2e_test.sh`, `release.sh`
- Dependabot (temporarily disabled)

## [0.0.1] - 2026-07-16

### Added

- Initial public release of Zo Tunnel
- Self-hosted HTTP tunnel server with Traefik TLS integration
- Client CLI (`zotunnel`) with foreground and background modes
- Dashboard UI, `/install` and `/download` endpoints
- Docker image publish to Docker Hub on `main`
- Multi-platform client binaries (Linux/macOS, amd64/arm64)

[Unreleased]: https://github.com/phamvanquyit/ZoTunnel/compare/v0.0.5...HEAD
[0.0.5]: https://github.com/phamvanquyit/ZoTunnel/compare/v0.0.4...v0.0.5
[0.0.4]: https://github.com/phamvanquyit/ZoTunnel/compare/v0.0.3...v0.0.4
[0.0.3]: https://github.com/phamvanquyit/ZoTunnel/compare/v0.0.2...v0.0.3
[0.0.2]: https://github.com/phamvanquyit/ZoTunnel/compare/v0.0.1...v0.0.2
[0.0.1]: https://github.com/phamvanquyit/ZoTunnel/releases/tag/v0.0.1
