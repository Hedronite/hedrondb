use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Row};
use uuid::Uuid;

use crate::contracts::validate_spec_shape;
use crate::error::{Error, Result};
use crate::reconcile::{adapt_lapis_observe, reconciler_for, CURRICULUM_CLOCK_KIND};
use crate::types::{
    content_hash, desired_state_hash, is_causal_type, reject_secrets, validate_importance,
    version_ref, Bootstrap, DesiredState, Edge, Event, Node, NodeType, Status, Tier,
    CAUSAL_CAUSED_BY, CAUSAL_RECONCILES, CAUSAL_SUPERSEDES, EDGE_GRANT,
};

/// Schema source: crate-root `schema.sql` (a copy of vault `store.sql`).
/// One file; `Store`, `RoStore`, tests and the Python twin all derive from it.
/// Never migrate an old file — `RoStore::schema_mismatches` reports drift.
pub const SCHEMA_SQL: &str = include_str!("../schema.sql");

const DS_COLUMNS: &str = "id, vault_id, name, state_version, content_hash, last_reconciled, \
                          reconciled_by, importance, spec, status";

#[derive(Clone)]
struct Session {
    agent_id: Uuid,
    vault_id: Uuid,
}

/// Sync handle to a HedronDB store file. Tokens live only in this process.
pub struct Store {
    conn: Connection,
    path: PathBuf,
    sessions: HashMap<String, Session>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path)?;
        lock_store_mode(&path)?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self {
            conn,
            path,
            sessions: HashMap::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open an outer transaction around several writes (import uses this so a
    /// crash leaves no half graph). `reconcile` nests via a savepoint.
    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn rollback(&self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    /// Create an isolated vault and bind an Agent (H.TEC is a path string).
    pub fn bootstrap(
        &mut self,
        vault_name: &str,
        agent_name: &str,
        htec_path: &str,
    ) -> Result<Bootstrap> {
        if vault_name.is_empty() || agent_name.is_empty() || htec_path.is_empty() {
            return Err(Error::Invalid(
                "vault name, agent name, and htec path are required".into(),
            ));
        }

        let vault_id = Uuid::new_v4();
        let vault_extra = serde_yaml::to_value(serde_yaml::Mapping::from_iter([(
            serde_yaml::Value::String("name".into()),
            serde_yaml::Value::String(vault_name.into()),
        )]))?;
        let vault = Node {
            id: vault_id,
            vault_id,
            node_type: NodeType::Vault,
            content_hash: content_hash(NodeType::Vault, Some(vault_name), None, &vault_extra),
            path: Some(vault_name.to_string()),
            version: 1,
            tier: Tier::Cool,
            importance: 1.0,
            htec_path: None,
            extra: vault_extra,
        };
        self.insert_node(&vault)?;

        let agent_extra = serde_yaml::to_value(serde_yaml::Mapping::from_iter([(
            serde_yaml::Value::String("name".into()),
            serde_yaml::Value::String(agent_name.into()),
        )]))?;
        let agent = Node {
            id: Uuid::new_v4(),
            vault_id,
            node_type: NodeType::Agent,
            content_hash: content_hash(
                NodeType::Agent,
                Some(agent_name),
                Some(htec_path),
                &agent_extra,
            ),
            path: Some(agent_name.to_string()),
            version: 1,
            tier: Tier::Hot,
            importance: 1.0,
            htec_path: Some(htec_path.to_string()),
            extra: agent_extra,
        };
        self.insert_node(&agent)?;

        let token = issue_token();
        self.sessions.insert(
            token.clone(),
            Session {
                agent_id: agent.id,
                vault_id,
            },
        );

        Ok(Bootstrap {
            vault,
            agent,
            token,
        })
    }

    pub fn rotate_token(&mut self, token: &str) -> Result<String> {
        let session = self.sessions.remove(token).ok_or(Error::Unauthorized)?;
        let next = issue_token();
        self.sessions.insert(next.clone(), session);
        Ok(next)
    }

    pub fn put_node(&mut self, token: &str, node: Node) -> Result<Node> {
        let session = self.auth(token)?;
        self.ensure_access(session, node.vault_id)?;
        if node.vault_id != session.vault_id && node.node_type == NodeType::Agent {
            return Err(Error::Invalid(
                "agents can only be created in their home vault".into(),
            ));
        }
        validate_importance(node.importance)?;
        reject_secrets(&node.extra)?;
        if node.node_type == NodeType::Agent && node.htec_path.is_none() {
            return Err(Error::Invalid("agent nodes require htec_path".into()));
        }
        self.insert_node(&node)?;
        Ok(node)
    }

    pub fn put_edge(&mut self, token: &str, edge: Edge) -> Result<Edge> {
        let session = self.auth(token)?;
        self.ensure_access(session, edge.vault_id)?;
        if is_causal_type(&edge.edge_type) {
            return Err(Error::Invalid(
                "causal edges are a projection of the event row; append an event instead".into(),
            ));
        }
        if edge.edge_type == EDGE_GRANT {
            return Err(Error::Invalid(
                "grants must be written through grant_access".into(),
            ));
        }
        self.insert_edge(&edge)?;
        Ok(edge)
    }

    /// Owner of `vault_id` grants another agent access. Hivemind/shared still need this row.
    pub fn grant_access(&mut self, token: &str, agent_id: Uuid, vault_id: Uuid) -> Result<Edge> {
        let session = self.auth(token)?.clone();
        if session.vault_id != vault_id {
            return Err(Error::VaultDenied { vault_id });
        }
        let edge = Edge {
            id: Uuid::new_v4(),
            vault_id,
            from: agent_id,
            to_id: Some(vault_id),
            to_raw: None,
            edge_type: EDGE_GRANT.into(),
            properties: serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        };
        self.insert_edge(&edge)?;
        Ok(edge)
    }

    /// Named intent, unique per vault. A second put with the same name upserts
    /// the existing id: spec and hash change; version, status, reconciled_by
    /// stay; no event (events are reconcile-only). `spec.kind` must name a
    /// known reconciler.
    pub fn put_desired_state(
        &mut self,
        token: &str,
        name: &str,
        spec: serde_yaml::Value,
        importance: f64,
    ) -> Result<DesiredState> {
        let session = self.auth(token)?.clone();
        if name.is_empty() {
            return Err(Error::Invalid("desired state name is required".into()));
        }
        validate_importance(importance)?;
        reconciler_for(&spec)?;
        validate_spec_shape(&spec)?;

        if let Some(mut ds) = self.load_desired_state_by_name(session.vault_id, name)? {
            ds.spec = spec;
            ds.importance = importance;
            ds.content_hash = desired_state_hash(&ds.spec, &ds.status, ds.state_version)?;
            persist_desired_state_update(&self.conn, &ds)?;
            return Ok(ds);
        }

        let status = Status::empty();
        let state_version = 1;
        let ds = DesiredState {
            id: Uuid::new_v4(),
            vault_id: session.vault_id,
            name: name.to_string(),
            state_version,
            content_hash: desired_state_hash(&spec, &status, state_version)?,
            last_reconciled: None,
            reconciled_by: None,
            importance,
            spec,
            status,
        };
        self.insert_desired_state(&ds)?;
        Ok(ds)
    }

    /// Refresh status via the spec's reconciler. No version bump, no event.
    pub fn observe(&mut self, token: &str, desired_state_id: Uuid) -> Result<DesiredState> {
        let session = self.auth(token)?.clone();
        let mut ds = self.load_desired_state(desired_state_id)?;
        self.ensure_access(&session, ds.vault_id)?;
        let docs = self.vault_documents(ds.vault_id)?;
        ds.status = reconciler_for(&ds.spec)?.observe(&docs, &ds.spec)?.status;
        ds.content_hash = desired_state_hash(&ds.spec, &ds.status, ds.state_version)?;
        persist_desired_state_update(&self.conn, &ds)?;
        Ok(ds)
    }

    /// Reconcile: observe via the spec's reconciler, bump version + hash,
    /// append the causal event and project its edges — one savepoint, so it
    /// is atomic alone and nests inside `begin`/`commit`.
    pub fn reconcile(
        &mut self,
        token: &str,
        desired_state_id: Uuid,
    ) -> Result<(DesiredState, Event)> {
        let session = self.auth(token)?.clone();
        let mut ds = self.load_desired_state(desired_state_id)?;
        self.ensure_access(&session, ds.vault_id)?;

        let previous = version_ref(ds.id, ds.state_version);
        let docs = self.vault_documents(ds.vault_id)?;
        let observation = reconciler_for(&ds.spec)?.observe(&docs, &ds.spec)?;
        ds.status = observation.status;
        ds.state_version += 1;
        ds.last_reconciled = Some(now_ms());
        ds.reconciled_by = Some(session.agent_id);
        ds.content_hash = desired_state_hash(&ds.spec, &ds.status, ds.state_version)?;

        let event = Event {
            id: Uuid::new_v4(),
            vault_id: ds.vault_id,
            ts: now_ms(),
            actor: session.agent_id,
            event_type: "Reconciled".into(),
            data: serde_yaml::to_value(&ds.status.observed)?,
            caused_by: observation.caused_by,
            reconciles: Some(ds.id),
            supersedes: Some(previous),
        };

        let sp = self.conn.savepoint()?;
        persist_desired_state_update(&sp, &ds)?;
        persist_event(&sp, &event)?;
        project_causal_edges(&sp, &event)?;
        sp.commit()?;
        Ok((ds, event))
    }

    /// Reconcile a `curriculum_clock` desired state from an in-memory emit.
    ///
    /// Same writes as [`Store::reconcile`] (status, version, `Reconciled`
    /// event, causal edges). Facts come from [`adapt_lapis_observe`], not from
    /// vault documents, so lattice rows are not ingested. `caused_by` is empty:
    /// lattice rows have no HedronDB ids. The event `data` still carries
    /// `present` / `missing` / `counts`, with `missing` recomputed by the
    /// reconciler.
    pub fn reconcile_observed(
        &mut self,
        token: &str,
        desired_state_id: Uuid,
        emit: &serde_yaml::Value,
    ) -> Result<(DesiredState, Event)> {
        let session = self.auth(token)?.clone();
        let mut ds = self.load_desired_state(desired_state_id)?;
        self.ensure_access(&session, ds.vault_id)?;
        let kind = ds.spec.get("kind").and_then(serde_yaml::Value::as_str);
        if kind != Some(CURRICULUM_CLOCK_KIND) {
            return Err(Error::Invalid(
                "reconcile_observed is only valid for kind curriculum_clock".into(),
            ));
        }

        let previous = version_ref(ds.id, ds.state_version);
        let observation = adapt_lapis_observe(&ds.spec, emit)?;
        ds.status = observation.status;
        ds.state_version += 1;
        ds.last_reconciled = Some(now_ms());
        ds.reconciled_by = Some(session.agent_id);
        ds.content_hash = desired_state_hash(&ds.spec, &ds.status, ds.state_version)?;

        let event = Event {
            id: Uuid::new_v4(),
            vault_id: ds.vault_id,
            ts: now_ms(),
            actor: session.agent_id,
            event_type: "Reconciled".into(),
            data: serde_yaml::to_value(&ds.status.observed)?,
            caused_by: Vec::new(),
            reconciles: Some(ds.id),
            supersedes: Some(previous),
        };

        let sp = self.conn.savepoint()?;
        persist_desired_state_update(&sp, &ds)?;
        persist_event(&sp, &event)?;
        project_causal_edges(&sp, &event)?;
        sp.commit()?;
        Ok((ds, event))
    }

    /// Warm path: spec vs status now. Does not read the event log.
    pub fn current_state(&self, token: &str, desired_state_id: Uuid) -> Result<DesiredState> {
        let session = self.auth(token)?;
        let ds = self.load_desired_state(desired_state_id)?;
        self.ensure_access(session, ds.vault_id)?;
        Ok(ds)
    }

    /// Cool path: causal / supersession history. Does not return spec vs status.
    pub fn causal_chain(&self, token: &str, desired_state_id: Uuid) -> Result<Vec<Event>> {
        let session = self.auth(token)?;
        let vault_id = self.desired_state_vault(desired_state_id)?;
        self.ensure_access(session, vault_id)?;
        Self::read_causal_chain(&self.conn, desired_state_id)
    }

    /// Event-row reader shared by `causal_chain` and HQL `history`.
    /// Does not return spec vs status. Graph edges are a projection; this reads events.
    pub(crate) fn read_causal_chain(
        conn: &Connection,
        desired_state_id: Uuid,
    ) -> Result<Vec<Event>> {
        let mut stmt = conn.prepare(
            "SELECT id, vault_id, ts, actor, type, data, caused_by, reconciles, supersedes
             FROM events
             WHERE reconciles = ?1
             ORDER BY ts ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![desired_state_id.to_string()], event_from_row)?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }
}

impl Store {
    fn auth(&self, token: &str) -> Result<&Session> {
        self.sessions.get(token).ok_or(Error::Unauthorized)
    }

    fn ensure_access(&self, session: &Session, vault_id: Uuid) -> Result<()> {
        if session.vault_id == vault_id || self.has_grant(session.agent_id, vault_id)? {
            Ok(())
        } else {
            Err(Error::VaultDenied { vault_id })
        }
    }

    fn has_grant(&self, agent_id: Uuid, vault_id: Uuid) -> Result<bool> {
        let found = self.conn.query_row(
            "SELECT 1 FROM edges
             WHERE type = ?1 AND from_id = ?2 AND to_id = ?3
             LIMIT 1",
            params![EDGE_GRANT, agent_id.to_string(), vault_id.to_string()],
            |_| Ok(()),
        );
        match found {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    fn insert_node(&self, node: &Node) -> Result<()> {
        self.conn.execute(
            "INSERT INTO nodes
             (id, vault_id, type, content_hash, path, version, tier, importance, htec_path, extra)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                node.id.to_string(),
                node.vault_id.to_string(),
                node.node_type.as_str(),
                node.content_hash,
                node.path,
                node.version as i64,
                node.tier.as_str(),
                node.importance,
                node.htec_path,
                serde_yaml::to_string(&node.extra)?,
            ],
        )?;
        Ok(())
    }

    fn insert_edge(&self, edge: &Edge) -> Result<()> {
        insert_edge_conn(&self.conn, edge)
    }

    fn insert_desired_state(&self, ds: &DesiredState) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO desired_states ({DS_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
            ),
            params![
                ds.id.to_string(),
                ds.vault_id.to_string(),
                ds.name,
                ds.state_version as i64,
                ds.content_hash,
                ds.last_reconciled,
                ds.reconciled_by.map(|id| id.to_string()),
                ds.importance,
                serde_yaml::to_string(&ds.spec)?,
                serde_yaml::to_string(&ds.status)?,
            ],
        )?;
        Ok(())
    }

    fn load_desired_state(&self, id: Uuid) -> Result<DesiredState> {
        self.conn
            .query_row(
                &format!("SELECT {DS_COLUMNS} FROM desired_states WHERE id = ?1"),
                params![id.to_string()],
                desired_state_from_row,
            )
            .optional()?
            .ok_or(Error::NotFound("desired state"))
    }

    fn load_desired_state_by_name(
        &self,
        vault_id: Uuid,
        name: &str,
    ) -> Result<Option<DesiredState>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {DS_COLUMNS} FROM desired_states WHERE vault_id = ?1 AND name = ?2"
                ),
                params![vault_id.to_string(), name],
                desired_state_from_row,
            )
            .optional()?)
    }

    fn desired_state_vault(&self, id: Uuid) -> Result<Uuid> {
        let raw: String = self
            .conn
            .query_row(
                "SELECT vault_id FROM desired_states WHERE id = ?1",
                params![id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(Error::NotFound("desired state"))?;
        Uuid::parse_str(&raw).map_err(|err| Error::Invalid(err.to_string()))
    }

    /// Every `Document` in the vault, for a reconciler to observe.
    fn vault_documents(&self, vault_id: Uuid) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, vault_id, type, content_hash, path, version, tier, importance, htec_path, extra
             FROM nodes WHERE vault_id = ?1 AND type = ?2",
        )?;
        let rows = stmt.query_map(
            params![vault_id.to_string(), NodeType::Document.as_str()],
            node_from_row,
        )?;
        let mut docs = Vec::new();
        for row in rows {
            docs.push(row?);
        }
        Ok(docs)
    }
}

