-- HedronDB store schema. One file. Never migrate; report drift.
-- Crate copies this to repo-root schema.sql on C0. Store, RoStore, python/hql,
-- and tests execute this file. Do not re-type CREATE TABLE elsewhere.

CREATE TABLE IF NOT EXISTS nodes (
    id TEXT PRIMARY KEY,
    vault_id TEXT NOT NULL,
    type TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    path TEXT,
    version INTEGER NOT NULL,
    tier TEXT NOT NULL,
    importance REAL NOT NULL,
    htec_path TEXT,
    extra TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS edges (
    id TEXT PRIMARY KEY,
    vault_id TEXT NOT NULL,
    from_id TEXT NOT NULL,
    to_id TEXT,
    to_raw TEXT,
    type TEXT NOT NULL,
    properties TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS desired_states (
    id TEXT PRIMARY KEY,
    vault_id TEXT NOT NULL,
    name TEXT NOT NULL,
    state_version INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    last_reconciled INTEGER,
    reconciled_by TEXT,
    importance REAL NOT NULL,
    spec TEXT NOT NULL,
    status TEXT NOT NULL,
    UNIQUE (vault_id, name)
);

CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
    vault_id TEXT NOT NULL,
    ts INTEGER NOT NULL,
    actor TEXT NOT NULL,
    type TEXT NOT NULL,
    data TEXT NOT NULL,
    caused_by TEXT NOT NULL,
    reconciles TEXT,
    supersedes TEXT
);

-- Bad manifest rows. Hedron state only: never written to lattice.db or the manifest.
CREATE TABLE IF NOT EXISTS manifest_quarantine (
    row_hash TEXT PRIMARY KEY,
    date TEXT,
    lanes TEXT NOT NULL,
    reason TEXT NOT NULL,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    cleared_at TEXT,
    acked_by TEXT,
    row_span_start INTEGER NOT NULL,
    row_span_end INTEGER NOT NULL
);
