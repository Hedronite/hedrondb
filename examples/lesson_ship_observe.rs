//! Read-only lesson-ship observe. Not a CLI verb and not on a schedule.
//!
//! Opens the Lapis lattice with `mode=ro` and `PRAGMA query_only=ON`, parses
//! the lesson manifest in memory, and prints one JSON object. It does not
//! write the lattice and does not open a HedronDB store.
//!
//! ```text
//! cargo run --example lesson_ship_observe -- \
//!   --lattice PATH --manifest PATH --date YYYY-MM-DD \
//!   [--lane NAME]... \
//!   [--check-at LANE=YYYY-MM-DDTHH:MM:SSZ]... \
//!   [--glob LANE=SQLITE_GLOB]... \
//!   [--register-log PATH] \
//!   [--busy-timeout-ms N]
//! ```
//!
//! Lanes default to `duha`, `asr`, and `maghrib` when `--lane` is omitted.
//! That list is a slice-1 stand-in. Fire Watch YAML is slice 2. A lane with
//! no manifest row and no `--glob` uses the label
//! `manifest-row-missing:{lane}:{date}` and does not treat a glob hit from
//! another lane as this lane's lesson.
//!
//! Each lane gets a default `check_at` unless `--check-at` sets that lane.
//! Those are wall-clock America/New_York times resolved for `--date`:
//! duha 11:00, dhuhr 13:45, asr 17:45 (fire plus grace), maghrib 20:35.
//! A lane before that time reports `pending`.
//!
//! A malformed in-scope manifest row quarantines only the lanes it could
//! describe. The JSON object includes a `quarantine` array of active flags.
//! This binary does not open a HedronDB store, so the flags it prints are
//! this pass only. `reconcile_lesson_ships` upserts the same flags into
//! Hedron state.
//!
//! `register.log` is read from `{manifest_dir}/_tools/register.log` when that
//! file exists. `--register-log` overrides the path. A missing default log is
//! ignored. An unreadable log fails closed to `cannot_tell`.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use hedron_core::{
    cannot_tell_batch, default_lane_check_at, observe_lesson_ships, Error, LaneDue, LatticeSource,
    ObserveBatch, SourceConfig,
};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("lesson_ship_observe: {err}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse(env::args().skip(1))?;
    let yaml = match fs::read_to_string(&args.manifest) {
        Ok(text) => text,
        Err(err) => {
            let failure =
                Error::SourceUnreadable(format!("manifest {}: {err}", args.manifest.display()));
            print_batch(&args, &cannot_tell_batch(&args.lanes, &failure));
            return Ok(());
        }
    };
    let register_log = match load_register_log(&args.manifest, args.register_log.as_deref()) {
        Ok(text) => text,
        Err(err) => {
            print_batch(&args, &cannot_tell_batch(&args.lanes, &err));
            return Ok(());
        }
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| dur.as_secs() as i64)
        .unwrap_or(0);
    let batch = match LatticeSource::open(&SourceConfig {
        lattice_path: args.lattice.clone(),
        busy_timeout_ms: args.busy_timeout_ms,
    }) {
        Ok(source) => match observe_lesson_ships(
            &source,
            &yaml,
            &args.lanes,
            Some(now),
            register_log.as_deref(),
        ) {
            Ok(batch) => batch,
            Err(err) => cannot_tell_batch(&args.lanes, &err),
        },
        Err(err) => cannot_tell_batch(&args.lanes, &err),
    };
    print_batch(&args, &batch);
    Ok(())
}

fn print_batch(args: &Args, batch: &ObserveBatch) {
    let body = serde_json::json!({
        "date": args.date,
        "freshness_utc": batch.watermark_utc,
        "watermark_utc": batch.watermark_utc,
        "last_reconcile_at": batch.last_reconcile_at,
        "last_full_pass_at": batch.last_full_pass_at,
        "last_indexer_at": batch.last_indexer_at,
        "document_count": batch.document_count,
        "live_count": batch.live_count,
        "lanes": batch.lanes,
        "quarantine": batch.quarantine,
    });
    match serde_json::to_string_pretty(&body) {
        Ok(text) => println!("{text}"),
        Err(err) => eprintln!("lesson_ship_observe: json: {err}"),
    }
}

struct Args {
    lattice: PathBuf,
    manifest: PathBuf,
    register_log: Option<PathBuf>,
    date: String,
    lanes: Vec<LaneDue>,
    busy_timeout_ms: u64,
}

