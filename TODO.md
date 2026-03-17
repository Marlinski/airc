# aircd Performance Audit — TODO

**Goal:** Production-ready IRC server handling 50,000+ concurrent connections.  
**Severity:** 🔴 Critical · 🟠 High · 🟡 Medium · ⚪ Low

---

## 🔴 Critical

- [x] **ON-1** — `find_client_by_nick` / `find_user_by_nick`: O(n) linear scan on every command  
  `state.rs` — called on every PRIVMSG, WHOIS, KICK, NICK, MODE, ISON  
  50,000 string comparisons per lookup at target scale. The #1 bottleneck.  
  Fix: Add `nick_index: RwLock<HashMap<String, ClientId>>` (lowercase → ClientId). Update atomically in `register_client`, `update_nick`, `remove_client`. All lookups become O(1).

- [x] **LOCK-1** — Single global `users: RwLock<HashMap>` serialises all work  
  `state.rs`  
  Every command handler, every JOIN/PART, every PRIVMSG fan-out, every relay event contends on one lock. Becomes the server-wide bottleneck at scale.  
  Fix: Shard into 64 buckets by `ClientId % 64`, each with its own `RwLock<HashMap>`. Depends on ON-1 (nick index must be a separate structure).

- [x] **ASYNC-1** — `block_in_place` for PBKDF2 blocks worker threads during login storm  
  `sasl/scram.rs:319`  
  Every SCRAM auth stalls a Tokio worker thread for the PBKDF2 duration. Multiple concurrent logins starve all tasks on those threads.  
  Fix: Replace `block_in_place` with `spawn_blocking`. Better long-term: cache derived keys per identity at REGISTER time.

---

## 🟠 High

- [x] **ON-2** — `peers_in_shared_channels`: O(channels × members) on every QUIT and NICK  
  `state.rs`  
  Iterates ALL channels and ALL members to find overlap. 1,000 channels × 100 members = 100,000 ops per QUIT.  
  Fix: Store channel membership directly on each `Client` (`HashSet<ChannelName>`). Iterate the departing client's own channel set (≤50 channels), not all channels.

- [x] **LOCK-2** — `update_nick` TOCTOU: read lock then write lock for nick uniqueness  
  `state.rs`  
  Fixed: single write-lock critical section for both check and insert (resolved as part of ON-1 work).

- [x] **LOCK-3** — `register_client` O(n) scan under write lock  
  `state.rs`  
  Fixed: resolved by ON-1 (nick index makes this an O(1) lookup).

- [x] **LOCK-4** — `remove_client` / `remove_node`: O(channels) sequential write locks on disconnect  
  `state.rs`  
  Fixed: `remove_client` and `remove_remote_client` now read `membership_index` to get only the client's channels, then acquire write locks only on those (O(client_channels) not O(all_channels)).

- [x] **LOCK-5** — `part_all_channels`: holds channels map read lock while acquiring per-channel write locks  
  `state.rs`  
  Fixed: channels map read lock dropped after cloning arcs; per-channel write locks acquired afterward (resolved as part of membership_index work).

- [x] **LOCK-6** — `channel_nicks_with_prefix`: holds channel lock + users lock simultaneously  
  `state.rs`  
  Fixed: collect (client_id, mode_prefix) pairs while holding only the channel lock, then drop it before acquiring the users lock.

- [x] **LOCK-7** — `is_channel_operator_nick`, `can_speak_in_channel`, `is_channel_member`: O(n) + 2 locks each  
  `state.rs`  
  Each calls `find_user_by_nick` (O(n)) then acquires a channel lock. Called on every PRIVMSG, MODE, KICK.  
  Fix: Resolved by ON-1. Once nick→ClientId is O(1), all these become O(1) HashSet lookups.

- [x] **ON-3** — `local_client_count`, `oper_count`: O(n) scans for simple counts  
  `state.rs` — called from LUSERS, Prometheus scrape  
  Fixed: `server_counts()` method iterates all shards once, collecting both counts in a single pass. LUSERS uses this instead of three separate calls.

