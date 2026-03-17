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

### Phase A — Embed Services into aircd ✅ COMPLETE (commit 40f3124)

NickServ and ChanServ move from `airc-services` into `aircd/src/services/`. They become internal modules called directly from `handler.rs`.

**Tasks:**

- [x] Create `aircd/src/services/` module tree: `mod.rs`, `nickserv/`, `chanserv/`
- [x] Copy NickServ logic into `aircd/src/services/nickserv/`
  - Identity, FriendList, SilenceList structs
  - REGISTER, IDENTIFY, INFO, GHOST/RELEASE handlers
  - REGISTER-KEY, CHALLENGE, VERIFY (Ed25519) handlers
  - VOUCH, REPORT, REPUTATION handlers
  - FRIEND, SILENCE handlers
- [x] Copy ChanServ logic into `aircd/src/services/chanserv/`
  - RegisteredChannel, ChanServState structs
  - REGISTER, INFO, SET handlers
  - BAN, UNBAN handlers
  - `check_join()` — currently dead code, will be wired in
- [x] Replace `ReplyHandle` (sends IRC NOTICE over TCP) with direct `ClientHandle::send_notice()` call
- [x] Replace JSON file persistence with in-memory structs (temporary — SQLite comes in Phase B)
- [x] Wire `handler.rs`:
  - `PRIVMSG NickServ` → `services::nickserv::dispatch()`
  - `PRIVMSG ChanServ` → `services::chanserv::dispatch()`
  - `JOIN` → `services::chanserv::check_join()` (unblocks ban enforcement)
  - `WHOIS` → reputation lookup via NickServ service
- [x] Add `[services]` section to `aircd.toml` with per-service enable/disable flags
- [x] Add `[services]` parsing to `aircd/src/config.rs`
- [x] Remove NickServ and ChanServ from `airc-services` crate (keep crate, keep bot framework)
- [x] Ensure `cargo build --workspace` passes

**Outcome:** Services run inside `aircd` with zero IPC. `airc-services` crate survives as the external bot framework.

---

### Phase 1 Remainder — Spec Compliance ✅ COMPLETE (commit 40f3124)

After Phase A is complete, close the three remaining MUST-level gaps.

#### 1. CAP Negotiation ✅

- [x] Add `Command::Cap { subcommand, params }` variant to `airc-shared/src/message.rs`
- [x] Add pre-registration state guard: track `cap_negotiating: bool` on connection state
- [x] Handle `CAP LS [version]` — respond with `CAP * LS :` (empty cap list for now)
- [x] Handle `CAP LIST` — respond with `CAP <nick> LIST :`
- [x] Handle `CAP REQ` — respond with `CAP <nick> NAK :<caps>` (reject all until caps are implemented)
- [x] Handle `CAP END` — clear `cap_negotiating`, proceed with registration if NICK+USER already received
- [x] Defer welcome burst until both CAP END and NICK+USER are received
- [x] Update `RPL_MYINFO` / ISUPPORT as needed

#### 2. Channel mode +b (Ban) — wired in Phase B

Phase A wires `check_join()` and gives services access to `SharedState`. Phase B (CRDT) is where ban lists get proper persistent storage. The handler wiring and CRDT backing land together in Phase B. See Phase B tasks.

#### 3. Invisible user mode +i ✅

- [x] Add `remove_user_mode()` to `state.rs` (currently only `set_user_mode()` exists)
- [x] Handle `MODE <nick> +i` — set invisible flag on `ClientInfo::modes`
- [x] Handle `MODE <nick> -i` — clear invisible flag
- [x] Filter WHO responses: skip +i users not sharing a channel with the querier
- [x] Filter NAMES responses: skip +i users not sharing a channel with the querier
- [x] WHOIS: show +i in mode string

---

### Phase B — CRDT Persistent State + SQLite ✅ COMPLETE

**Tasks:**

- [x] Add dependencies to `aircd/Cargo.toml`: `crdts = "7.3.2"`, `sqlx` (sqlite feature), `sha3`, `bincode` (serde feature)
- [x] Define `PersistentState` struct with CRDT fields:
  - `nick_registrations: Map<Nick, LwwEntry<NickRecord>, NodeId>`
  - `channel_registrations: Map<ChanName, LwwEntry<ChannelRecord>, NodeId>`
  - `ban_lists: Map<ChanName, Orswot<BanMask, NodeId>>`