fn persist_desired_state_update(conn: &Connection, ds: &DesiredState) -> Result<()> {
    let n = conn.execute(
        "UPDATE desired_states
         SET state_version = ?2, content_hash = ?3, last_reconciled = ?4, reconciled_by = ?5,
             importance = ?6, spec = ?7, status = ?8
         WHERE id = ?1",
        params![
            ds.id.to_string(),
            ds.state_version as i64,
            ds.content_hash,
            ds.last_reconciled,
            ds.reconciled_by.map(|id| id.to_string()),
            ds.importance,
            serde_yaml::to_string(&ds.spec)?,
            serde_yaml::to_string(&ds.status)?,
        ],
    )?;
    if n == 0 {
        Err(Error::NotFound("desired state"))
    } else {
        Ok(())
    }
}

fn persist_event(conn: &Connection, event: &Event) -> Result<()> {
    conn.execute(
        "INSERT INTO events
         (id, vault_id, ts, actor, type, data, caused_by, reconciles, supersedes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            event.id.to_string(),
            event.vault_id.to_string(),
            event.ts,
            event.actor.to_string(),
            event.event_type,
            serde_yaml::to_string(&event.data)?,
            serde_yaml::to_string(&event.caused_by)?,
            event.reconciles.map(|id| id.to_string()),
            event.supersedes,
        ],
    )?;
    Ok(())
}