- [x] **ON-4** — ISON: O(n × m) for n requested nicks × m connected users  
  `handler/user.rs`  
  Calls `find_client_by_nick` (O(n)) for each nick in the ISON parameter list.  
  Fix: Resolved by ON-1 (each lookup becomes O(1) via nick_index).

- [x] **ON-5** — WHO `*`: clones entire users HashMap (50k entries)  
  `handler/user.rs`  
  `all_clients()` allocates a 50,000-entry clone. Should iterate in-place under read lock.  
  Fix: Iterate under read lock directly, stream results. Add rate limiting per client for WHO `*`.

- [x] **ALLOC-1/2** — `ClientInfo.modes` as `String`: linear scan + full struct clone on every mode change  
  `client.rs`  
  Fixed: `modes: String` replaced with `modes: u32` bitfield. `user_mode` module with `INVISIBLE/OPER/SERVICE` constants. `has_mode`/`with_mode`/`without_mode` operate on bits. No heap alloc for mode storage.

- [x] **MEM-1** — `rate_limits` HashMap grows without bound  
  `services/nickserv/state.rs:46`  
  Every unique `"sender:action:target"` combination stays in memory forever. Unbounded growth.  
  Fix: Background eviction task every 60s removing entries older than the cooldown. Or cap with LRU.

- [x] **TIMEOUT-1** — No registration timeout on TCP connections  
  `connection.rs`  
  A TCP client that connects but never sends NICK/USER holds a task and fd indefinitely. Trivial resource exhaustion attack.  
  Fix: 30-second deadline timer after TCP accept. Close with `ERROR :Registration timeout` if not registered in time.

- [x] **RELAY-1** — `INBOUND_BUF = 256`: relay channel too small  
  `relay/redis.rs`  
  256 entry capacity causes backpressure to Redis during any burst of inter-node traffic. Messages queue up in TCP buffers, risking loss.  
  Fix: Increase to 4096 minimum. Make configurable.

- [x] **RELAY-2** — `KEYS *` in node-down watcher: O(n) blocking Redis scan  
  `relay/redis.rs:364`  
  `KEYS *` blocks all Redis commands while it scans the entire keyspace. Explicitly dangerous in production.  
  Fix: Replace with `SCAN 0 MATCH <pattern> COUNT 100` cursor iteration, or use a Redis `SET` to track alive nodes.

- [x] **DB-1** — `enqueue_write` silently drops SQLite writes when queue full  
  `persist/mod.rs:602`  
  `try_send` on a full channel silently discards persistence operations (nick registrations, bans, etc.).  
  Fix: Expose a metric counter for dropped write ops. For critical writes (REGISTER), use `send().await` with timeout so failures are observable.

- [x] **ASYNC-2** — Relay event processing inline on accept loop  
  `server.rs`  
  Fixed: relay events forwarded via unbounded channel to a dedicated handler task, fully decoupled from the accept loop.

- [x] **ASYNC-3** — `AntiEntropyResponse` CRDT merge on accept loop  
  `server.rs`  
  Fixed: `AntiEntropyResponse` is matched before dispatching to `handle_relay_event` and spawned into its own task.

- [x] **MISC-1** — OPER password comparison is not timing-safe  
  `handler/server.rs`  
  `if password == oper.password` allows timing side-channel attacks against OPER credentials.  
  Fix: Use `subtle::ConstantTimeEq`.

- [x] **MISC-2** — `SEND_BUFFER = 512`: too small, silent message loss on overflow  
  `connection.rs`  
  Clients in busy channels can overflow 512 messages silently. Standard IRC behaviour is `ERROR :SendQ exceeded`.  
  Fix: Increase to 2048 (configurable). Send `ERROR :SendQ exceeded` before closing on overflow rather than silently discarding.

---

## 🟡 Medium

