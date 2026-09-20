//! Named-ask Jev gate: intent vs evidence. Facet TypeSafe / System One is
//! the transport; this module classifies. It never writes store rows.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::hql::RoStore;

/// Bundled Facet OpenCollection (intent vs evidence recipe).
pub const FACET_COLLECTION: &str = include_str!("../docs/examples/typesafe/opencollection.yml");
pub const FACET_SELECTOR: &str = "items/0/items/0";
pub const FACET_ENVIRONMENT: &str = "typesafe";

const CONFIDENCE_FLOOR: f64 = 0.6;
const INTENT_MAX: usize = 4000;
const DIGEST_MAX: usize = 128;

pub const JEV_INTENT_HELP: &str = "\
Named-ask Jev gate: intent vs evidence.

Usage:
  hedron jev-intent --intent TEXT --evidence-digest DIGEST
  hedron jev-intent --db FILE --name NAME [--vault NAME] --evidence-digest DIGEST

Returns Choice {apply, wait, escalate, ignore} plus confidence.
Shadow: never writes desired_states, events, or nodes. Low confidence
escalates; apply is never executed.

Options:
  --intent TEXT          Intent text (named ask; mutually exclusive with --name)
  --name NAME            Load a named desired-state spec read-only from --db
  --db FILE              HedronDB sqlite file (read-only when --name is set)
  --vault NAME           Vault that holds --name (required if the name is not unique)
  --evidence-digest HEX  Evidence digest (blake3 / tip SHA). Not a raw body.
  --evidence FILE        Hash FILE with blake3; may replace or check --evidence-digest
  --format json|table    Output format (default json)
  -h, --help             Print help

Prefers `facet request run` against docs/examples/typesafe (selector
items/0/items/0). Key stays in Facet env. Named asks only — no grind clocks.
";