fn project_causal_edges(conn: &Connection, event: &Event) -> Result<()> {
    let ds_id = event
        .reconciles
        .ok_or_else(|| Error::Invalid("reconcile event must set reconciles".into()))?;

    for cause in &event.caused_by {
        let edge = Edge {
            id: Uuid::new_v4(),
            vault_id: event.vault_id,
            from: *cause,
            to_id: Some(ds_id),
            to_raw: None,
            edge_type: CAUSAL_CAUSED_BY.into(),
            properties: serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        };
        insert_edge_conn(conn, &edge)?;
    }

    let reconciles = Edge {
        id: Uuid::new_v4(),
        vault_id: event.vault_id,
        from: event.actor,
        to_id: Some(ds_id),
        to_raw: None,
        edge_type: CAUSAL_RECONCILES.into(),
        properties: serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
    };
    insert_edge_conn(conn, &reconciles)?;

    if let Some(previous) = &event.supersedes {
        let supersedes = Edge {
            id: Uuid::new_v4(),
            vault_id: event.vault_id,
            from: ds_id,
            to_id: None,
            to_raw: Some(previous.clone()),
            edge_type: CAUSAL_SUPERSEDES.into(),
            properties: serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        };
        insert_edge_conn(conn, &supersedes)?;
    }
    Ok(())
}