- [x] **LOCK-8** — `all_crdt_hashes()`: five sequential lock acquisitions  
  `persist/mod.rs`  
  Fixed: all five read guards acquired simultaneously inside a single block, clones taken, then all dropped at once.

- [x] **LOCK-9** — JOIN: triple separate `get_channel()` calls (3 lock acquisitions)  
  `handler/channel.rs`  
  Fixed: merged key check (+k), invite-only check (+i), and member limit check (+l) into a single `get_channel()` call.

- [x] **LOCK-10** — MODE: ~10 lock acquisitions + O(n) scans per mode flag  
  `handler/mode.rs`  
  `MODE #channel +oooo` triggers ~40 lock acquisitions.  
  Fix: Resolve all target nicks to ClientIds under a single users read lock, then apply all mutations in a single channel write-lock section.

- [x] **LOCK-11** — PRIVMSG: 3 separate lock acquisitions before fan-out  
  `handler/message.rs`  
  Fixed: new `check_channel_send(channel, sender_id) -> ChannelSendResult` method in `state.rs` performs +n, +m, and fan-out target collection under a single channel lock acquisition. Also resolves ON-7 inline.

- [x] **LOCK-12** — LUSERS: 3 separate lock acquisitions for simple counts  
  `handler/server.rs`  
  Three independent lock acquisitions for counts that can be inconsistent with each other.  
  Fix: Single `stats_snapshot()` method. Resolved by ON-3 (atomic counters).

- [x] **LOCK-13** — `modify_channel`: holds write lock across `upsert_channel` await  
  `services/chanserv/state.rs`  
  Fixed: mutation applied and record cloned inside the write lock; lock dropped before calling `upsert_channel` (async Redis gossip). Also fixed same pattern in `register_channel`.

- [x] **ON-6** — KICK: double O(n) lookup  
  `handler/channel.rs`  
  `find_client_by_nick` (O(n)) + `is_channel_member` (O(n)) sequentially.  
  Fix: Resolved by ON-1.

- [x] **ON-7** — `all_member_ids()`: allocates `Vec` on every PRIVMSG fan-out  
  `channel.rs`  
  Fixed inline in `check_channel_send`: iterates `ch.members.keys()` directly inside the lock without allocating via `all_member_ids()`.

- [x] **ON-8** — `/api/channels` and Prometheus: O(channels) lock acquisitions per scrape  
  `web.rs`, `state.rs`  
  10,000 channels = 10,000 lock acquisitions per HTTP stats request.  
  Fix: Cache stats result for 1 second. Store member counts as atomics updated on join/part.

- [x] **ALLOC-3** — `Channel::clone()`: clones 4 HashSets  
  `channel.rs`  
  Full channel clone copies members, operators, voiced, invited. Called in `get_channel()` which is called frequently.  
  Fix: Have callers work with `RwLockReadGuard<Channel>` and extract only what they need.

- [x] **ALLOC-4** — WebSocket: `line.to_string().into()` per outgoing frame  
  `web.rs:132`  
  Allocates a new `String` per line when the source is already an `Arc<str>`.  
  Fix: Convert `Arc<str>` directly to `String` via `(*line).to_owned()` or avoid the intermediate copy.

- [x] **ALLOC-5** — CRDT mutation: `orswot.clone()` for serialisation under write lock  
  `persist/mod.rs`  
  Full CRDT clone for snapshot while holding the map write lock.  
  Fix: Drop the write lock, then serialise. Or use delta encoding.

- [x] **MEM-2** — `challenges` HashMap: pending keypair challenges never expire  
  `services/nickserv/state.rs:44`  
  Fixed: Added `created_at: u64` to `PendingChallenge`. `set_challenge` evicts entries older than 5 minutes before inserting. `take_challenge` also rejects expired entries.

- [x] **MEM-3** — CRDT maps: tombstone accumulation  
  `persist/mod.rs`  
  Orswot CRDTs grow in memory as removed elements leave causal tombstones.  
  Fix: Periodic CRDT compaction pass discarding tombstones once all nodes have processed the removes.

