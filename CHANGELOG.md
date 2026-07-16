# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/phamvanquyit/ZoTunnel/compare/v0.0.2...HEAD
[0.0.2]: https://github.com/phamvanquyit/ZoTunnel/compare/v0.0.1...v0.0.2
[0.0.1]: https://github.com/phamvanquyit/ZoTunnel/releases/tag/v0.0.1