/// Shipped System One questions (Choice + Noul).
pub fn questions() -> Value {
    json!({
        "apply": {
            "type": "choice",
            "instructions": "Given this named intent and evidence digest, what should HedronDB do? Shadow only — do not write rows.",
            "criteria": {
                "apply": "Evidence matches the intent; a human may apply later. Not a write grant.",
                "wait": "Evidence is incomplete or stale; do not apply yet",
                "escalate": "Ambiguous, conflicting, or high-stakes — need a human",
                "ignore": "Noise, duplicate, or unrelated to this named intent"
            }
        },
        "sufficient": {
            "type": "noul",
            "instructions": "Is the evidence digest sufficient to consider applying this intent?",
            "criteria": {
                "true": "The digest identifies concrete, relevant evidence for the intent",
                "false": "The digest is empty, generic, or does not speak to the intent"
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Choice {
    Apply,
    Wait,
    Escalate,
    Ignore,
}

impl Choice {
    pub fn as_str(self) -> &'static str {
        match self {
            Choice::Apply => "apply",
            Choice::Wait => "wait",
            Choice::Escalate => "escalate",
            Choice::Ignore => "ignore",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "apply" => Some(Choice::Apply),
            "wait" => Some(Choice::Wait),
            "escalate" => Some(Choice::Escalate),
            "ignore" => Some(Choice::Ignore),
            _ => None,
        }
    }
}

/// Shadow report for one named ask. Never persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub shadow: bool,
    /// Shadow: a Jev Choice is never a write.
    pub applied: bool,
    pub choice: Choice,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sufficient: Option<f64>,
    pub status: String,
    pub transport: String,
    pub intent: String,
    pub evidence_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Decision {
    fn escalate(
        transport: &str,
        status: &str,
        intent: &str,
        evidence_digest: &str,
        confidence: Option<f64>,
        sufficient: Option<f64>,
        reason: Option<String>,
    ) -> Self {
        Self {
            shadow: true,
            applied: false,
            choice: Choice::Escalate,
            confidence,
            sufficient,
            status: status.to_string(),
            transport: transport.to_string(),
            intent: intent.to_string(),
            evidence_digest: evidence_digest.to_string(),
            reason,
        }
    }
}

/// How to reach System One. Never holds a store write or a key value.
pub enum Transport {
    None {
        reason: &'static str,
    },
    Facet {
        bin: PathBuf,
    },
    Fixture {
        path: PathBuf,
    },
    #[cfg(test)]
    Fake(FakeScript),
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Transport::None { reason } => f.debug_struct("None").field("reason", reason).finish(),
            Transport::Facet { bin } => f.debug_struct("Facet").field("bin", bin).finish(),
            Transport::Fixture { path } => f.debug_struct("Fixture").field("path", path).finish(),
            #[cfg(test)]
            Transport::Fake(_) => write!(f, "Fake"),
        }
    }
}

/// Canned System One body. Offline tests only.
#[cfg(test)]
#[derive(Clone)]
pub struct FakeScript {
    reply: Value,
}

#[cfg(test)]
impl FakeScript {
    pub fn reply(reply: Value) -> Self {
        Self { reply }
    }
}

impl Transport {
    /// Facet binary if present, else none. Never reads `$TYPESAFE_API_KEY`.
    pub fn resolve() -> Self {
        match std::env::var("HEDRON_JEV_TRANSPORT") {
            Ok(v) if v.eq_ignore_ascii_case("none") => {
                return Transport::None {
                    reason: "HEDRON_JEV_TRANSPORT=none",
                };
            }
            Ok(v) if v.eq_ignore_ascii_case("facet") => {
                return match find_on_path("facet") {
                    Some(bin) => Transport::Facet { bin },
                    None => Transport::None {
                        reason: "facet_not_on_path",
                    },
                };
            }
            Ok(v) if v.eq_ignore_ascii_case("fixture") => {
                return match std::env::var("HEDRON_JEV_FIXTURE") {
                    Ok(path) if !path.trim().is_empty() => Transport::Fixture {
                        path: PathBuf::from(path),
                    },
                    _ => Transport::None {
                        reason: "fixture_path_absent",
                    },
                };
            }
            _ => {}
        }
        if let Some(bin) = find_on_path("facet") {
            return Transport::Facet { bin };
        }
        Transport::None {
            reason: "facet_not_on_path",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Transport::None { .. } => "none",
            Transport::Facet { .. } => "facet",
            Transport::Fixture { .. } => "fixture",
            #[cfg(test)]
            Transport::Fake(_) => "fake",
        }
    }

    fn decide(&self, state: &str) -> std::result::Result<Value, String> {
        match self {
            Transport::None { reason } => Err((*reason).into()),
            Transport::Facet { bin } => facet_decide(bin, state),
            Transport::Fixture { path } => fixture_decide(path),
            #[cfg(test)]
            Transport::Fake(script) => Ok(script.reply.clone()),
        }
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Bound intent↔digest state. Digest stays attached; intent is clipped.
pub fn ask_state(intent: &str, evidence_digest: &str) -> String {
    format!(
        "Named ask: should HedronDB apply this intent given the evidence?\n\n\
         Intent:\n{}\n\nEvidence digest:\n{}\n\n\
         Shadow: classify only. Do not write desired_states, events, or nodes.",
        clip(intent, INTENT_MAX),
        clip(evidence_digest, DIGEST_MAX)
    )
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn fixture_decide(path: &Path) -> std::result::Result<Value, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("fixture json: {e}"))
}

fn facet_decide(bin: &Path, state: &str) -> std::result::Result<Value, String> {
    let dir = tempfile_dir()?;
    let yaml = dir.join("opencollection.yml");
    std::fs::write(&yaml, FACET_COLLECTION).map_err(|e| e.to_string())?;
    let out = Command::new(bin)
        .arg("--json")
        .arg("request")
        .arg("run")
        .arg(&yaml)
        .arg(FACET_SELECTOR)
        .arg("--environment")
        .arg(FACET_ENVIRONMENT)
        .arg("--no-record")
        .arg("--var")
        .arg(format!("state={state}"))
        .output()
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&dir);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(format!(
            "facet exit {}: {}",
            out.status,
            clip(&format!("{err}{stdout}"), 300)
        ));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("facet json: {e}"))?;
    parse_facet_answers(&v)
}

fn tempfile_dir() -> std::result::Result<PathBuf, String> {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("hedron-jev-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn parse_facet_answers(v: &Value) -> std::result::Result<Value, String> {
    let content = v
        .pointer("/response/body/content")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            if v.get("error").is_some() {
                format!("facet error: {}", v["error"])
            } else {
                "facet response missing body".into()
            }
        })?;
    serde_json::from_str(content).map_err(|e| format!("facet body json: {e}"))
}