- [x] **RELAY-3** — No pub/sub reconnection on Redis connection drop  
  `relay/redis.rs`  
  Fixed: Subscriber task is now a reconnect loop with exponential backoff (500ms → 30s). On stream end or read timeout the loop reconnects automatically.

- [x] **RELAY-4** — Three separate Redis connections per node  
  `relay/redis.rs`  
  Fixed: Heartbeat task now clones the shared `MultiplexedConnection` initialized in `subscribe()` instead of opening a new connection. Publisher and heartbeat share the same connection.

- [x] **DB-2** — `SqlitePool` created with implicit default settings  
  `persist/mod.rs`  
  Intent (single-writer) is not explicit in the pool configuration.  
  Fix: Use `SqlitePoolOptions::new().max_connections(1).connect(...)`.

- [x] **TIMEOUT-2** — No reconnection on Redis pub/sub subscriber drop  
  `relay/redis.rs`  
  Fixed: Each pub/sub read is wrapped in a 30s `tokio::time::timeout`. Timeout or stream end triggers the reconnect loop (see RELAY-3).

- [x] **TIMEOUT-3** — No read timeout on IPC socket  
  `ipc.rs`  
  Fixed: `read_frame` is wrapped in `tokio::time::timeout(5s)`. Stalled IPC clients are disconnected after 5 seconds.

- [x] **SVC-1** — `check_join`: two extra lock acquisitions on every JOIN  
  `handler/channel.rs:101`  
  NickServ identities lock + ChanServ channels lock acquired even for unregistered nicks/channels.  
  Fix: Short-circuit immediately if NickServ disabled or nick unregistered. Skip ChanServ lock if channel has no registered entry.

- [x] **SVC-3** — `check_join`: ban list cloned on every JOIN  
  `services/chanserv/state.rs:158`  
  `reg.bans.clone()` called from under the channels read lock on every JOIN.  
  Fix: Iterate `reg.bans` directly inside the lock guard without cloning.

- [x] **MISC-3** — `glob_match` duplicated in two places  
  `persist/mod.rs` and `services/chanserv/persist.rs`  
  Identical implementation in two files.  
  Fix: Move to `airc_shared` or a shared module.

---

## ⚪ Low

- [x] **LOCK-14** — `rate_limits` write-lock contention under load  
  `services/nickserv/state.rs:46`  
  Fixed: replaced `AsyncRwLock<HashMap>` with `DashMap` — lock-free reads, fine-grained sharded writes. No `await` needed on access.

- [ ] **ALLOC-6** — `hash_password` allocates on every SASL PLAIN auth  
  `services/nickserv/persist.rs`  
  Minor; SHA-256 is fast enough. Acceptable as-is.

- [x] **ON-9** — `all_silence_lists`: N separate lock acquisitions  
  `persist/mod.rs`  
  Fixed: acquires `silence_sets` and `silence_reasons` read locks once each, iterates both maps in a single pass, then drops both guards.

- [x] **TIMEOUT-4** — WebSocket: no ping/idle timeout  
  `web.rs:148`  
  Fixed: `WsCtrl` channel added between reader and writer tasks. Ping sent every 60s via `tokio::time::interval`; connection closed if no pong received within 90s.

- [x] **DB-3** — Write task batch cap of 256 may create many small SQLite transactions  
  `persist/mod.rs`  
  Fixed: cap increased from 256 to 1024 — fewer, larger transactions under burst load.

- [ ] **SVC-2** — `hash_password` called twice per IDENTIFY/GHOST  
  `services/nickserv/identity.rs`  
  SHA-256 twice is negligible. Acceptable as-is.

- [x] **MISC-4** — IPC frame size limit: 16 MiB  
  `ipc.rs:164`  
  Fixed: reduced from 16 MiB to 64 KiB. IPC messages are tiny; the old limit allowed a local attacker to force a 16 MiB allocation per connection.