- [x] Define SQLite schema (inline migrations via `sqlx::query`)
- [x] Startup: load full state from SQLite into `PersistentState` CRDTs
- [x] Write-through helper: every CRDT mutation → `sqlx` upsert
- [x] Implement `MODE +b/-b` in `handler/mode.rs` using ban CRDT → emit `ERR_BANNEDFROMCHAN (474)` in `handle_join()`
- [x] Update `CHANMODES=` in ISUPPORT to advertise `b` (ban list mode)
- [x] Migrate NickServ → CRDT write-through (`register_identity`, `modify_reputation` call `upsert_nick`; startup loads from `all_nicks`)
- [x] Migrate ChanServ → CRDT write-through (`register_channel`, `modify_channel` call `upsert_channel`; startup loads from `all_channels`)
- [x] `RPL_BANLIST (367)` and `RPL_ENDOFBANLIST (368)` added to `airc-shared/src/reply.rs`
- [x] `MODE #chan +b` (no mask) → ban list query sends 367/368

**Outcome:** All persistent state (bans, registrations) survives restarts. +b ban mode fully functional. `ERR_BANNEDFROMCHAN (474)` emitted. NickServ and ChanServ write through to CRDT+SQLite on every mutation.

---

### Phase C — Gossip + Anti-Entropy ✅ COMPLETE

Extend the relay layer so CRDT delta ops propagate to all nodes.

**Tasks:**

- [x] Extend relay message enum with `CrdtDelta`, `AntiEntropyRequest`, `AntiEntropyResponse` variants (`relay/mod.rs`)
- [x] Extend `Relay` trait with `publish_crdt`, `publish_anti_entropy_request`, `publish_anti_entropy_response` methods
- [x] Implement all three new methods as no-ops in `NoopRelay` (`relay/noop.rs`)
- [x] On every CRDT mutation: serialize full CRDT blob → gossip via `gossip_tx` channel → forwarded to relay by server task (`persist/mod.rs`, `server.rs`)
- [x] In relay event handler (`server.rs`): dispatch `CrdtDelta` → `merge_crdt()`, `AntiEntropyRequest` → compare hashes, respond with diverged blobs, `AntiEntropyResponse` → merge all blobs
- [x] Anti-entropy on `NodeUp`: send our `all_crdt_hashes()` to peer; peer responds with any diverged blobs
- [x] `PersistentState`: added `all_crdt_hashes()`, `export_crdt()`, `merge_crdt()` dispatch methods
- [x] Integration test: ban set on node A → CRDT delta gossiped via `PairRelay` → ban present on node B (`crdt_ban_gossips_from_node_a_to_node_b`)
- [x] Integration test: nick registered on node A (pre-populated SQLite) → node B empty → anti-entropy on request → node B converges (`anti_entropy_syncs_nick_from_populated_node_to_empty_node`)
- [x] `cargo build` clean (zero errors, only dead-code warnings)
- [x] `cargo test` — 32 tests pass (30 existing + 2 new Phase C tests)

**Outcome:** Full multi-node consistency for all persistent state without central coordination. CRDT deltas gossip on every mutation; anti-entropy on node connect reconciles diverged state.

---

### Logger Redesign ✅ COMPLETE

**Goal:** Move channel logging out of `SharedState` and into the relay layer so it works correctly in a horizontally-scaled multi-node deployment. The relay already sees every IRC event worth logging — handlers should not call a logger at all.

**Design:**
- `NoopRelay` owns a `FileLogger` internally. Every IRC message passing through `publish()` is intercepted and logged transparently — zero handler call-site changes required.
- `RedisRelay` (future): will register as a pure pub/sub subscriber; the one relay subscriber per cluster sees the complete event stream with no duplication.
- QUIT and NICK are logged once to `_server.csv` (channel = `""`). Per-channel membership can be reconstructed from the monotonic `seq` column and surrounding JOIN/PART events in the global event stream.

**CSV format** (updated — was 5 columns, now 7):
```
seq,node_id,timestamp,event_type,channel,nick,content
```
- `seq` — monotonic u64 counter scoped to this logger instance; enables global event ordering and reconstruction
- `node_id` — originating node identity; enables per-node replay and scaling analysis

**Tasks:**
- [x] Update `common.proto` — add `seq` (uint64, field 1) and `node_id` (string, field 2) to `LogEvent`; renumber existing fields 1–5 → 3–7
- [x] Update `airc-shared/src/log.rs` — add `node_id: String` and `seq: u64` to `FileLoggerInner`; update `FileLogger::new()` to accept `node_id`; update `CSV_HEADER`, `log_event_to_csv`, `log_event_from_csv`; `sanitize_filename("")` → `"_server"`; new tests: `file_logger_stamps_seq_and_node_id`, `file_logger_server_wide_goes_to_server_file`, `roundtrip_server_wide_event`
- [x] Update `relay/noop.rs` — add `logger: FileLogger` field; change `NoopRelay::new(log_dir: Option<PathBuf>)`; implement `log_message()` mapping `IrcMessage` → logger calls for all 8 IRC event types
- [x] Update `main.rs` — pass `cfg.log_dir` as `Option<PathBuf>` to `NoopRelay::new()`
- [x] Remove `logger: ChannelLogger` from `SharedState::Inner`; remove `logger()` accessor; remove `use crate::logger::ChannelLogger`; remove now-unused `PathBuf` import
- [x] Remove all 13 logger call sites from handlers: `channel.rs` (5), `message.rs` (6), `nick.rs` (1 loop removed), `user.rs` (1 loop removed), `connection.rs` (1 loop removed)
- [x] Delete `logger.rs`; remove `mod logger` from `main.rs`
- [x] Fix `airc/src/daemon.rs` — `FileLogger::new()` now requires `node_id`; pass `"client"`
- [x] Add `#[allow(dead_code)]` to `channels_for_client` in `state.rs` (no longer called; kept for future use)
- [x] `cargo build` — zero errors, zero warnings
- [x] `cargo test` — 143 tests pass (65 shared + 29 aircd + 20 airc + 9 doctests + ...)

