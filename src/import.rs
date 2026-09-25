//! Markdown tree import. Tokens stay in-process and are never printed.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};
use uuid::Uuid;

use crate::{DesiredState, Edge, Node, Store};

const DEFAULT_EXCLUDE: &str = "inbox/private/";
const BRIEFS_PREFIX: &str = "inbox";
const ROLLBACK_TEST_ENV: &str = "HEDRON_IMPORT_TEST_ROLLBACK";

const DROPPED_KEYS: &[&str] = &[
    "token",
    "api_key",
    "secret",
    "password",
    "authorization",
    "doc_id",
    "content_hash",
];

pub const IMPORT_HELP: &str = "\
Import a markdown tree into a HedronDB store.

Usage:
  hedron import --src DIR --db FILE --vault NAME --agent NAME [OPTIONS]
  hedron-import --src DIR --db FILE --vault NAME --agent NAME [OPTIONS]

hedron-import is a thin alias of `hedron import`.

Options:
  --src DIR              Markdown tree to walk
  --db FILE              Destination store (created; refused if it exists)
  --vault NAME           Vault to bootstrap
  --agent NAME           Agent to bootstrap
  --htec-path PATH       Optional H.TEC path prefix for imported documents
  --exclude-prefix PRE   Skip this relative prefix (default: inbox/private/)
  --briefs-date DATE     Optional Option B briefs recon after import
  --force                Replace an existing store file
  -h, --help             Print help

Tokens stay in-process and are never printed. Frontmatter is the first
`---` YAML fence; `<!-- hal:authoritative:yaml -->` is ignored. Do not
infer extra.domain or extra.name from paths.
";

pub struct ImportArgs {
    pub src: PathBuf,
    pub db: PathBuf,
    pub vault: String,
    pub agent: String,
    pub htec_path: Option<String>,
    pub exclude_prefix: String,
    pub force: bool,
    pub briefs_date: Option<String>,
}

/// Parse flags and run import, or print help. Same flags as `hedron-import`.
pub fn run_cli(raw: Vec<String>) -> Result<(), String> {
    if raw.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{IMPORT_HELP}");
        return Ok(());
    }
    let args = parse_args(raw)?;
    run(args)
}

pub fn run(args: ImportArgs) -> Result<(), String> {
    if args.db.exists() {
        if !args.force {
            return Err(format!(
                "refuse existing store {} (pass --force to replace)",
                args.db.display()
            ));
        }
        fs::remove_file(&args.db).map_err(|err| err.to_string())?;
    }

    let htec = args
        .htec_path
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("agents/{}", args.agent));

    let mut store = Store::open(&args.db).map_err(err_str)?;
    store.begin().map_err(err_str)?;

    match write_import(&mut store, &args, &htec) {
        Ok((docs_len, resolved, dangling)) => {
            store.commit().map_err(err_str)?;
            let mode = store_mode(&args.db)?;
            let mut out = io::stdout();
            writeln!(out, "documents: {docs_len}").map_err(|err| err.to_string())?;
            writeln!(out, "mentions_resolved: {resolved}").map_err(|err| err.to_string())?;
            writeln!(out, "mentions_dangling: {dangling}").map_err(|err| err.to_string())?;
            writeln!(out, "store: {}", store.path().display()).map_err(|err| err.to_string())?;
            writeln!(out, "mode: {mode:04o}").map_err(|err| err.to_string())?;
            Ok(())
        }
        Err(err) => {
            let _ = store.rollback();
            Err(err)
        }
    }
}