/// Classify one named ask. Missing transport / low confidence → escalate.
/// Never opens a writable store.
pub fn ask(intent: &str, evidence_digest: &str, transport: Transport) -> Result<Decision> {
    reject_secret_text(intent, "intent")?;
    reject_secret_text(evidence_digest, "evidence_digest")?;
    if intent.trim().is_empty() {
        return Err(Error::Invalid("intent is required".into()));
    }
    if evidence_digest.trim().is_empty() {
        return Err(Error::Invalid("evidence_digest is required".into()));
    }

    if let Transport::None { reason } = &transport {
        return Ok(Decision::escalate(
            "none",
            "unavailable",
            intent,
            evidence_digest,
            None,
            None,
            Some((*reason).into()),
        ));
    }

    let state = ask_state(intent, evidence_digest);
    match transport.decide(&state) {
        Ok(body) => Ok(decision_from_answers(
            transport.name(),
            intent,
            evidence_digest,
            &body,
        )),
        Err(reason) => Ok(Decision::escalate(
            transport.name(),
            "unavailable",
            intent,
            evidence_digest,
            None,
            None,
            Some(reason),
        )),
    }
}

/// Parse a System One `answers` object. Empty / low confidence → escalate.
pub fn decision_from_answers(
    transport: &str,
    intent: &str,
    evidence_digest: &str,
    body: &Value,
) -> Decision {
    let answers = body.get("answers").unwrap_or(body);
    let choice_raw = answers.pointer("/apply/choice").and_then(Value::as_str);
    let choice_conf = answers.pointer("/apply/confidence").and_then(Value::as_f64);
    let sufficient = answers.pointer("/sufficient/noul").and_then(Value::as_f64);
    let sufficient_conf = answers
        .pointer("/sufficient/confidence")
        .and_then(Value::as_f64);
    let confidence = [choice_conf, sufficient_conf]
        .into_iter()
        .flatten()
        .reduce(f64::min);
    let parsed = choice_raw.and_then(Choice::parse);
    let empty = parsed.is_none();
    let low = confidence.is_some_and(|c| c < CONFIDENCE_FLOOR);

    if empty || low {
        let status = if empty { "uncertain" } else { "low_confidence" };
        let reason = if empty {
            Some("empty_or_unknown_choice".into())
        } else {
            Some(format!("confidence below {CONFIDENCE_FLOOR}"))
        };
        return Decision::escalate(
            transport,
            status,
            intent,
            evidence_digest,
            confidence,
            sufficient,
            reason,
        );
    }

    Decision {
        shadow: true,
        applied: false,
        choice: parsed.expect("parsed after empty check"),
        confidence,
        sufficient,
        status: "judged".into(),
        transport: transport.to_string(),
        intent: intent.to_string(),
        evidence_digest: evidence_digest.to_string(),
        reason: None,
    }
}

fn reject_secret_text(text: &str, field: &str) -> Result<()> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("sk-")
        || lower.contains("bearer ")
        || lower.contains("typesafe_api_key")
        || lower.contains("api_key=")
    {
        return Err(Error::Invalid(format!(
            "{field} must not carry a TypeSafe key or Authorization material"
        )));
    }
    Ok(())
}