fn insert_edge_conn(conn: &Connection, edge: &Edge) -> Result<()> {
    conn.execute(
        "INSERT INTO edges (id, vault_id, from_id, to_id, to_raw, type, properties)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            edge.id.to_string(),
            edge.vault_id.to_string(),
            edge.from.to_string(),
            edge.to_id.map(|id| id.to_string()),
            edge.to_raw,
            edge.edge_type,
            serde_yaml::to_string(&edge.properties)?,
        ],
    )?;
    Ok(())
}

fn node_from_row(row: &Row<'_>) -> rusqlite::Result<Node> {
    let id = parse_uuid_row(row.get::<_, String>(0)?)?;
    let vault_id = parse_uuid_row(row.get::<_, String>(1)?)?;
    let node_type = NodeType::parse(&row.get::<_, String>(2)?).map_err(to_sql_err)?;
    let tier = Tier::parse(&row.get::<_, String>(6)?).map_err(to_sql_err)?;
    let extra_raw: String = row.get(9)?;
    let extra: serde_yaml::Value = serde_yaml::from_str(&extra_raw).map_err(to_sql_err)?;
    Ok(Node {
        id,
        vault_id,
        node_type,
        content_hash: row.get(3)?,
        path: row.get(4)?,
        version: row.get::<_, i64>(5)? as u64,
        tier,
        importance: row.get(7)?,
        htec_path: row.get(8)?,
        extra,
    })
}