fn write_import(
    store: &mut Store,
    args: &ImportArgs,
    htec: &str,
) -> Result<(usize, usize, usize), String> {
    let boot = store
        .bootstrap(&args.vault, &args.agent, htec)
        .map_err(err_str)?;
    let token = store.rotate_token(&boot.token).map_err(err_str)?;
    let vault_id = boot.vault.id;
    // boot.token is dead after rotate; never print either token.

    let prefix_docs = args.htec_path.as_deref().filter(|s| !s.is_empty());
    let files = collect_markdown(&args.src, &args.exclude_prefix)?;
    let mut docs = Vec::new();
    for path in files {
        let rel = relative_posix(&args.src, &path)?;
        let text = fs::read_to_string(&path).map_err(|err| err.to_string())?;
        let (front, body) = split_frontmatter(&text);
        let extra = extra_from_frontmatter(&front);
        let store_path = match prefix_docs {
            Some(prefix) => format!("{}/{}", prefix.trim_end_matches('/'), rel),
            None => rel.clone(),
        };
        let node = Node::document(vault_id, Some(&store_path), extra).map_err(err_str)?;
        let stem = stem_of(&rel);
        docs.push(Imported {
            rel,
            store_path,
            stem,
            extra: node.extra.clone(),
            body: body.to_string(),
            node_id: node.id,
            node,
        });
    }

    for doc in &docs {
        store.put_node(&token, doc.node.clone()).map_err(err_str)?;
    }

    let index = ResolveIndex::build(&docs);
    let mut resolved = 0usize;
    let mut dangling = 0usize;
    for doc in &docs {
        for target in extract_wikilinks(&doc.body) {
            let to_id = index.resolve(&target);
            if to_id.is_some() {
                resolved += 1;
            } else {
                dangling += 1;
            }
            let edge = Edge::new(vault_id, doc.node_id, to_id, Some(target), "mentions")
                .map_err(err_str)?;
            store.put_edge(&token, edge).map_err(err_str)?;
        }
    }

    if let Some(date) = &args.briefs_date {
        let inbox: Vec<&Imported> = docs
            .iter()
            .filter(|doc| {
                doc.rel == BRIEFS_PREFIX || doc.rel.starts_with(&format!("{BRIEFS_PREFIX}/"))
            })
            .collect();
        let source: Vec<&Imported> = if inbox.is_empty() {
            docs.iter().collect()
        } else {
            inbox
        };
        let briefs: Vec<&str> = source.iter().map(|doc| doc.stem.as_str()).collect();
        let spec = DesiredState::docs_eod_spec(date, &briefs).map_err(err_str)?;
        let ds = store
            .put_desired_state(&token, &format!("eod-{date}"), spec, 0.5)
            .map_err(err_str)?;
        store.reconcile(&token, ds.id).map_err(err_str)?;
    }

    if std::env::var(ROLLBACK_TEST_ENV).is_ok() {
        return Err("import rollback test hook".into());
    }

    let _ = token;
    Ok((docs.len(), resolved, dangling))
}

struct Imported {
    rel: String,
    store_path: String,
    stem: String,
    extra: Value,
    body: String,
    node_id: Uuid,
    node: Node,
}

struct ResolveIndex {
    exact: HashMap<String, Uuid>,
    unique: HashMap<String, Uuid>,
}

impl ResolveIndex {
    fn build(docs: &[Imported]) -> Self {
        let mut exact = HashMap::new();
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut unique = HashMap::new();

        for doc in docs {
            exact.insert(doc.store_path.clone(), doc.node_id);
            exact.insert(strip_md(&doc.store_path), doc.node_id);
            exact.insert(doc.rel.clone(), doc.node_id);
            exact.insert(strip_md(&doc.rel), doc.node_id);

            let mut keys = vec![doc.stem.clone()];
            if let Some(name) = extra_str(&doc.extra, "name") {
                keys.push(name);
            }
            if let Some(title) = extra_str(&doc.extra, "title") {
                keys.push(title);
            }
            for key in keys {
                *counts.entry(key.clone()).or_insert(0) += 1;
                unique.insert(key, doc.node_id);
            }
        }
        unique.retain(|key, _| counts.get(key).copied().unwrap_or(0) == 1);
        Self { exact, unique }
    }

    fn resolve(&self, target: &str) -> Option<Uuid> {
        let trimmed = target.trim().trim_start_matches('/').to_string();
        if let Some(id) = self.exact.get(&trimmed) {
            return Some(*id);
        }
        let no_md = strip_md(&trimmed);
        if let Some(id) = self.exact.get(&no_md) {
            return Some(*id);
        }
        if let Some(id) = self.unique.get(&trimmed) {
            return Some(*id);
        }
        self.unique.get(&no_md).copied()
    }
}

