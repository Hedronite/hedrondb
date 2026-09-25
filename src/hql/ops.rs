//! HQL operators over typed rows. `state` never reads events; `history`
//! never reads spec / status.

use std::collections::{HashMap, HashSet};

use crate::error::{Error, Result};
use crate::hql::row::{DesiredStateView, EdgeView, NodeView, Row};
use crate::hql::store::RoStore;

/// Node index + outgoing edges, built once per `run`.
pub(crate) struct Graph<'a> {
    pub by_id: &'a HashMap<String, NodeView>,
    pub outgoing: &'a HashMap<String, Vec<EdgeView>>,
}

impl Graph<'_> {
    fn edges_from(&self, id: &str) -> &[EdgeView] {
        self.outgoing.get(id).map(Vec::as_slice).unwrap_or(&[])
    }
}

pub(crate) fn search_hit(row: &Row, needle: &str, graph: &Graph<'_>) -> bool {
    if row.searchable_text().contains(needle) {
        return true;
    }
    let Some(source_id) = row.node_id().or(row.from_id()) else {
        return false;
    };
    for edge in graph.edges_from(source_id) {
        let blob = format!(
            "{}\n{}",
            edge.to_raw.as_deref().unwrap_or(""),
            edge.properties
        );
        if blob.contains(needle) {
            return true;
        }
        if let Some(dest) = edge.to_id.as_ref().and_then(|id| graph.by_id.get(id)) {
            if dest.searchable_text_node().contains(needle) {
                return true;
            }
        }
    }
    false
}

impl NodeView {
    fn searchable_text_node(&self) -> String {
        format!("{}\n{}\n\n", self.path.as_deref().unwrap_or(""), self.extra)
    }
}

pub(crate) fn traverse(
    rows: Vec<Row>,
    edge_type: &str,
    hops: i64,
    graph: &Graph<'_>,
) -> Result<Vec<Row>> {
    if hops < 1 {
        return Err(Error::Invalid("traverse --hops must be >= 1".into()));
    }
    let mut frontier: Vec<NodeView> = Vec::new();
    for row in &rows {
        if let Some(node) = row.node() {
            frontier.push(node.clone());
        } else if let Some(dest) = row.to_id().and_then(|id| graph.by_id.get(id)) {
            frontier.push(dest.clone());
        }
    }
    let mut emitted = Vec::new();
    let mut seen_edges: HashSet<String> = HashSet::new();
    for _ in 0..hops {
        let mut nxt = Vec::new();
        for src in &frontier {
            for edge in graph.edges_from(&src.id) {
                if edge.edge_type != edge_type {
                    continue;
                }
                if !seen_edges.insert(edge.id.clone()) {
                    continue;
                }
                let dest = edge.to_id.as_ref().and_then(|id| graph.by_id.get(id));
                emitted.push(Row::Walk {
                    from: src.clone(),
                    edge: edge.clone(),
                    to: dest.cloned(),
                });
                if let Some(dest) = dest {
                    nxt.push(dest.clone());
                }
            }
        }
        frontier = nxt;
    }
    Ok(emitted)
}

/// Keep Agent rows matching `name`: exact on extra.name / extra.title / path /
/// path basename first, else substring. Stays inside the current slice.
pub(crate) fn filter_agents(rows: Vec<Row>, name: &str) -> Vec<Row> {
    let agents: Vec<Row> = rows
        .into_iter()
        .filter(|row| row.node_type() == Some("Agent"))
        .collect();
    let exact: Vec<Row> = agents
        .iter()
        .filter(|row| row.node().is_some_and(|n| agent_exact(n, name)))
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    agents
        .into_iter()
        .filter(|row| row.node().is_some_and(|n| agent_substring(n, name)))
        .collect()
}

fn agent_exact(node: &NodeView, name: &str) -> bool {
    if node.extra_get("name") == Some(name) || node.extra_get("title") == Some(name) {
        return true;
    }
    let path = node.path.as_deref().unwrap_or("");
    path == name || path_basename(path) == name
}

fn agent_substring(node: &NodeView, name: &str) -> bool {
    let extra_name = node.extra_get("name").unwrap_or("");
    let extra_title = node.extra_get("title").unwrap_or("");
    let path = node.path.as_deref().unwrap_or("");
    extra_name.contains(name) || extra_title.contains(name) || path.contains(name)
}