---

### Phase D — Redis Relay ✅ COMPLETE

Implement `RedisRelay` — the Redis Pub/Sub backend for multi-node horizontal scaling.

**Tasks:**

- [x] Add `redis = { version = "0.27", features = ["tokio-comp"] }` and `base64 = "0.22"` to workspace `Cargo.toml` and `crates/aircd/Cargo.toml`
- [x] Add `serde::Serialize/Deserialize` to `IrcMessage` (wire-string round-trip via `to_string()`/`parse()`) in `airc-shared/src/message.rs`
- [x] Add `serde::Serialize/Deserialize` to `NodeId` in `crates/aircd/src/client.rs`
- [x] Implement `RedisRelay` in `crates/aircd/src/relay/redis.rs`:
  - Single Redis channel `airc:relay` carries all event types
  - JSON envelope with `type` discriminant; CRDT/hash blobs are base64-encoded
  - `publish()` — serializes to JSON envelope, `PUBLISH airc:relay <json>`, also logs via `FileLogger`
  - `subscribe()` — opens dedicated pub/sub connection, spawns subscriber task that deserializes envelopes → `InboundEvent`, filters own node_id
  - Heartbeat: sets `airc:heartbeat:<node_id>` key with 15s TTL; background task refreshes every 5s
  - Node-down watcher: polls `KEYS airc:heartbeat:*` every 6s, emits `NodeDown` for disappeared nodes
  - Publishes `NodeUp` envelope on `subscribe()` call
  - `publish_crdt`, `publish_anti_entropy_request`, `publish_anti_entropy_response` — full implementations
- [x] Re-export `RedisRelay` from `relay/mod.rs`
- [x] Wire backend selection in `main.rs`: branch on `cfg.relay.backend` (`"redis"` → `RedisRelay::new()`, else `NoopRelay`)
- [x] `cargo build` — zero errors, zero warnings
- [x] `cargo test` — 143 tests pass (all existing tests pass unchanged)

**Config (already wired — no changes needed):**
```toml
[relay]
backend = "redis"
redis_url = "redis://127.0.0.1:6379"
```
Env vars: `AIRCD_RELAY_BACKEND=redis`, `AIRCD_RELAY_REDIS_URL=redis://127.0.0.1:6379`

**Outcome:** Full multi-node support via Redis Pub/Sub. All 6 relay event types are propagated. Heartbeat-based node lifecycle detection.

---

### Phase E — `aircd-redis-logger` Sidecar ✅ COMPLETE

Dedicated sidecar process that subscribes to `airc:relay` and writes structured CSV logs for all nodes in a multi-node cluster. One subscriber per cluster = complete event stream, zero duplication.

**Design:**
- `RedisRelay` does **not** log — logging is the sole responsibility of this sidecar.
- `NoopRelay` (single-node) continues to own a `FileLogger` directly in `publish()`.
- `Envelope` decode logic lives in `airc-shared/src/relay.rs` so both `aircd` and `aircd-redis-logger` share it without coupling.
- One `FileLogger` per originating `node_id` — created on demand, stored in a `HashMap<String, FileLogger>`; each stamps its own node_id on CSV rows.
- Only `RelayEvent::Message` is logged. CRDT/anti-entropy events are skipped; `NodeUp`/`NodeDown` are traced only.

**Shared envelope module (`airc-shared/src/relay.rs`):**
- `Envelope` — JSON-tagged enum: `Message`, `NodeUp`, `NodeDown`, `CrdtDelta`, `AntiEntropyRequest`, `AntiEntropyResponse`
- `RelayEvent` — decoded higher-level view (base64 decoded, IRC wire parsed)
- `Envelope::decode()` converts `Envelope` → `RelayEvent`
- `Envelope::sender_node_id()`, `Envelope::to_json()`, `Envelope::from_json()`
- `encode_bytes_map()` / `decode_bytes_map()` — base64 encode/decode for CRDT maps
- Constants: `RELAY_CHANNEL = "airc:relay"`, `HEARTBEAT_KEY_PREFIX = "airc:heartbeat:"`