impl Args {
    fn parse(argv: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut lattice = None;
        let mut manifest = None;
        let mut date = None;
        let mut lane_names = Vec::new();
        let mut check_at: BTreeMap<String, String> = BTreeMap::new();
        let mut globs: BTreeMap<String, String> = BTreeMap::new();
        let mut register_log = None;
        let mut busy_timeout_ms = 2_000u64;
        let mut argv = argv.peekable();
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--help" | "-h" => return Err(help()),
                "--lattice" => lattice = Some(need(&mut argv, "--lattice")?),
                "--manifest" => manifest = Some(need(&mut argv, "--manifest")?),
                "--date" => date = Some(need(&mut argv, "--date")?),
                "--lane" => lane_names.push(need(&mut argv, "--lane")?),
                "--check-at" => {
                    let (lane, value) = split_pair(&need(&mut argv, "--check-at")?, "--check-at")?;
                    check_at.insert(lane, value);
                }
                "--glob" => {
                    let (lane, value) = split_pair(&need(&mut argv, "--glob")?, "--glob")?;
                    globs.insert(lane, value);
                }
                "--register-log" => {
                    register_log = Some(PathBuf::from(need(&mut argv, "--register-log")?));
                }
                "--busy-timeout-ms" => {
                    let raw = need(&mut argv, "--busy-timeout-ms")?;
                    busy_timeout_ms = raw
                        .parse()
                        .map_err(|_| format!("--busy-timeout-ms expects an integer, got {raw}"))?;
                }
                other => return Err(format!("unknown argument {other}\n{}", help())),
            }
        }
        let lattice = lattice.ok_or_else(|| format!("--lattice is required\n{}", help()))?;
        let manifest = manifest.ok_or_else(|| format!("--manifest is required\n{}", help()))?;
        let date = date.ok_or_else(|| format!("--date is required\n{}", help()))?;
        if date.len() != 10
            || date.as_bytes().get(4) != Some(&b'-')
            || date.as_bytes().get(7) != Some(&b'-')
        {
            return Err(format!("--date must be YYYY-MM-DD, got {date}"));
        }
        if lane_names.is_empty() {
            lane_names.extend(["duha".to_string(), "asr".to_string(), "maghrib".to_string()]);
        }
        let lanes = lane_names
            .into_iter()
            .map(|lane| LaneDue {
                check_at: check_at
                    .get(&lane)
                    .cloned()
                    .or_else(|| default_check_at(&date, &lane)),
                lesson_glob: globs.get(&lane).cloned(),
                date: date.clone(),
                lane,
            })
            .collect();
        Ok(Self {
            lattice: PathBuf::from(lattice),
            manifest: PathBuf::from(manifest),
            register_log,
            date,
            lanes,
            busy_timeout_ms,
        })
    }
}

/// America/New_York wall clock. Asr and maghrib match the design card; duha
/// and dhuhr are morning and midday placeholders until Fire Watch is parsed.
fn default_check_at(date: &str, lane: &str) -> Option<String> {
    default_lane_check_at(date, lane)
}

fn load_register_log(
    manifest: &Path,
    override_path: Option<&Path>,
) -> Result<Option<String>, Error> {
    let path = if let Some(path) = override_path {
        path.to_path_buf()
    } else {
        let Some(dir) = manifest.parent() else {
            return Ok(None);
        };
        dir.join("_tools").join("register.log")
    };
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound && override_path.is_none() => Ok(None),
        Err(err) => Err(Error::SourceUnreadable(format!(
            "register log {}: {err}",
            path.display()
        ))),
    }
}

fn need(argv: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    argv.next()
        .filter(|value| !value.starts_with("--"))
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn split_pair(raw: &str, flag: &str) -> Result<(String, String), String> {
    let (lane, value) = raw
        .split_once('=')
        .ok_or_else(|| format!("{flag} expects LANE=VALUE, got {raw}"))?;
    if lane.is_empty() || value.is_empty() {
        return Err(format!("{flag} expects LANE=VALUE, got {raw}"));
    }
    Ok((lane.to_string(), value.to_string()))
}

fn help() -> String {
    "usage: lesson_ship_observe --lattice PATH --manifest PATH --date YYYY-MM-DD \
[--lane NAME]... [--check-at LANE=ISO]... [--glob LANE=GLOB]... \
[--register-log PATH] [--busy-timeout-ms N]"
        .into()
}
