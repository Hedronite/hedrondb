//! Named-ask Jev gate: CLI, MCP, and shadow (no store writes).

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use hedron_core::hql::RoStore;
use hedron_core::jev::evidence_digest_of;
use hedron_core::{DesiredState, Node, Store};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempTree {
    path: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hedron-jev-{}-{}-{}", label, std::process::id(), n));
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

fn write_fixture(dir: &TempTree, body: &str) -> PathBuf {
    let path = dir.path.join("systemone.json");
    fs::write(&path, body).unwrap();
    path
}

#[test]
fn jev_intent_help() {
    let out = hedron()
        .args(["jev-intent", "--help"])
        .output()
        .expect("hedron jev-intent --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "help failed: {stderr}{stdout}");
    assert!(stdout.contains("hedron jev-intent"));
    assert!(stdout.contains("--intent"));
    assert!(stdout.contains("--evidence-digest"));
    assert!(stdout.contains("never writes"));
    assert!(!stdout.to_ascii_lowercase().contains("duha"));
    assert!(!stdout.to_ascii_lowercase().contains("weekday"));
}

#[test]
fn mcp_help() {
    let out = hedron()
        .args(["mcp", "--help"])
        .output()
        .expect("hedron mcp --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("jev_intent"));
    assert!(stdout.contains("never writes"));
}

#[test]
fn none_transport_escalates_and_does_not_apply() {
    let out = hedron()
        .env("HEDRON_JEV_TRANSPORT", "none")
        .args([
            "jev-intent",
            "--intent",
            "replicas=2",
            "--evidence-digest",
            "abc123",
        ])
        .output()
        .expect("hedron jev-intent none");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "jev-intent failed: {stderr}{stdout}");
    assert!(stdout.contains("\"choice\": \"escalate\""));
    assert!(stdout.contains("\"applied\": false"));
    assert!(stdout.contains("\"shadow\": true"));
    assert!(stdout.contains("\"status\": \"unavailable\""));
    assert!(!stdout.contains("sk-") && !stderr.contains("sk-"));
}

#[test]
fn fixture_apply_stays_shadow() {
    let dir = TempTree::new("fixture-apply");
    let fixture = write_fixture(
        &dir,
        r#"{
          "answers": {
            "apply": { "type": "choice", "choice": "apply", "confidence": 0.94 },
            "sufficient": { "type": "noul", "noul": 1.0, "confidence": 0.94 }
          }
        }"#,
    );
    let out = hedron()
        .env("HEDRON_JEV_TRANSPORT", "fixture")
        .env("HEDRON_JEV_FIXTURE", &fixture)
        .args([
            "jev-intent",
            "--intent",
            "replicas=2",
            "--evidence-digest",
            "abc123",
        ])
        .output()
        .expect("hedron jev-intent fixture");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "jev-intent failed: {stderr}{stdout}");
    assert!(stdout.contains("\"choice\": \"apply\""));
    assert!(stdout.contains("\"applied\": false"));
    assert!(stdout.contains("\"status\": \"judged\""));
}

#[test]
fn fixture_low_confidence_escalates() {
    let dir = TempTree::new("fixture-low");
    let fixture = write_fixture(
        &dir,
        r#"{
          "answers": {
            "apply": { "type": "choice", "choice": "apply", "confidence": 0.2 },
            "sufficient": { "type": "noul", "noul": 1.0, "confidence": 0.2 }
          }
        }"#,
    );
    let out = hedron()
        .env("HEDRON_JEV_TRANSPORT", "fixture")
        .env("HEDRON_JEV_FIXTURE", &fixture)
        .args([
            "jev-intent",
            "--intent",
            "replicas=2",
            "--evidence-digest",
            "abc123",
        ])
        .output()
        .expect("hedron jev-intent low");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("\"choice\": \"escalate\""));
    assert!(stdout.contains("\"status\": \"low_confidence\""));
    assert!(stdout.contains("\"applied\": false"));
}

#[test]
fn named_intent_is_read_only_even_when_choice_is_apply() {
    let dir = TempTree::new("named-ro");
    let db = dir.path.join("intent.db");
    let mut store = Store::open(&db).unwrap();
    let boot = store.bootstrap("prod", "deploy", "agents/deploy").unwrap();
    let spec = DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap();
    let ds = store
        .put_desired_state(&boot.token, "deploy", spec, 0.5)
        .unwrap();
    let doc = Node::brief_document(boot.vault.id, "alpha", "2026-08-25").unwrap();
    store.put_node(&boot.token, doc).unwrap();
    let (before, _) = store.reconcile(&boot.token, ds.id).unwrap();
    let events_before = store.causal_chain(&boot.token, ds.id).unwrap().len();
    let version_before = before.state_version;
    drop(store);

    let fixture = write_fixture(
        &dir,
        r#"{
          "answers": {
            "apply": { "type": "choice", "choice": "apply", "confidence": 0.99 },
            "sufficient": { "type": "noul", "noul": 1.0, "confidence": 0.99 }
          }
        }"#,
    );
    let out = hedron()
        .env("HEDRON_JEV_TRANSPORT", "fixture")
        .env("HEDRON_JEV_FIXTURE", &fixture)
        .args([
            "jev-intent",
            "--db",
            db.to_str().unwrap(),
            "--vault",
            "prod",
            "--name",
            "deploy",
            "--evidence-digest",
            "deadbeef",
        ])
        .output()
        .expect("hedron jev-intent named");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "named ask failed: {stderr}{stdout}");
    assert!(stdout.contains("\"choice\": \"apply\""));
    assert!(stdout.contains("\"applied\": false"));
    assert!(stdout.contains("name: deploy"));
    assert!(stdout.contains("docs_eod"));

    let ro = RoStore::open(&db).unwrap();
    let after = ro
        .desired_states()
        .unwrap()
        .into_iter()
        .find(|s| s.name == "deploy")
        .expect("deploy still present");
    let events_after = ro.causal_chain(&ds.id.to_string()).unwrap().len();
    assert_eq!(after.state_version, version_before as i64);
    assert_eq!(events_after, events_before, "Jev must not append events");
}

#[test]
fn evidence_file_is_hashed_not_inlined() {
    let dir = TempTree::new("evidence-file");
    let ev = dir.path.join("lattice-body.json");
    fs::write(&ev, b"cluster observed replicas=2").unwrap();
    let digest = evidence_digest_of(b"cluster observed replicas=2");
    let out = hedron()
        .env("HEDRON_JEV_TRANSPORT", "none")
        .args([
            "jev-intent",
            "--intent",
            "replicas=2",
            "--evidence",
            ev.to_str().unwrap(),
        ])
        .output()
        .expect("hedron jev-intent evidence file");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains(&digest));
    assert!(!stdout.contains("cluster observed"));
}

#[test]
fn mcp_tools_call_is_the_cli_document() {
    let mut child = hedron()
        .env("HEDRON_JEV_TRANSPORT", "none")
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hedron mcp");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"jev_intent","arguments":{{"intent":"replicas=2","evidence_digest":"abc123"}}}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("mcp exit");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "mcp failed: {stdout}");
    assert!(
        stdout.contains("\"choice\":\"escalate\"") || stdout.contains("\"choice\": \"escalate\"")
    );
    assert!(stdout.contains("\"applied\":false") || stdout.contains("\"applied\": false"));
    assert!(stdout.contains("structuredContent"));
}