**Tasks:**

- [x] Create `airc-shared/src/relay.rs` — shared `Envelope`, `RelayEvent`, constants, helpers
- [x] Add `pub mod relay;` to `airc-shared/src/lib.rs`
- [x] Add `serde_json` and `base64` to `airc-shared/Cargo.toml`
- [x] Refactor `crates/aircd/src/relay/redis.rs` — strip `FileLogger`, use shared `Envelope` from `airc_shared::relay`
- [x] Update `crates/aircd/src/main.rs` — `RedisRelay::new(url)` (no `log_dir` arg)
- [x] Add `aircd-redis-logger` to workspace `Cargo.toml` members
- [x] Create `crates/aircd-redis-logger/Cargo.toml`
- [x] Create `crates/aircd-redis-logger/src/main.rs` — Redis pub/sub subscriber + per-node `FileLogger` fan-out
- [x] Create `crates/aircd-redis-logger/Dockerfile`
- [x] `cargo build` — zero errors
- [x] `cargo test` — all tests pass (114 unit + 10 doc-tests)

**Config (env vars):**
```
AIRCD_REDIS_URL=redis://127.0.0.1:6379   # default
AIRCD_LOG_DIR=./logs                      # default
AIRCD_LOGGER_NODE_ID=logger               # default (reserved, unused)
RUST_LOG=info                             # standard tracing
```

**Outcome:** `aircd-redis-logger` runs as a sidecar alongside a Redis-backed cluster. It subscribes once, receives every IRC event from every node, and writes per-node CSV log files. `RedisRelay` is now logging-free.

---

### Phase F — SASL Authentication ✅ COMPLETE

Replace the legacy `PRIVMSG NickServ :IDENTIFY` flow with a proper SASL handshake during connection registration, before the welcome burst. Implements PLAIN and SCRAM-SHA-256 mechanisms per IRCv3.

**Design:**
- SASL mechanisms are isolated state machines behind a `SaslMechanism` trait. Each mechanism is self-contained and testable without server infrastructure.
- `SaslSession` wraps a boxed mechanism and drives the exchange in `connection.rs`.
- Credential lookup is decoupled via a `PasswordLookup` callback (`Box<dyn Fn(&str) -> Option<PasswordRecord> + Send>`), keeping mechanisms independent of NickServ internals.
- `get_identity_sync()` added to `NickServState` for use in the sync lookup closure.
- SHA-256 replaces Rust's non-cryptographic `DefaultHasher` for password storage (`hash_password()` via `sha2` crate).
- `ClientInfo` gains `account: Option<String>`; `identified` is derived from it automatically.

**New files:**
- `crates/aircd/src/sasl/error.rs` — `SaslError` enum
- `crates/aircd/src/sasl/mod.rs` — `SaslMechanism` trait, `SaslStep`, `SaslSession`, `PasswordRecord`, `PasswordLookup`, `new_session()`, `SUPPORTED_MECHANISMS`
- `crates/aircd/src/sasl/plain.rs` — complete PLAIN implementation + 5 unit tests
- `crates/aircd/src/sasl/scram.rs` — SCRAM-SHA-256 implementation (3-step state machine with `State::AwaitingAck` variant)

**Modified files:**
- `crates/airc-shared/src/message.rs` — `Command::Authenticate` variant added
- `crates/airc-shared/src/reply.rs` — SASL numerics 900–908 added
- `crates/aircd/src/main.rs` — `mod sasl;` added
- `crates/aircd/src/client.rs` — `account: Option<String>` added to `ClientInfo`
- `crates/aircd/src/state.rs` — `register_client()` accepts `account: Option<String>`
- `crates/aircd/src/connection.rs` — CAP LS advertises `sasl`; `CAP REQ :sasl` → ACK; `AUTHENTICATE` handled with full SASL exchange loop; 900/903/904 numerics; `authenticated_account` passed to `register_client()`
- `crates/aircd/src/services/nickserv/persist.rs` — `hash_password()` (SHA-256 via `sha2`)
- `crates/aircd/src/services/nickserv/mod.rs` — `hash_password` re-exported; `simple_hash` removed
- `crates/aircd/src/services/nickserv/state.rs` — `get_identity_sync()` using `blocking_read()`
- `crates/aircd/src/services/nickserv/identity.rs` — all uses of `simple_hash` replaced with `hash_password`
- `Cargo.toml` (workspace) + `crates/aircd/Cargo.toml` — `sha2`, `hmac`, `pbkdf2` added

