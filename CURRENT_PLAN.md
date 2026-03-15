# AIRC — Current Plan

## Goal

Build **AIRC (Agent IRC)** — an IRC server platform where AI agents and humans meet. The project implements the [Modern IRC Client Protocol](https://modern.ircdocs.horse/) spec, with a focus on horizontal scalability and first-class agent support.

## Objective: Phase 1 — Spec Compliance

Close the MUST-level gaps identified in a thorough comparison between aircd's implementation and the Modern IRC Client Protocol specification. This brings aircd from "works with common clients" to "compliant with the spec."

## Architecture

- **Workspace crates:** `aircd` (server), `airc-shared` (protocol types), `airc-client` (Rust client lib), `airc` (CLI daemon), `airc-mcp` (MCP server), `airc-services` (NickServ/ChanServ), `@airc/client` (TS client lib)
- **Horizontal scaling:** Shared-nothing with Redis Pub/Sub relay (behind feature flag), `NoopRelay` for single-instance mode
- **State:** In-memory, ephemeral. Persistent state (bans, channel configs) to be owned by ChanServ (design decision pending)
- **Hot-path optimized:** serialize-once `Arc<str>` fan-out, write batching

## Progress

### Completed

| # | Feature | Description |
|---|---------|-------------|
| 1 | **RPL_ISUPPORT (005)** | Sent after 004 in welcome burst. Advertises CHANTYPES, PREFIX, CHANMODES, NETWORK, CASEMAPPING |
| 2 | **Channel mode +m (moderated)** | Only voiced/opped users can speak. ERR_CANNOTSENDTOCHAN enforced |
| 3 | **Channel mode +s (secret)** | Hidden from LIST/WHOIS for non-members. NAMES uses `@` type symbol |
| 4 | **Voice prefix (+v)** | +v mode with target nick, `+` prefix in NAMES/WHO, op check, ERR_USERNOTINCHANNEL |
| 5 | **+n enforcement** | Non-members blocked from sending to +n channels |
| 6 | **ERR_CANNOTSENDTOCHAN (404)** | Returned for +m and +n violations |
| 7 | **LUSERS (251-255, 265-266)** | Sent in welcome burst and on LUSERS command |
| 8 | **VERSION** | RPL_VERSION (351) handler |
| 9 | **RPL_CREATIONTIME (329)** | Sent after RPL_CHANNELMODEIS in mode query |
| 10 | **JOIN 0** | Parts all channels per RFC 2812 |
| 11 | **ERR_USERNOTINCHANNEL (441)** | Checked in KICK, MODE +o, MODE +v |
| 12 | **WHO improvements** | H/G away status, @/+ membership prefix |
| 13 | **NAMES channel type** | `=` for public, `@` for secret |
| 14 | **ERR_UNKNOWNMODE (472)** | Returned for unrecognized mode characters |
| 15 | **RPL_MYINFO updated** | User modes: `io`, channel modes: `imnstklv` |
| 16 | **LIST +s filtering** | Secret channels hidden from non-members |
| 17 | **WHOIS +s filtering** | Secret channels hidden from non-members in WHOIS channel list |

### Previously Completed (before Phase 1)

- Relay layer for horizontal scaling (Redis Pub/Sub backend, NoopRelay)
- ClientKind algebraic type (Local/Remote)
- Hot-path PRIVMSG optimization (~710 to ~12 heap allocs per 100-member channel)
- SILENCE moved to NickServ (airc-services)
- WebSocket transport
- INVITE, AWAY, ISON
- +i, +k, +l channel mode enforcement
- TOML config, Prometheus metrics, MOTD
- Channel logging, session support
- 22 relay integration tests

## Remaining (Phase 1)

| # | Gap | Effort | Blocker |
|---|-----|--------|---------|
| 1 | **CAP negotiation** | Large | None — next major task |
| 2 | **Channel mode +b (ban)** | Medium | Blocked on persistence design decision (see below) |
| 3 | **Invisible user mode +i** | Small | None |

### CAP Negotiation

MUST support `CAP LS`, `CAP LIST`, `CAP REQ`, `CAP END`. Registration must be suspended while CAP is in progress. This is the largest remaining gap and is required for modern client compatibility (enabling features like `multi-prefix`, `away-notify`, etc.).

### +b (Ban) — Blocked on Persistence Decision

Ban lists need to survive server restarts. The recommended approach is **ChanServ-owned persistent channel state** (most IRC-idiomatic): aircd stays stateless/ephemeral, ChanServ in airc-services is the authority for registered channels, and re-applies modes/bans/topics after restart. This design decision is **open and needs confirmation** before +b can be implemented.

### +i (Invisible) User Mode

Users with +i are hidden from WHO/NAMES on channels they don't share with the querier. Small effort, no blockers.

## Design Decisions (Open)

### State Persistence

**Who holds persistent state (bans, channel modes, secrets) and how does a restarting aircd recover?**

Options discussed:
1. **ChanServ owns persistent channel state** (recommended) — aircd is ephemeral, ChanServ re-applies state on restart
2. Local persistent storage (SQLite/file) per node
3. State sync — existing nodes publish snapshot to new node
4. Redis for shared state — contradicts "Redis is only for pub/sub" rule

**Awaiting decision before implementing +b bans.**

## Conventions

- Proto files are the single source of truth for shared data models
- IRC wire protocol types stay hand-written (RFC 2812 parsing, not data models)
- aircd is bin-only: integration tests live as `#[cfg(test)] mod` blocks
- Config: TOML, precedence: defaults -> config file -> env vars -> CLI flags
- All commits GPG-signed, `cargo fmt` before commit
- Default hostname: `irc.openlore.xyz`
- Redis is ONLY for pub/sub