fn collect_markdown(src: &Path, exclude_prefix: &str) -> Result<Vec<PathBuf>, String> {
    if !src.is_dir() {
        return Err(format!("--src is not a directory: {}", src.display()));
    }
    let mut out = Vec::new();
    walk_md(src, src, exclude_prefix, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_md(
    src: &Path,
    dir: &Path,
    exclude_prefix: &str,
    out: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        let rel = relative_posix(src, &path)?;
        if excluded(&rel, exclude_prefix) {
            continue;
        }
        if path.is_dir() {
            walk_md(src, &path, exclude_prefix, out)?;
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

fn excluded(rel: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    let prefix = prefix.trim_end_matches('/');
    rel == prefix || rel.starts_with(&format!("{prefix}/"))
}

fn relative_posix(src: &Path, path: &Path) -> Result<String, String> {
    let rel = path
        .strip_prefix(src)
        .map_err(|_| format!("{} is not under {}", path.display(), src.display()))?;
    Ok(rel
        .iter()
        .map(|c| c.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn stem_of(rel: &str) -> String {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    strip_md(name)
}

fn strip_md(path: &str) -> String {
    path.strip_suffix(".md")
        .or_else(|| path.strip_suffix(".MD"))
        .unwrap_or(path)
        .to_string()
}

/// First `---` ... `---` YAML fence in the file. A BOM or comment may sit above
/// it. The deprecated `<!-- hal:authoritative:yaml -->` wrapper is ignored.
fn split_frontmatter(text: &str) -> (Mapping, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = after_open_fence(text) else {
        return (Mapping::new(), text);
    };
    let close = rest
        .find("\n---\n")
        .map(|i| (i, 5))
        .or_else(|| rest.find("\n---\r\n").map(|i| (i, 6)))
        .or_else(|| rest.find("\r\n---\r\n").map(|i| (i, 8)))
        .or_else(|| {
            if rest == "---" || rest.starts_with("---\n") || rest.starts_with("---\r\n") {
                Some((0, 3))
            } else {
                None
            }
        });
    let Some((idx, skip)) = close else {
        return (Mapping::new(), text);
    };
    let yaml = &rest[..idx];
    let body = &rest[idx + skip..];
    match serde_yaml::from_str::<Value>(yaml) {
        Ok(Value::Mapping(map)) => (map, body),
        _ => (Mapping::new(), body),
    }
}

/// Text after the first line that is exactly `---`.
fn after_open_fence(text: &str) -> Option<&str> {
    let mut pos = 0;
    while pos <= text.len() {
        let rest = &text[pos..];
        let (line, next) = match rest.find('\n') {
            Some(i) => {
                let line = rest[..i].strip_suffix('\r').unwrap_or(&rest[..i]);
                (line, pos + i + 1)
            }
            None => (rest.strip_suffix('\r').unwrap_or(rest), text.len()),
        };
        if line == "---" {
            return Some(&text[next..]);
        }
        if next == text.len() {
            break;
        }
        pos = next;
    }
    None
}

fn extra_from_frontmatter(front: &Mapping) -> Value {
    let mut out = Mapping::new();
    for (key, value) in front {
        let Some(name) = key.as_str() else {
            continue;
        };
        if DROPPED_KEYS
            .iter()
            .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
        {
            continue;
        }
        out.insert(key.clone(), value.clone());
    }
    Value::Mapping(out)
}

fn extra_str(extra: &Value, key: &str) -> Option<String> {
    extra
        .as_mapping()
        .and_then(|map| map.get(Value::String(key.into())))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn extract_wikilinks(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if let Some(end) = body[i + 2..].find("]]") {
                let inner = &body[i + 2..i + 2 + end];
                let target = inner
                    .split(|c| c == '|' || c == '#')
                    .next()
                    .unwrap_or(inner)
                    .trim();
                if !target.is_empty() {
                    out.push(target.to_string());
                }
                i = i + 2 + end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn store_mode(path: &Path) -> Result<u32, String> {
    let meta = fs::metadata(path).map_err(|err| err.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return Ok(meta.permissions().mode() & 0o777);
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Ok(0)
    }
}

fn parse_args(raw: Vec<String>) -> Result<ImportArgs, String> {
    let mut src = None;
    let mut db = None;
    let mut vault = None;
    let mut agent = None;
    let mut htec_path = None;
    let mut exclude_prefix = DEFAULT_EXCLUDE.to_string();
    let mut force = false;
    let mut briefs_date = None;
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--src" => {
                src = Some(PathBuf::from(need_value(&raw, i, "--src")?));
                i += 2;
            }
            "--db" => {
                db = Some(PathBuf::from(need_value(&raw, i, "--db")?));
                i += 2;
            }
            "--vault" => {
                vault = Some(need_value(&raw, i, "--vault")?.to_string());
                i += 2;
            }
            "--agent" => {
                agent = Some(need_value(&raw, i, "--agent")?.to_string());
                i += 2;
            }
            "--htec-path" => {
                htec_path = Some(need_value(&raw, i, "--htec-path")?.to_string());
                i += 2;
            }
            "--exclude-prefix" => {
                exclude_prefix = need_value(&raw, i, "--exclude-prefix")?.to_string();
                i += 2;
            }
            "--briefs-date" => {
                briefs_date = Some(need_value(&raw, i, "--briefs-date")?.to_string());
                i += 2;
            }
            "--force" => {
                force = true;
                i += 1;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(ImportArgs {
        src: src.ok_or("--src DIR is required")?,
        db: db.ok_or("--db FILE is required")?,
        vault: vault.ok_or("--vault NAME is required")?,
        agent: agent.ok_or("--agent NAME is required")?,
        htec_path,
        exclude_prefix,
        force,
        briefs_date,
    })
}

fn need_value<'a>(raw: &'a [String], i: usize, flag: &str) -> Result<&'a str, String> {
    raw.get(i + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn err_str(err: crate::Error) -> String {
    err.to_string()
}