/// Load a named desired-state spec read-only. Never opens a writable store.
pub fn load_named_intent(db: &Path, name: &str, vault: Option<&str>) -> Result<String> {
    if name.trim().is_empty() {
        return Err(Error::Invalid("desired state name is required".into()));
    }
    let store = RoStore::open(db)?;
    let mut states = store.desired_states()?;
    if let Some(vault) = vault {
        let ids = store.vault_ids_named(vault)?;
        if ids.is_empty() {
            return Err(Error::NotFound("vault"));
        }
        states.retain(|s| ids.iter().any(|id| id == &s.vault_id));
    }
    let matches: Vec<_> = states.into_iter().filter(|s| s.name == name).collect();
    match matches.len() {
        0 => Err(Error::NotFound("desired state")),
        1 => Ok(format!(
            "name: {}\nspec:\n{}",
            matches[0].name, matches[0].spec
        )),
        _ => Err(Error::Invalid(format!(
            "desired state {name:?} is not unique; pass --vault"
        ))),
    }
}

pub fn evidence_digest_of(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskArgs {
    pub intent: Option<String>,
    pub name: Option<String>,
    pub db: Option<PathBuf>,
    pub vault: Option<String>,
    pub evidence_digest: Option<String>,
    pub evidence: Option<PathBuf>,
    pub format: AskFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskFormat {
    Json,
    Table,
}

impl AskFormat {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "json" => Ok(Self::Json),
            "table" => Ok(Self::Table),
            other => Err(Error::Invalid(format!(
                "unknown format {other:?} (expected json or table)"
            ))),
        }
    }
}

pub fn parse_ask_args(raw: Vec<String>) -> Result<AskArgs> {
    let mut intent = None;
    let mut name = None;
    let mut db = None;
    let mut vault = None;
    let mut evidence_digest = None;
    let mut evidence = None;
    let mut format = AskFormat::Json;
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--intent" => {
                intent = Some(need_value(&raw, i, "--intent")?.to_string());
                i += 2;
            }
            "--name" => {
                name = Some(need_value(&raw, i, "--name")?.to_string());
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
            "--evidence-digest" => {
                evidence_digest = Some(need_value(&raw, i, "--evidence-digest")?.to_string());
                i += 2;
            }
            "--evidence" => {
                evidence = Some(PathBuf::from(need_value(&raw, i, "--evidence")?));
                i += 2;
            }
            "--format" => {
                format = AskFormat::parse(need_value(&raw, i, "--format")?)?;
                i += 2;
            }
            other => {
                return Err(Error::Invalid(format!("unknown argument {other:?}")));
            }
        }
    }
    Ok(AskArgs {
        intent,
        name,
        db,
        vault,
        evidence_digest,
        evidence,
        format,
    })
}

fn need_value<'a>(raw: &'a [String], i: usize, flag: &str) -> Result<&'a str> {
    raw.get(i + 1)
        .map(String::as_str)
        .ok_or_else(|| Error::Invalid(format!("{flag} needs a value")))
}

pub fn resolve_ask(args: &AskArgs) -> Result<(String, String)> {
    let intent = match (&args.intent, &args.name) {
        (Some(intent), None) => intent.clone(),
        (None, Some(name)) => {
            let db = args
                .db
                .as_ref()
                .ok_or_else(|| Error::Invalid("--name requires --db".into()))?;
            load_named_intent(db, name, args.vault.as_deref())?
        }
        (Some(_), Some(_)) => {
            return Err(Error::Invalid("pass --intent or --name, not both".into()));
        }
        (None, None) => {
            return Err(Error::Invalid(
                "pass --intent TEXT or --name NAME --db FILE".into(),
            ));
        }
    };

    let hashed = args
        .evidence
        .as_ref()
        .map(|path| {
            let bytes = std::fs::read(path)?;
            Ok::<String, Error>(evidence_digest_of(&bytes))
        })
        .transpose()?;

    let digest = match (&args.evidence_digest, hashed) {
        (Some(given), Some(hashed)) if given != &hashed => {
            return Err(Error::Invalid(
                "--evidence-digest does not match blake3 of --evidence".into(),
            ));
        }
        (Some(given), _) => given.clone(),
        (None, Some(hashed)) => hashed,
        (None, None) => {
            return Err(Error::Invalid(
                "pass --evidence-digest or --evidence".into(),
            ));
        }
    };

    Ok((intent, digest))
}

