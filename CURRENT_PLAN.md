# AIRC — Current Plan

## Goal

Build **AIRC (Agent IRC)** — an IRC server platform where AI agents and humans meet. The project implements the [Modern IRC Client Protocol](https://modern.ircdocs.horse/) spec, with a focus on horizontal scalability and first-class agent support.

## Architecture

- **Workspace crates:** `aircd` (server), `airc-shared` (protocol types), `airc-client` (Rust client lib), `airc` (CLI daemon), `airc-mcp` (MCP server), `airc-services` (external service bots), `@airc/client` (TS client lib)
- **Horizontal scaling:** Shared-nothing with Redis Pub/Sub relay (behind feature flag), `NoopRelay` for single-instance mode. **Full state on every node** — no sharding.
- **State:** In-memory (hot path). Write-through to local SQLite via `sqlx`. CRDT-backed for all mutable shared state (bans, modes, registrations). State loaded fully into memory at startup.
- **Hot-path optimized:** serialize-once `Arc<str>` fan-out, write batching. All ban/mode/nick checks are pure in-memory — zero DB, zero network on the read path.

### Services Architecture

NickServ and ChanServ are **embedded into `aircd`** as internal Rust modules, not external processes. They are called via direct function calls from `handler.rs`, eliminating all IPC round trips on ban checks, mode applications, and nick authentication. They are configurable (enable/disable per service in `aircd.toml`).

`airc-services` remains a crate but NickServ and ChanServ are removed from it. It continues to serve as the framework for **external** service bots (bots that connect over TCP as IRC clients). NickServ and ChanServ are peculiar in that they merge deeply with the IRC experience — embedded is the right model for them.

### State Persistence

Every node holds the complete server state in memory. Persistent state uses **CRDTs** (`crdts` crate v7.3.2) for conflict-free distributed merging:

| State | CRDT type | Semantics |
|-------|-----------|-----------|
| Ban/invite/exception lists | `Orswot<BanMask, NodeId>` | Add-biased: concurrent add+remove keeps the ban |
| Channel modes, topics | `LWWReg<T, LamportClock>` | Last-write-wins with logical clock (not wall time) |
| Nick and channel registrations | `Map<Key, LWWReg<Record, Lamport>, NodeId>` | Last-write-wins map entries |

All CRDT mutations are written through to a **local SQLite file** (via `sqlx`). On startup, the full state is loaded from SQLite into memory. On node-to-node connect, nodes exchange per-CRDT SHA3 hashes and sync only diverged CRDTs (CvRDT merge is idempotent).

### Gossip / Multi-node Sync

On every CRDT mutation: serialize the CRDT delta op → publish via Redis Pub/Sub → all peer nodes apply and write through to their local SQLite. Anti-entropy on connect exchanges hashes and merges diverged state.

Redis is **only** for pub/sub. Never used for persistent state storage.

## Conventions

- Proto files are the single source of truth for shared data models
- IRC wire protocol types stay hand-written (RFC 2812 parsing, not data models)
- `aircd` is bin-only: integration tests live as `#[cfg(test)] mod` blocks
- Config: TOML, precedence: defaults → config file → env vars → CLI flags
- All commits GPG-signed, `cargo fmt` before commit
- Default hostname: `irc.openlore.xyz`
- Redis is ONLY for pub/sub

---

## Progress

### Completed (Phase 1 — Spec Compliance)

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

### Previously Completed (pre-Phase 1)

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

---

## Remaining Work

### Phase A — Embed Services into aircd

NickServ and ChanServ move from `airc-services` into `aircd/src/services/`. They become internal modules called directly from `handler.rs`.

**Tasks:**

- [ ] Create `aircd/src/services/` module tree: `mod.rs`, `nickserv/`, `chanserv/`
- [ ] Copy NickServ logic into `aircd/src/services/nickserv/`
  - Identity, FriendList, SilenceList structs
  - REGISTER, IDENTIFY, INFO, GHOST/RELEASE handlers
  - REGISTER-KEY, CHALLENGE, VERIFY (Ed25519) handlers
  - VOUCH, REPORT, REPUTATION handlers
  - FRIEND, SILENCE handlers
- [ ] Copy ChanServ logic into `aircd/src/services/chanserv/`
  - RegisteredChannel, ChanServState structs
  - REGISTER, INFO, SET handlers
  - BAN, UNBAN handlers
  - `check_join()` — currently dead code, will be wired in
- [ ] Replace `ReplyHandle` (sends IRC NOTICE over TCP) with direct `ClientHandle::send_notice()` call
- [ ] Replace JSON file persistence with in-memory structs (temporary — SQLite comes in Phase B)
- [ ] Wire `handler.rs`:
  - `PRIVMSG NickServ` → `services::nickserv::dispatch()`
  - `PRIVMSG ChanServ` → `services::chanserv::dispatch()`
  - `JOIN` → `services::chanserv::check_join()` (unblocks ban enforcement)
  - `WHOIS` → reputation lookup via NickServ service
- [ ] Add `[services]` section to `aircd.toml` with per-service enable/disable flags
- [ ] Add `[services]` parsing to `aircd/src/config.rs`
- [ ] Remove NickServ and ChanServ from `airc-services` crate (keep crate, keep bot framework)
- [ ] Ensure `cargo build --workspace` passes

**Outcome:** Services run inside `aircd` with zero IPC. `airc-services` crate survives as the external bot framework.

---

### Phase 1 Remainder — Spec Compliance

After Phase A is complete, close the three remaining MUST-level gaps.

#### 1. CAP Negotiation (Large)

MUST support `CAP LS`, `CAP LIST`, `CAP REQ`, `CAP END`. Registration must be suspended while CAP negotiation is in progress. Required for modern client compatibility (irssi, weechat, HexChat all send `CAP LS` before `NICK`/`USER`; aircd currently returns `ERR_NOTREGISTERED 451`).

- [ ] Add `Command::Cap { subcommand, params }` variant to `airc-shared/src/message.rs`
- [ ] Add pre-registration state guard: track `cap_negotiating: bool` on connection state
- [ ] Handle `CAP LS [version]` — respond with `CAP * LS :` (empty cap list for now)
- [ ] Handle `CAP LIST` — respond with `CAP <nick> LIST :`
- [ ] Handle `CAP REQ` — respond with `CAP <nick> NAK :<caps>` (reject all until caps are implemented)
- [ ] Handle `CAP END` — clear `cap_negotiating`, proceed with registration if NICK+USER already received
- [ ] Defer welcome burst until both CAP END and NICK+USER are received
- [ ] Update `RPL_MYINFO` / ISUPPORT as needed

#### 2. Channel mode +b (Ban) — unblocked by Phase A

Phase A wires `check_join()` and gives services access to `SharedState`. Phase B (CRDT) is where ban lists get proper persistent storage. The handler wiring and CRDT backing land together in Phase B. See Phase B tasks.

#### 3. Invisible user mode +i (Small)

Users with +i are hidden from WHO/NAMES to users who don't share a channel with them.

- [ ] Add `remove_user_mode()` to `state.rs` (currently only `set_user_mode()` exists)
- [ ] Handle `MODE <nick> +i` — set invisible flag on `ClientInfo::modes`
- [ ] Handle `MODE <nick> -i` — clear invisible flag
- [ ] Filter WHO responses: skip +i users not sharing a channel with the querier
- [ ] Filter NAMES responses: skip +i users not sharing a channel with the querier
- [ ] WHOIS: show +i in mode string

---

### Phase B — CRDT Persistent State + SQLite

Replace in-memory-only structs with CRDT-backed state persisted to local SQLite.

**Tasks:**

- [ ] Add dependencies to `aircd/Cargo.toml`: `crdts = "7.3.2"`, `sqlx` (sqlite feature), `sha3`
- [ ] Define `PersistentState` struct with CRDT fields:
  - `nick_registrations: Map<Nick, LWWReg<Identity, Lamport>, NodeId>`
  - `channel_registrations: Map<ChanName, LWWReg<RegisteredChannel, Lamport>, NodeId>`
  - `ban_lists: Map<ChanName, Orswot<BanMask, NodeId>>`
  - `invite_lists: Map<ChanName, Orswot<Nick, NodeId>>`
- [ ] Define SQLite schema (migrations via `sqlx migrate`)
- [ ] Startup: load full state from SQLite into `PersistentState` CRDTs
- [ ] Write-through helper: every CRDT mutation → `sqlx` upsert
- [ ] Implement `MODE +b/-b` in `handler.rs` using ban CRDT → emit `ERR_BANNEDFROMCHAN (474)` in `check_join()`
- [ ] Update `CHANMODES=` in ISUPPORT to advertise `b` (ban list mode)
- [ ] Migrate NickServ JSON file data → CRDT Identity map
- [ ] Migrate ChanServ JSON file data → CRDT RegisteredChannel map
- [ ] Remove JSON file persistence from embedded services

**Outcome:** All persistent state (bans, registrations) survives restarts. +b ban mode fully functional. `ERR_BANNEDFROMCHAN (474)` emitted.

---

### Phase C — Gossip + Anti-Entropy

Extend the relay layer so CRDT delta ops propagate to all nodes.

**Tasks:**

- [ ] Extend relay message enum with `CrdtDelta { crdt_id: String, op: Vec<u8> }` variant
- [ ] On every CRDT mutation: serialize delta op → publish via `RelayEvent::CrdtDelta`
- [ ] In relay event handler (`server.rs`): deserialize and apply incoming delta ops to local CRDT + write-through to SQLite
- [ ] Anti-entropy on node connect:
  - Exchange `HashMap<CrdtId, Sha3Hash>` with peer
  - For each diverged CRDT: send full CvRDT state, peer merges
- [ ] Ensure CRDT merge is idempotent (CvRDT property — guaranteed by `crdts` crate)
- [ ] Integration test: two nodes, ban set on node A, verify ban enforced on node B after gossip

**Outcome:** Full multi-node consistency for all persistent state without central coordination.