**Tasks:**
- [x] Add `Command::Authenticate` to `airc-shared/src/message.rs`
- [x] Add SASL numerics 900–908 to `airc-shared/src/reply.rs`
- [x] Create `crates/aircd/src/sasl/` module: `error.rs`, `mod.rs`, `plain.rs`, `scram.rs`
- [x] Replace `simple_hash` (non-cryptographic) with `hash_password()` (SHA-256)
- [x] Add `get_identity_sync()` to `NickServState`
- [x] Add `account: Option<String>` to `ClientInfo`; update `register_client()`
- [x] Rewrite `connection.rs` SASL flow: CAP LS, CAP REQ, AUTHENTICATE handler, numerics
- [x] Add `sha2`, `hmac`, `pbkdf2` to workspace and crate `Cargo.toml`
- [x] `cargo build` — zero errors, zero warnings
- [x] `cargo test` — 34 `aircd` tests pass (includes 5 new SASL PLAIN unit tests)
- [x] `cargo fmt` — all files formatted

**Wire exchange (PLAIN example):**
```
C: CAP LS 302
S: CAP * LS :sasl
C: CAP REQ :sasl
S: CAP * ACK :sasl
C: AUTHENTICATE PLAIN
S: AUTHENTICATE +
C: AUTHENTICATE <base64(\0authcid\0password)>
S: 900 * * alice :You are now logged in as alice
S: 903 * :SASL authentication successful
C: CAP END
S: 001 ...  (welcome burst)
```

**Outcome:** SASL PLAIN and SCRAM-SHA-256 fully functional during connection registration. Password hashing upgraded to SHA-256. Mechanisms are independently testable. Connection registration deferred until SASL completes (or fails with 904/906).

---

### Phase G — `airc-client` CAP / SASL Support ✅ COMPLETE

Update the `airc-client` library to negotiate IRCv3 capabilities and authenticate with SASL PLAIN during connection registration.

**Design:**
- The client always sends `CAP LS 302` before NICK/USER so capability negotiation is possible with any IRCv3-capable server.
- A `SaslHandshake` struct (behind `Arc<Mutex>`) acts as the in-progress SASL state machine; it is shared between `connect()` and `handle_message()`.
- New `SaslConfig` type in `config.rs` carries mechanism, account, and password. `ClientConfig` gains `sasl: Option<SaslConfig>` and `with_sasl()` builder.
- New `SaslMechanism` enum: `Plain` | `ScramSha256`. `ScramSha256` is recognized and gracefully aborted (not yet implemented client-side).
- New `IrcEvent` variants: `SaslLoggedIn { account }` and `SaslFailed { code, reason }`.
- The `IrcClient::connect` registration wait loop now buffers all pre-001 events (including `SaslLoggedIn` / `SaslFailed`) and re-emits them after the client is constructed.

**Modified files:**
- `crates/airc-client/Cargo.toml` — added `base64` workspace dep
- `crates/airc-client/src/config.rs` — `SaslMechanism`, `SaslConfig`, `ClientConfig::sasl` field + `with_sasl()` builder
- `crates/airc-client/src/event.rs` — `IrcEvent::SaslLoggedIn` and `IrcEvent::SaslFailed` variants
- `crates/airc-client/src/conn.rs` — **fully rewritten**: CAP LS 302 in registration sequence; `SaslHandshake` state machine; `handle_cap()` and `handle_authenticate()` handlers; 900/903/904/906 numeric handlers; `SaslStep` enum; `Arc<Mutex<Option<SaslHandshake>>>` passed through all I/O tasks
- `crates/airc-client/src/client.rs` — registration wait loop now buffers pre-001 events for re-emission
- `crates/airc-client/src/lib.rs` — `SaslConfig` and `SaslMechanism` added to public re-exports

**Tasks:**
- [x] Add `SaslMechanism` and `SaslConfig` to `config.rs`; add `sasl` field to `ClientConfig`
- [x] Add `IrcEvent::SaslLoggedIn` and `IrcEvent::SaslFailed` variants
- [x] Add `base64` to `airc-client/Cargo.toml`
- [x] Rewrite `conn.rs`: always send `CAP LS 302`, drive SASL state machine via `SaslHandshake`
- [x] Handle `CAP LS` (decide whether to request sasl), `CAP ACK`, `CAP NAK`
- [x] Handle `AUTHENTICATE +` (PLAIN: encode and send credentials)
- [x] Handle 900 (RPL_LOGGEDIN), 903 (RPL_SASLSUCCESS), 904/906 (failure) — send `CAP END`
- [x] Buffer pre-001 events in `IrcClient::connect` for re-emission
- [x] Re-export `SaslConfig`, `SaslMechanism` from `lib.rs`
- [x] `cargo build` — zero errors, zero warnings
- [x] `cargo test` — all tests pass
- [x] `cargo fmt` — all files formatted