pub fn run_cli(raw: Vec<String>) -> std::result::Result<(), String> {
    if raw.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{JEV_INTENT_HELP}");
        return Ok(());
    }
    let args = parse_ask_args(raw).map_err(|err| err.to_string())?;
    let (intent, digest) = resolve_ask(&args).map_err(|err| err.to_string())?;
    let decision = ask(&intent, &digest, Transport::resolve()).map_err(|err| err.to_string())?;
    write_decision(args.format, &decision).map_err(|err| err.to_string())
}

fn write_decision(format: AskFormat, decision: &Decision) -> std::io::Result<()> {
    match format {
        AskFormat::Json => {
            let mut text = serde_json::to_string_pretty(decision).expect("decision json");
            text.push('\n');
            use std::io::Write;
            let mut out = std::io::stdout();
            out.write_all(text.as_bytes())?;
            out.flush()
        }
        AskFormat::Table => {
            use std::io::Write;
            let mut out = std::io::stdout();
            writeln!(out, "choice\t{}", decision.choice.as_str())?;
            writeln!(
                out,
                "confidence\t{}",
                decision
                    .confidence
                    .map(|c| format!("{c:.3}"))
                    .unwrap_or_else(|| "-".into())
            )?;
            writeln!(out, "status\t{}", decision.status)?;
            writeln!(out, "applied\t{}", decision.applied)?;
            writeln!(out, "shadow\t{}", decision.shadow)?;
            writeln!(out, "transport\t{}", decision.transport)?;
            if let Some(reason) = &decision.reason {
                writeln!(out, "reason\t{reason}")?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn judged_body(choice: &str, noul: f64, conf: f64) -> Value {
        json!({
            "answers": {
                "apply": { "type": "choice", "choice": choice, "confidence": conf },
                "sufficient": { "type": "noul", "noul": noul, "confidence": conf }
            }
        })
    }

    #[test]
    fn collection_is_secret_and_shadow_and_has_no_key() {
        assert!(!FACET_COLLECTION.contains("sk-"));
        assert!(!FACET_COLLECTION.contains("Bearer ts_"));
        assert!(
            FACET_COLLECTION.contains("$TYPESAFE_API_KEY"),
            "comments may name the env var; the YAML must not bake a value"
        );
        let body = FACET_COLLECTION.split("data: |-").nth(1).unwrap_or("");
        assert!(
            !body.contains("TYPESAFE_API_KEY"),
            "recipe JSON must not mention the key"
        );
        assert!(FACET_COLLECTION.contains("secret: true"));
        assert!(FACET_COLLECTION.contains("typesafeApiKey"));
        assert!(FACET_COLLECTION.contains("jevShadow"));
        assert!(FACET_COLLECTION.contains("\"apply\""));
        assert!(FACET_COLLECTION.contains("\"wait\""));
        assert!(FACET_COLLECTION.contains("\"escalate\""));
        assert!(FACET_COLLECTION.contains("\"ignore\""));
        assert!(FACET_COLLECTION.contains("\"type\": \"choice\""));
        assert!(FACET_COLLECTION.contains("\"type\": \"noul\""));
        assert!(!questions().to_string().contains("TYPESAFE"));
        assert!(!FACET_COLLECTION.to_ascii_lowercase().contains("duha"));
        assert!(!FACET_COLLECTION.to_ascii_lowercase().contains("cron"));
        assert!(!FACET_COLLECTION.to_ascii_lowercase().contains("weekday"));
    }

    #[test]
    fn empty_or_low_confidence_escalates_and_never_applies() {
        let empty = decision_from_answers("fake", "replicas=2", "abc", &json!({}));
        assert_eq!(empty.choice, Choice::Escalate);
        assert_eq!(empty.status, "uncertain");
        assert!(!empty.applied && empty.shadow);

        let low =
            decision_from_answers("fake", "replicas=2", "abc", &judged_body("apply", 1.0, 0.2));
        assert_eq!(low.choice, Choice::Escalate);
        assert_eq!(low.status, "low_confidence");
        assert!(!low.applied);

        let ok = decision_from_answers(
            "fake",
            "replicas=2",
            "abc",
            &judged_body("apply", 1.0, 0.95),
        );
        assert_eq!(ok.choice, Choice::Apply);
        assert_eq!(ok.status, "judged");
        assert!(!ok.applied && ok.shadow);
        assert_eq!(ok.confidence, Some(0.95));
        assert_eq!(ok.sufficient, Some(1.0));
    }

    #[test]
    fn unknown_choice_escalates() {
        let d = decision_from_answers("fake", "x", "y", &judged_body("merge", 1.0, 0.99));
        assert_eq!(d.choice, Choice::Escalate);
        assert_eq!(d.status, "uncertain");
        assert!(!d.applied);
    }

    #[test]
    fn ask_state_binds_digest_and_clips() {
        let long = "α".repeat(5000);
        let s = ask_state(&long, "deadbeef");
        assert!(s.contains("Evidence digest:\ndeadbeef"));
        assert!(s.contains("Shadow: classify only"));
        assert!(s.len() < 5000 + 400, "intent clipped, got {}", s.len());
        assert!(!s.contains("TYPESAFE"));
    }

    #[test]
    fn missing_transport_escalates_without_approve() {
        let d = ask(
            "replicas=2",
            "abc123",
            Transport::None {
                reason: "facet_not_on_path",
            },
        )
        .unwrap();
        assert_eq!(d.choice, Choice::Escalate);
        assert_eq!(d.status, "unavailable");
        assert!(!d.applied && d.shadow);
        assert_eq!(d.reason.as_deref(), Some("facet_not_on_path"));
    }

    #[test]
    fn fake_apply_is_shadow_only() {
        let d = ask(
            "replicas=2",
            "abc123",
            Transport::Fake(FakeScript::reply(judged_body("wait", 0.0, 0.9))),
        )
        .unwrap();
        assert_eq!(d.choice, Choice::Wait);
        assert!(!d.applied && d.shadow);
        assert_eq!(d.transport, "fake");
    }

    #[test]
    fn secret_material_is_rejected() {
        let err = ask(
            "use TYPESAFE_API_KEY=sk-live",
            "abc",
            Transport::None { reason: "x" },
        )
        .unwrap_err();
        assert!(err.to_string().contains("must not carry"));
    }

    #[test]
    fn resolve_without_facet_is_none() {
        let _lock = ENV.lock().unwrap();
        let old_path = std::env::var_os("PATH");
        let old_force = std::env::var_os("HEDRON_JEV_TRANSPORT");
        let old_key = std::env::var_os("TYPESAFE_API_KEY");
        unsafe {
            std::env::set_var("PATH", "/nonexistent-hedron-jev-path");
            std::env::remove_var("HEDRON_JEV_TRANSPORT");
            std::env::set_var("TYPESAFE_API_KEY", "sk-must-not-be-read");
        }
        let t = Transport::resolve();
        match old_path {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        match old_force {
            Some(v) => unsafe { std::env::set_var("HEDRON_JEV_TRANSPORT", v) },
            None => unsafe { std::env::remove_var("HEDRON_JEV_TRANSPORT") },
        }
        match old_key {
            Some(v) => unsafe { std::env::set_var("TYPESAFE_API_KEY", v) },
            None => unsafe { std::env::remove_var("TYPESAFE_API_KEY") },
        }
        assert!(matches!(t, Transport::None { reason } if reason == "facet_not_on_path"));
    }

    #[test]
    fn facet_response_body_is_unwrapped() {
        let inner = judged_body("ignore", 0.0, 0.91);
        let wrapped = json!({
            "response": { "body": { "content": inner.to_string() } }
        });
        let answers = parse_facet_answers(&wrapped).unwrap();
        let d = decision_from_answers("facet", "x", "y", &answers);
        assert_eq!(d.choice, Choice::Ignore);
        assert!(!d.applied);
    }

    #[test]
    fn evidence_digest_is_blake3() {
        assert_eq!(
            evidence_digest_of(b"hello"),
            blake3::hash(b"hello").to_hex().to_string()
        );
    }

    static ENV: Mutex<()> = Mutex::new(());
}