fn path_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Which desired states a slice selects, with the node that selected each.
///
/// - Vault nodes in the slice: every named desired state of that vault
///   (or the one called `name`), subject = the vault node.
/// - Otherwise Agent nodes: the desired state called `name`, or the one whose
///   name matches the agent (exact on extra.name / extra.title / path
///   basename, then substring), subject = the agent node.
/// - Otherwise (`state <name>` over documents / walks): the desired state
///   called `name` in each vault the slice touches, subject = None; bare
///   `state` lists all of them.
fn select_states(
    rows: &[Row],
    name: Option<&str>,
    store: &RoStore,
) -> Result<Vec<(Option<NodeView>, DesiredStateView)>> {
    let all = store.desired_states()?;
    let in_vault = |vault_id: &str| -> Vec<&DesiredStateView> {
        all.iter().filter(|ds| ds.vault_id == vault_id).collect()
    };
    let wanted = |ds: &DesiredStateView| name.map_or(true, |n| ds.name == n);
    let mut selected: Vec<(Option<NodeView>, DesiredStateView)> = Vec::new();

    let vaults: Vec<&NodeView> = rows
        .iter()
        .filter_map(Row::node)
        .filter(|n| n.node_type == "Vault")
        .collect();
    if !vaults.is_empty() {
        for vault in vaults {
            for ds in in_vault(&vault.vault_id).into_iter().filter(|ds| wanted(ds)) {
                selected.push((Some(vault.clone()), ds.clone()));
            }
        }
        return Ok(selected);
    }

    let agents: Vec<&NodeView> = rows
        .iter()
        .filter_map(Row::node)
        .filter(|n| n.node_type == "Agent")
        .collect();
    if !agents.is_empty() {
        for agent in agents {
            let candidates = in_vault(&agent.vault_id);
            let picked: Vec<&DesiredStateView> = match name {
                Some(n) => candidates.into_iter().filter(|ds| ds.name == n).collect(),
                None => states_named_like(agent, &candidates),
            };
            for ds in picked {
                selected.push((Some(agent.clone()), ds.clone()));
            }
        }
        return Ok(selected);
    }

    let mut vault_ids: Vec<&str> = Vec::new();
    for row in rows {
        if let Some(id) = row.vault_id() {
            if !vault_ids.contains(&id) {
                vault_ids.push(id);
            }
        }
    }
    for vault_id in vault_ids {
        for ds in in_vault(vault_id).into_iter().filter(|ds| wanted(ds)) {
            selected.push((None, ds.clone()));
        }
    }
    Ok(selected)
}

/// Desired states whose name matches the agent: exact on extra.name /
/// extra.title / path basename, else substring — the `agent` rule mirrored.
fn states_named_like<'a>(
    agent: &NodeView,
    candidates: &[&'a DesiredStateView],
) -> Vec<&'a DesiredStateView> {
    let path = agent.path.as_deref().unwrap_or("");
    let keys: Vec<&str> = [
        agent.extra_get("name"),
        agent.extra_get("title"),
        Some(path_basename(path)),
    ]
    .into_iter()
    .flatten()
    .filter(|k| !k.is_empty())
    .collect();
    let exact: Vec<&DesiredStateView> = candidates
        .iter()
        .copied()
        .filter(|ds| keys.iter().any(|k| ds.name == *k))
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    candidates
        .iter()
        .copied()
        .filter(|ds| keys.iter().any(|k| ds.name.contains(k)))
        .collect()
}

/// Warm: one `State` row per selected desired state. Never reads events.
pub(crate) fn apply_state(rows: &[Row], name: Option<&str>, store: &RoStore) -> Result<Vec<Row>> {
    Ok(select_states(rows, name, store)?
        .into_iter()
        .map(|(subject, ds)| Row::State { subject, ds })
        .collect())
}

/// Cool: the causal events of each selected desired state
/// (`events.reconciles = ds.id`, `ts ASC, id ASC`). Never reads spec / status.
pub(crate) fn apply_history(
    rows: &[Row],
    name: Option<&str>,
    store: &RoStore,
) -> Result<Vec<Row>> {
    let mut emitted = Vec::new();
    for (_, ds) in select_states(rows, name, store)? {
        for event in store.causal_chain(&ds.id)? {
            emitted.push(Row::History { event });
        }
    }
    Ok(emitted)
}

pub(crate) fn apply_limit(rows: &mut Vec<Row>, n: i64) {
    if n >= 0 {
        rows.truncate((n as usize).min(rows.len()));
    } else {
        let drop = n.unsigned_abs() as usize;
        let keep = rows.len().saturating_sub(drop);
        rows.truncate(keep);
    }
}