fn desired_state_from_row(row: &Row<'_>) -> rusqlite::Result<DesiredState> {
    let id = parse_uuid_row(row.get::<_, String>(0)?)?;
    let vault_id = parse_uuid_row(row.get::<_, String>(1)?)?;
    let spec_raw: String = row.get(8)?;
    let status_raw: String = row.get(9)?;
    let spec: serde_yaml::Value = serde_yaml::from_str(&spec_raw).map_err(to_sql_err)?;
    let status: Status = serde_yaml::from_str(&status_raw).map_err(to_sql_err)?;
    let reconciled_by = match row.get::<_, Option<String>>(6)? {
        Some(raw) => Some(parse_uuid_row(raw)?),
        None => None,
    };
    Ok(DesiredState {
        id,
        vault_id,
        name: row.get(2)?,
        state_version: row.get::<_, i64>(3)? as u64,
        content_hash: row.get(4)?,
        last_reconciled: row.get(5)?,
        reconciled_by,
        importance: row.get(7)?,
        spec,
        status,
    })
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    let id = parse_uuid_row(row.get::<_, String>(0)?)?;
    let vault_id = parse_uuid_row(row.get::<_, String>(1)?)?;
    let actor = parse_uuid_row(row.get::<_, String>(3)?)?;
    let data_raw: String = row.get(5)?;
    let caused_raw: String = row.get(6)?;
    let data: serde_yaml::Value = serde_yaml::from_str(&data_raw).map_err(to_sql_err)?;
    let caused_by: Vec<Uuid> = serde_yaml::from_str(&caused_raw).map_err(to_sql_err)?;
    let reconciles = match row.get::<_, Option<String>>(7)? {
        Some(raw) => Some(parse_uuid_row(raw)?),
        None => None,
    };
    Ok(Event {
        id,
        vault_id,
        ts: row.get(2)?,
        actor,
        event_type: row.get(4)?,
        data,
        caused_by,
        reconciles,
        supersedes: row.get(8)?,
    })
}

fn to_sql_err(err: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(err.to_string().into())
}

fn parse_uuid_row(raw: String) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(&raw).map_err(to_sql_err)
}

fn issue_token() -> String {
    format!("hdt_{}", Uuid::new_v4().as_simple())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn lock_store_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
}