**Wire exchange (client-initiated PLAIN):**
```
C: CAP LS 302
C: NICK alice
C: USER alice 0 * :Alice
S: CAP * LS :sasl multi-prefix ...
C: CAP REQ :sasl
S: CAP * ACK :sasl
C: AUTHENTICATE PLAIN
S: AUTHENTICATE +
C: AUTHENTICATE <base64(\0alice\0s3cr3t)>
S: 900 alice alice!alice@host alice :You are now logged in as alice
S: 903 alice :SASL authentication successful
C: CAP END
S: 001 alice :Welcome to the IRC network alice
```

**Outcome:** `airc-client` speaks IRCv3 CAP negotiation on every connection and can authenticate with SASL PLAIN before registration completes. `SaslLoggedIn`/`SaslFailed` events are surfaced to callers. `IrcClient::connect` correctly forwards all pre-001 events.

---

### Phase G (continued) — `conn.rs` Refactor into `handler/` ✅ COMPLETE

`conn.rs` had grown to 816 lines by mixing transport concerns with all protocol logic. The protocol handling has been extracted into a `handler/` subdirectory, leaving `conn.rs` as pure transport.

**New module tree (`crates/airc-client/src/handler/`):**

| File | Responsibility |
|------|---------------|
| `mod.rs` | `ConnContext` struct, `handle_message()` dispatcher, `extract_nick()` helper |
| `cap.rs` | `SaslHandshake`, `SaslStep` types + `handle_cap()` |
| `sasl.rs` | `handle_authenticate()`, PLAIN encoding, 900/903/904/906 handlers |
| `registration.rs` | 001 RPL_WELCOME, 433 ERR_NICKNAMEINUSE, 332 RPL_TOPIC, 353 RPL_NAMREPLY |
| `channel.rs` | JOIN, PART, QUIT, KICK, NICK, TOPIC |
| `message.rs` | PRIVMSG, NOTICE, CTCP ACTION |
| `motd.rs` | 375, 372, 376 |

**`ConnContext`** groups all per-connection shared state passed into handlers:
```rust
pub struct ConnContext {
    pub line_tx: LineSender,
    pub event_tx: mpsc::Sender<IrcEvent>,
    pub state: ClientState,
    pub sasl_state: Arc<Mutex<Option<SaslHandshake>>>,
}
```

**`conn.rs`** now contains only: `tls_connector()`, `extract_host()`, `establish_tls()`, `fallback_plain_addr()`, `connect()`, `spawn_io_tasks()`, `write_loop()`, `read_loop()`. `read_loop` constructs `ConnContext` and calls `handler::handle_message()`.

**Tasks:**
- [x] Create `handler/mod.rs` — `ConnContext`, dispatcher, `extract_nick()`
- [x] Create `handler/cap.rs` — `SaslHandshake`, `SaslStep`, `handle_cap()`
- [x] Create `handler/sasl.rs` — `handle_authenticate()`, PLAIN, 900/903/904/906
- [x] Create `handler/registration.rs` — 001, 433, 332, 353
- [x] Create `handler/channel.rs` — JOIN, PART, QUIT, KICK, NICK, TOPIC
- [x] Create `handler/message.rs` — PRIVMSG, NOTICE, CTCP
- [x] Create `handler/motd.rs` — 375, 372, 376
- [x] Rewrite `conn.rs` to pure transport; call `handler::handle_message()`
- [x] Add `mod handler;` to `lib.rs`
- [x] `cargo build` — zero errors, zero warnings
- [x] `cargo test` — all tests pass
- [x] `cargo fmt`

**Outcome:** `conn.rs` reduced to ~230 lines of pure transport. All protocol logic lives in focused, single-responsibility files under `handler/`. The `ConnContext` pattern eliminates repetitive argument threading across handler functions.

---

### Phase H — Async/Sync Audit Fixes ✅ COMPLETE

Eliminated all 5 async/sync violations found during a codebase audit of `crates/aircd/src/`. Goal: ensure `aircd` uses only async code throughout, maximizing concurrency via Tokio coroutines and preventing CPU bottlenecks on the async executor.

**Issues fixed:**

| # | Severity | File | Fix |
|---|----------|------|-----|
| 1 | 🔴 | `services/nickserv/state.rs` + `connection.rs` | Replaced `blocking_read()` inside `PasswordLookup` sync closure with `identities_snapshot()` async method; snapshot taken before closure construction |
| 2 | 🔴 | `persist/mod.rs` | Replaced `gossip_tx: RwLock<Option<...>>` + `try_write()` (silent failure) with `std::sync::OnceLock<...>` — write-once, infallible |
| 3 | 🔴 | `sasl/scram.rs` | Wrapped `pbkdf2_hmac` (4096-iteration PBKDF2 — ~ms of CPU) in `tokio::task::block_in_place` to avoid stalling the async runtime |
| 4 | 🟡 | `persist/mod.rs` | `std::fs::create_dir_all` → `tokio::fs::create_dir_all(...).await` in `open()` |
| 5 | 🟡 | `persist/mod.rs` | Double-lock antipattern in `all_crdt_hashes()` — `ban_lists`, `friend_lists`, `silence_sets` now collected under a single lock hold |

