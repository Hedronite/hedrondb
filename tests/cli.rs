//! Product CLI surface: `hedron import` / `hedron hql` help and import alias.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use hedron_core::{DesiredState, Node, Store};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempTree {
    path: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hedron-cli-{}-{}-{}", label, std::process::id(), n));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn hedron() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hedron"))
}

#[test]
fn hedron_import_help() {
    let out = hedron()
        .args(["import", "--help"])
        .output()
        .expect("hedron import --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "help failed: {stderr}{stdout}");
    assert!(stdout.contains("hedron import"));
    assert!(stdout.contains("--src"));
    assert!(stdout.contains("thin alias"));
}

#[test]
fn hedron_hql_help() {
    let out = hedron()
        .args(["hql", "--help"])
        .output()
        .expect("hedron hql --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "help failed: {stderr}{stdout}");
    assert!(stdout.contains("hedron hql"));
    assert!(stdout.contains("--db"));
    assert!(stdout.contains("tsv|table|json"));
    assert!(stdout.contains("result-twin"));
}

#[test]
fn hedron_root_help() {
    let out = hedron().arg("--help").output().expect("hedron --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("import"));
    assert!(stdout.contains("hql"));
    assert!(stdout.contains("jev-intent"));
    assert!(stdout.contains("mcp"));
}

#[test]
fn hedron_import_subcommand_matches_alias() {
    let src = TempTree::new("src");
    fs::write(
        src.path.join("alpha.md"),
        "---\nname: Alpha\n---\nSee [[ghost]].\n",
    )
    .unwrap();

    let out_dir = TempTree::new("out");
    let db = out_dir.path.join("store.db");
    let result = hedron()
        .args([
            "import",
            "--src",
            src.path.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--vault",
            "fixture",
            "--agent",
            "importer",
        ])
        .output()
        .expect("hedron import");
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        result.status.success(),
        "hedron import failed: {stderr}{stdout}"
    );
    assert!(!stdout.contains("hdt_") && !stderr.contains("hdt_"));
    assert!(stdout.contains("documents: 1"));
    assert!(stdout.contains("mentions_dangling: 1"));
    assert!(stdout.contains("mode: 0600"));
}

#[test]
fn hedron_hql_quoted_history_pipeline() {
    let out_dir = TempTree::new("history-db");
    let db = out_dir.path.join("history.db");
    let mut store = Store::open(&db).unwrap();
    let boot = store.bootstrap("prod", "deploy", "agents/deploy").unwrap();
    let spec = DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap();
    let ds = store
        .put_desired_state(&boot.token, "deploy", spec, 0.5)
        .unwrap();
    let doc = Node::brief_document(boot.vault.id, "alpha", "2026-08-25").unwrap();
    store.put_node(&boot.token, doc).unwrap();
    let (_, event) = store.reconcile(&boot.token, ds.id).unwrap();
    drop(store);

    let result = hedron()
        .args([
            "hql",
            "--db",
            db.to_str().unwrap(),
            "--format",
            "json",
            "vault prod | agent deploy | history",
        ])
        .output()
        .expect("hedron hql history");
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        result.status.success(),
        "hedron hql history failed: {stderr}{stdout}"
    );
    assert!(stdout.contains(&event.id.to_string()));
    assert!(stdout.contains(&format!("{}@1", ds.id)));
    assert!(stdout.contains("Reconciled"));
    assert!(!stdout.contains("hdt_") && !stderr.contains("hdt_"));
    assert!(!stdout.contains("spec"));
    assert!(!stdout.contains("status"));
}
