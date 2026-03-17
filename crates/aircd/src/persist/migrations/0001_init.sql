-- Phase B persistent state schema
-- Each table stores a single CRDT blob per key, serialised with bincode.
-- The causal context is embedded inside the CRDT blob itself.
-- updated_at is a Unix-seconds timestamp used only for diagnostics / monitoring.

-- Ban lists: one row per channel, blob is bincode of Orswot<String, NodeId>
CREATE TABLE IF NOT EXISTS ban_lists (
    channel     TEXT    NOT NULL,
    crdt_blob   BLOB    NOT NULL,
    updated_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    PRIMARY KEY (channel)
);

-- Nick registrations: one row per nick, blob is bincode of the Identity struct
-- (not a CRDT itself — the outer Map<Nick, LWWReg<Identity>> is the CRDT,
--  but we store each entry individually for efficient single-nick lookup).
CREATE TABLE IF NOT EXISTS nick_registrations (
    nick_lower  TEXT    NOT NULL,
    data_blob   BLOB    NOT NULL,   -- bincode of Identity
    clock       INTEGER NOT NULL DEFAULT 0,
    node_id     TEXT    NOT NULL DEFAULT '',
    updated_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    PRIMARY KEY (nick_lower)
);

-- Channel registrations: one row per channel name
CREATE TABLE IF NOT EXISTS channel_registrations (
    channel_lower   TEXT    NOT NULL,
    data_blob       BLOB    NOT NULL,   -- bincode of RegisteredChannel
    clock           INTEGER NOT NULL DEFAULT 0,
    node_id         TEXT    NOT NULL DEFAULT '',
    updated_at      INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    PRIMARY KEY (channel_lower)
);