**Tasks:**
- [x] Fix Issue 1: `identities_snapshot()` async method in `NickServState`; remove `get_identity_sync()`; update `connection.rs` closure
- [x] Fix Issue 2: `gossip_tx` changed to `OnceLock`; `set_gossip_tx` infallible; `gossip()` uses `get()`
- [x] Fix Issue 3: `block_in_place` wrapping `pbkdf2_hmac` in `derive_keys()`
- [x] Fix Issue 4: `tokio::fs::create_dir_all` in `persist/mod.rs:open()`
- [x] Fix Issue 5: single-lock collection for `ban_lists`, `friend_lists`, `silence_sets` in `all_crdt_hashes()`
- [x] `cargo build` — zero errors, zero warnings
- [x] `cargo test` — 119 tests pass
- [x] `cargo fmt`

---

---

### Phase I — S2S Protobuf Protocol + ClientId-based Identity ✅ COMPLETE (performance audit ongoing)

**Goal:** Replace the unstructured IRC wire relay with a typed protobuf S2S protocol. Every user is identified by a stable `ClientId`. Running state synchronization on `NodeUp` via `StateSnapshot`.

**Completed:**
- Steps 1–12: Full protobuf relay, ClientId-based identity, channel membership, relay tests (36 passing)
- StateSnapshot: on `NodeUp`, existing nodes publish a compact `StateSnapshot` protobuf containing all local clients, channels, and memberships. New node applies it idempotently.
- Performance audit fixes (see TODO.md for full list):
  - ON-1: nick_index O(1) lookups
  - ON-2: membership_index for O(channels) → O(client_channels) on QUIT
  - ON-7: all_member_ids() Vec alloc eliminated in PRIVMSG hot path
  - ALLOC-1/2: modes: String → u32 bitfield
  - ASYNC-1/2/3: block_in_place, relay event loop, AntiEntropyResponse spawn
  - LOCK-4/5/6/9/11/13: various simultaneous lock and redundant lock acquisition fixes
  - LOCK-6: channel_nicks_with_prefix — dropped channel lock before acquiring users lock
  - LOCK-9: JOIN — merged key/invite/limit checks into single get_channel() call
  - LOCK-11: PRIVMSG — new check_channel_send() handles +n, +m, and fan-out in one lock
  - MEM-1: rate_limits eviction
  - DB-1: no silent write drops
  - MISC-1: constant-time OPER password
  - MISC-2: send buffer 512→4096
  - RELAY-2: SCAN instead of KEYS
  - TIMEOUT-1: 30s registration timeout

**Next up (in priority order from TODO.md):**
1. RELAY-1 — INBOUND_BUF = 256: increase to 4096 ✅ DONE
2. RELAY-3/4 + TIMEOUT-2 — Redis reconnection; shared pub+heartbeat connection; read timeout ✅ DONE
3. LOCK-8 — all_crdt_hashes(): five sequential lock acquisitions ✅ DONE
4. MEM-2 — challenges HashMap: pending keypair challenges never expire ✅ DONE
5. TIMEOUT-3 — No IPC read timeout ✅ DONE

**Remaining (in priority order from TODO.md):**

*Completed this session:*
- LOCK-10 — MODE: ~10 lock acquisitions per +o/+v ✅ DONE
- ON-5/6/8 ✅ DONE
- ALLOC-3/4/5 ✅ DONE
- MEM-3 ✅ DONE
- DB-2 ✅ DONE
- SVC-1/3 ✅ DONE
- MISC-3 ✅ DONE
- LOCK-12 ✅ DONE

*Next up (in priority order):*
1. LOCK-1 — Single global users RwLock (🔴 Critical — sharding required)
2. LOCK-7 — is_channel_operator_nick etc: O(n) + 2 locks each (resolved by ON-1 once LOCK-1 done)
3. ON-3/4 — LUSERS, ISON O(n) scans
4. Low-priority: LOCK-14, ALLOC-6, ON-9, TIMEOUT-4, DB-3, SVC-2, MISC-4

### `state/` Refactor ✅ COMPLETE

Split `crates/aircd/src/state.rs` (2057 lines, one monolithic `impl SharedState`) into a
`state/` module directory with four focused `impl` files:

| File | Contents |
|------|----------|
| `state/mod.rs` | `Inner`, `SharedState`, `NickError`, `ChannelSendResult`, `PrometheusStats`, `StatsCache`, `new()`, `user_shard()`, infra accessors (`relay`, `services`, `persistent`, `next_client_id`, `server_name`, `config`), `fnv1a_hash`, `is_valid_nick` |
| `state/user.rs` | User registry + remote state + away + user modes + shutdown + WHO helpers |
| `state/channel.rs` | Channel lifecycle, mutations, queries, invite; `local_handles_for_ids` private helper |
| `state/relay.rs` | `relay_publish`, `relay_subscribe`, `build_state_snapshot`, `apply_state_snapshot` |
| `state/stats.rs` | `ensure_stats_cache`, `api_stats`, `api_channels`, `prometheus_stats`, `stats` |

- `state.rs` deleted; `mod state;` in `main.rs` resolves to `state/mod.rs` automatically.
- All call sites unchanged.
- `cargo build -p aircd` — zero errors, 5 pre-existing dead-code warnings.
- `cargo test -p aircd` — 38 tests pass (37 prior + 1 new e2e cluster test).

### `relay_tests.rs` Split + E2E Cluster Test ✅ COMPLETE

Split the monolithic `relay_tests.rs` (1595 lines) into a `src/tests/` subdirectory with focused files, and added the end-to-end cluster join test:

| File | Contents |
|------|----------|
| `tests/mod.rs` | Shared infra: `TestRelay`, `TestClient`, `test_state`, `connect_client`, `make_remote_client` |
| `tests/relay_outbound.rs` | 10 outbound publish tests |
| `tests/relay_inbound.rs` | `setup_with_relay_loop` + 9 inbound event tests |
| `tests/crdt.rs` | `PairRelay`, `setup_pair` + 2 CRDT/anti-entropy tests |
| `tests/snapshot.rs` | 3 snapshot tests (including the new e2e test) |

**Key fix — `ClientIntro` node identity:** `RelayEvent::ClientIntro` now carries an explicit `node_id: NodeId` field alongside the `client`. The `server.rs` handler always rebuilds the client as `Client::new_remote(id, info, node_id)` regardless of what `ClientKind` the `Client` struct carries. This fixes an in-process relay bug where `PairRelay` forwarded Local clients verbatim, causing the receiving node to register them as Local instead of Remote.

**New e2e test:** `node_up_propagates_running_state_to_joining_node` — verifies the full NodeUp → StateSnapshot round-trip via `PairRelay`. Node B starts empty, publishes `NodeUp`, receives a `StateSnapshot` from Node A, and ends up with alice and bob as Remote clients in `#lobby`.

---

## Future Work (Backlog)

### SASL wiring for `airc` CLI and `airc-mcp`

SASL is fully implemented in `airc-client` but not exposed through the CLI or MCP entry points. Wiring it up requires:

1. **Nick registration bootstrapping** — SASL only works against a pre-registered nick. Before SASL flags are useful, users need a way to register. Options:
   - A `airc register <server> <nick> <password>` subcommand (connects, sends `PRIVMSG NickServ :REGISTER`, disconnects)
   - Or rely on operator pre-creating accounts out of band (appropriate for agent deployments)

2. **`airc` CLI** (`crates/airc/src/`):
   - Add `--sasl-account`, `--sasl-password`, `--sasl-mechanism` flags to the `connect` subcommand (`main.rs`)
   - Thread them into `.with_sasl()` in `daemon.rs`

3. **`airc-mcp`** (`crates/airc-mcp/src/`):
   - Add `sasl_account`, `sasl_password` fields to `ConnectParams`
   - Pass corresponding flags to the spawned `airc` binary

4. **Client-side SCRAM-SHA-256** (`crates/airc-client/src/handler/sasl.rs`):
   - Currently aborts gracefully with `AUTHENTICATE *`
   - Implement the 3-step SCRAM exchange (client-first → server-first → client-final)
   - Server side is already fully implemented (`crates/aircd/src/sasl/scram.rs`)

5. **`@airc/client` TypeScript client** (`packages/airc-client-ts/`):
   - Currently has no CAP negotiation and no SASL support
   - Authentication is limited to plaintext `PASS` and `PRIVMSG NickServ :IDENTIFY`
   - `command.ts` does not include `CAP` or `AUTHENTICATE` command types
   - Needs the same Phase G treatment as the Rust client:
     - Add `CAP` and `AUTHENTICATE` to the command type union
     - Add `SaslConfig` to `ClientConfig`
     - Drive `CAP LS 302` → `CAP REQ :sasl` → `AUTHENTICATE PLAIN` handshake in `client.ts`
     - Add `sasl_logged_in` / `sasl_failed` variants to the `IrcEvent` union
     - SCRAM-SHA-256 can follow once PLAIN is working
