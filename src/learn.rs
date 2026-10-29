//! Explicit capture of an edited terminal draft. Never reads terminal scrollback,
//! submits a prompt, calls an API, or silently turns message rewrites into gold.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use crate::{
    learn_match::{self, CaptureMatch, MatchKind, Source},
    pipeline::{self, Ctx},
    target::{self, DraftTarget},
    x11::X,
};

const MAX_AGE: f64 = 1800.0;
const CAPTURE_SECS: f64 = 8.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delivery {
    pub id: String,
    pub entry: String,
    pub at: f64,
    pub target: DraftTarget,
    pub text: String,
    pub audio_aligned: bool,
    #[serde(default)]
    pub raw: Option<String>,
    #[serde(default)]
    pub corrected: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Pending {
    nonce: String,
    target: DraftTarget,
    created: f64,
    expires: f64,
    deliveries: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Capture {
    pub id: String,
    pub at: f64,
    pub target: DraftTarget,
    pub deliveries: Vec<Delivery>,
    pub matches: Vec<CaptureMatch>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    id: String,
    error: Option<String>,
    words: usize,
    formatting: usize,
    review: usize,
    #[serde(default)]
    feedback: String,
}

fn root(ctx: &Ctx) -> PathBuf {
    ctx.paths.state_dir.join("learn")
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() < 160
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

fn pending_path(ctx: &Ctx, pane: &str) -> Result<PathBuf> {
    let number = pane.strip_prefix('%').context("invalid tmux pane")?;
    ensure!(
        !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit()),
        "invalid tmux pane"
    );
    Ok(root(ctx)
        .join("pending")
        .join(format!("pane-{number}.json")))
}

fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    private_dir(path.parent().context("missing parent")?)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn focused_target() -> Result<DraftTarget> {
    let pid = X::connect()?
        .active_window_pid()
        .context("no focused terminal")?;
    target::draft_target(pid).context("focus a Codex or Claude Code draft in tmux")
}

/// Call only for an archived utterance, after attempting delivery to this target.
pub fn record_delivery(
    ctx: &Ctx,
    entry: &Path,
    text: &str,
    destination: DraftTarget,
    audio_aligned: bool,
) -> Result<()> {
    let raw = fs::read_to_string(entry.join("transcript.raw.txt")).ok();
    let corrected = fs::read_to_string(entry.join("transcript.corrected.txt"))
        .ok()
        .or_else(|| raw.clone());
    let entry = entry
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid history id")?;
    ensure!(safe_id(entry), "invalid history id");
    let at = pipeline::now_unix();
    let record = Delivery {
        id: format!("{:020}-{}-{entry}", (at * 1e6) as u64, std::process::id()),
        entry: entry.into(),
        at,
        target: destination,
        text: text.into(),
        audio_aligned,
        raw,
        corrected,
    };
    write_json(
        &root(ctx)
            .join("deliveries")
            .join(format!("{}.json", record.id)),
        &record,
    )
}

fn recent(ctx: &Ctx, target: &DraftTarget, now: f64) -> Vec<Delivery> {
    let mut rows: Vec<Delivery> = fs::read_dir(root(ctx).join("deliveries"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| serde_json::from_slice::<Delivery>(&fs::read(e.path()).ok()?).ok())
        .filter(|d| d.target == *target && d.at <= now && now - d.at <= MAX_AGE)
        .collect();
    rows.sort_by(|a, b| a.at.total_cmp(&b.at));
    if rows.len() > 20 {
        rows.drain(..rows.len() - 20);
    }
    rows
}

pub fn command(ctx: &Ctx) -> Result<()> {
    let result = request_capture(ctx);
    if let Err(e) = &result {
        pipeline::notify(ctx, "dictate: Correction not saved", &e.to_string(), 5000);
    }
    result
}

fn request_capture(ctx: &Ctx) -> Result<()> {
    ensure!(
        !ctx.paths.pidfile.exists(),
        "finish dictating before capturing corrections"
    );
    let target = focused_target()?;
    let now = pipeline::now_unix();
    let deliveries = recent(ctx, &target, now);
    ensure!(
        !deliveries.is_empty(),
        "no recent dictation in this draft's pane; dictate once first"
    );
    let path = pending_path(ctx, &target.pane)?;
    private_dir(path.parent().unwrap())?;
    // Hold this across the editor's rename/claim and response; no stale lock on exit.
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.try_lock()
        .map_err(|_| anyhow::anyhow!("a correction capture is already waiting"))?;
    if let Ok(bytes) = fs::read(&path) {
        let expired = match serde_json::from_slice::<Pending>(&bytes) {
            Ok(old) => old.expires < now,
            Err(_) => {
                fs::metadata(&path)?
                    .modified()?
                    .elapsed()
                    .unwrap_or_default()
                    .as_secs_f64()
                    > CAPTURE_SECS
            }
        };
        ensure!(expired, "a correction capture is already waiting");
        fs::remove_file(&path)?;
    }
    let mut random = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let nonce = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let pending = Pending {
        nonce: nonce.clone(),
        target: target.clone(),
        created: now,
        expires: now + CAPTURE_SECS,
        deliveries: deliveries.into_iter().map(|d| d.id).collect(),
    };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(&serde_json::to_vec(&pending)?)?;
    drop(file);
    let response = root(ctx).join("responses").join(format!("{nonce}.json"));
    let result = (|| -> Result<()> {
        ensure!(
            focused_target()? == target,
            "the draft focus changed; try again"
        );
        ensure!(
            Command::new("tmux")
                .args(["send-keys", "-t", &target.pane, "C-g"])
                .status()?
                .success(),
            "could not request the draft"
        );
        let deadline = Instant::now() + Duration::from_secs_f64(CAPTURE_SECS + 2.0);
        while Instant::now() < deadline {
            if let Ok(bytes) = fs::read(&response) {
                let r: Response = serde_json::from_slice(&bytes)?;
                if let Some(error) = r.error {
                    bail!("{error}");
                }
                pipeline::notify(ctx, "✓ Draft captured", &r.feedback, 10000);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        bail!("draft capture timed out; the editor wrapper may not be configured")
    })();
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(&response);
    result
}

/// `Ok(false)` means ordinary editing: the shell wrapper must launch Neovim.
pub fn editor_capture(ctx: &Ctx, args: &[OsString]) -> Result<bool> {
    let Ok(pane) = std::env::var("TMUX_PANE") else {
        return Ok(false);
    };
    let path = pending_path(ctx, &pane)?;
    let Ok(bytes) = fs::read(&path) else {
        return Ok(false);
    };
    let pending: Pending = serde_json::from_slice(&bytes)?;
    let now = pipeline::now_unix();
    if now < pending.created || now > pending.expires {
        let _ = fs::remove_file(&path);
        return Ok(false);
    }
    if pending.target.pane != pane || !target::editor_from_target(&pending.target) {
        return Ok(false);
    }
    ensure!(
        safe_id(&pending.nonce) && pending.nonce.len() == 32,
        "invalid capture nonce"
    );
    let claimed = path.with_extension(format!("{}.claimed", pending.nonce));
    fs::rename(&path, &claimed)?;
    let result = save_capture(ctx, &pending, args);
    let response = match &result {
        Ok(r) => Response {
            id: r.id.clone(),
            error: None,
            words: r.words,
            formatting: r.formatting,
            review: r.review,
            feedback: r.feedback.clone(),
        },
        Err(e) => Response {
            id: pending.nonce.clone(),
            error: Some(e.to_string()),
            words: 0,
            formatting: 0,
            review: 0,
            feedback: String::new(),
        },
    };
    write_json(
        &root(ctx)
            .join("responses")
            .join(format!("{}.json", pending.nonce)),
        &response,
    )?;
    let _ = fs::remove_file(claimed);
    result?;
    Ok(true)
}

fn draft_without_context(raw: &str, app: &str) -> String {
    if app == "claude" && raw.starts_with("# ─── Claude's last response") {
        let lines: Vec<_> = raw.lines().collect();
        if let Some(i) = lines
            .iter()
            .position(|s| s.starts_with("# ─── Write your reply below this line"))
        {
            return lines[i + 1..]
                .join("\n")
                .trim_start_matches('\n')
                .to_string();
        }
    }
    raw.to_string()
}

fn save_capture(ctx: &Ctx, pending: &Pending, args: &[OsString]) -> Result<Response> {
    let files: Vec<_> = args
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .collect();
    ensure!(files.len() == 1, "expected one external-editor draft file");
    ensure!(
        fs::metadata(&files[0])?.len() <= 1_048_576,
        "draft is too large to capture"
    );
    let raw = fs::read_to_string(&files[0]).context("could not read the editor draft")?;
    let draft = draft_without_context(&raw, &pending.target.app);
    ensure!(!draft.trim().is_empty(), "the draft is empty");
    let mut deliveries = Vec::new();
    for id in &pending.deliveries {
        ensure!(safe_id(id), "invalid delivery id");
        let d: Delivery = serde_json::from_slice(&fs::read(
            root(ctx).join("deliveries").join(format!("{id}.json")),
        )?)?;
        ensure!(
            d.id == *id && d.target == pending.target,
            "delivery target mismatch"
        );
        deliveries.push(d);
    }
    let sources: Vec<_> = deliveries
        .iter()
        .map(|d| Source {
            id: d.id.clone(),
            text: d.text.clone(),
        })
        .collect();
    let matches = learn_match::match_draft(&sources, &draft);
    let response = Response {
        id: pending.nonce.clone(),
        error: None,
        words: matches
            .iter()
            .filter(|m| matches!(m.kind, MatchKind::Lexical))
            .map(|m| m.edits.len())
            .sum(),
        formatting: matches
            .iter()
            .filter(|m| matches!(m.kind, MatchKind::Formatting))
            .count(),
        review: matches
            .iter()
            .filter(|m| matches!(m.kind, MatchKind::Review))
            .count(),
        feedback: capture_feedback(&matches, &draft),
    };
    let capture = Capture {
        id: pending.nonce.clone(),
        at: pipeline::now_unix(),
        target: pending.target.clone(),
        deliveries,
        matches,
    };
    ensure!(
        capture.at <= pending.expires + 1.0,
        "draft capture expired before it could be saved"
    );
    let dir = root(ctx).join("captures").join(&capture.id);
    private_dir(&dir)?;
    fs::write(dir.join("editor.raw.txt"), raw)?;
    fs::write(dir.join("draft.txt"), draft)?;
    write_json(&dir.join("capture.json"), &capture)?;
    Ok(response)
}

/// A bounded, readable receipt, including the actual draft for captures with no edits.
fn capture_feedback(matches: &[CaptureMatch], draft: &str) -> String {
    fn preview(text: &str, limit: usize) -> String {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut chars = text.chars();
        let mut short: String = chars.by_ref().take(limit).collect();
        if chars.next().is_some() {
            short.push('…');
        }
        short
    }
    let edits: Vec<_> = matches
        .iter()
        .filter(|m| m.kind == MatchKind::Lexical)
        .flat_map(|m| &m.edits)
        .collect();
    let formatting = matches
        .iter()
        .filter(|m| m.kind == MatchKind::Formatting)
        .count();
    let review = matches
        .iter()
        .filter(|m| m.kind == MatchKind::Review)
        .count();
    let mut status = Vec::new();
    if !edits.is_empty() {
        status.push(format!(
            "{} word correction{}",
            edits.len(),
            if edits.len() == 1 { "" } else { "s" }
        ));
    }
    if formatting > 0 {
        status.push(format!(
            "{formatting} formatting change{}",
            if formatting == 1 { "" } else { "s" }
        ));
    }
    if review > 0 {
        status.push(format!(
            "{review} passage{} saved for review",
            if review == 1 { "" } else { "s" }
        ));
    }
    let mut lines = vec![if status.is_empty() {
        "No changes found in the matched dictation.".into()
    } else {
        status.join(" · ")
    }];
    for edit in edits.iter().take(4) {
        lines.push(format!(
            "{} → {}",
            preview(&edit.from, 45),
            preview(&edit.to, 45)
        ));
    }
    if edits.len() > 4 {
        lines.push(format!("+{} more corrections", edits.len() - 4));
    }
    lines.push(format!("\nCaptured draft:\n{}", preview(draft, 220)));
    lines.join("\n")
}

pub fn list(ctx: &Ctx, n: usize) -> Result<String> {
    let mut rows: Vec<Capture> = fs::read_dir(root(ctx).join("captures"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| serde_json::from_slice(&fs::read(e.path().join("capture.json")).ok()?).ok())
        .collect();
    rows.sort_by(|a, b| b.at.total_cmp(&a.at));
    let mut out = String::new();
    for r in rows.iter().take(n) {
        let words: usize = r
            .matches
            .iter()
            .filter(|m| matches!(m.kind, MatchKind::Lexical))
            .map(|m| m.edits.len())
            .sum();
        let review = r
            .matches
            .iter()
            .filter(|m| matches!(m.kind, MatchKind::Review))
            .count();
        out.push_str(&format!(
            "{}  {:6} {:>4}  {} word fixes, {} for review\n",
            r.id, r.target.app, r.target.pane, words, review
        ));
    }
    Ok(out)
}

pub fn show(ctx: &Ctx, id: &str, approve: Option<&str>) -> Result<String> {
    ensure!(safe_id(id), "invalid capture id");
    let r: Capture = serde_json::from_slice(&fs::read(
        root(ctx).join("captures").join(id).join("capture.json"),
    )?)?;
    if let Some(entry) = approve {
        ensure!(safe_id(entry), "use a complete history entry id");
        let matching: Vec<_> = r
            .matches
            .iter()
            .filter(|m| {
                m.source_ids.len() == 1
                    && r.deliveries
                        .iter()
                        .any(|d| d.id == m.source_ids[0] && d.entry == entry)
            })
            .collect();
        ensure!(
            matching.len() == 1,
            "entry does not have one unambiguous match in this capture"
        );
        let m = matching[0];
        ensure!(
            !matches!(m.kind, MatchKind::Review),
            "this edit needs manual alignment; it cannot become a gold transcript automatically"
        );
        let d = r
            .deliveries
            .iter()
            .find(|d| d.id == m.source_ids[0])
            .unwrap();
        ensure!(
            d.audio_aligned,
            "this dictation used a spoken edit; its saved audio does not cover the full text"
        );
        let dir = ctx.history.dir.join(entry);
        ensure!(dir.join("audio.wav").is_file(), "saved audio is missing");
        let gold = dir.join("transcript.gold.txt");
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&gold)
        {
            Ok(mut file) => writeln!(file, "{}", m.edited.trim())?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = fs::read_to_string(&gold)?;
                ensure!(
                    existing.trim() == m.edited.trim(),
                    "a different gold transcript already exists; it was preserved"
                );
            }
            Err(e) => return Err(e.into()),
        }
        write_json(
            &dir.join("transcript.gold.source.json"),
            &serde_json::json!({"capture": id, "source": "edited_draft_explicit_approval", "at": pipeline::now_unix()}),
        )?;
        return Ok(format!("Approved {} from capture {}\n", entry, id));
    }
    Ok(format!("{}\n", serde_json::to_string_pretty(&r)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Ctx, DraftTarget, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let mut paths = crate::paths::Paths::resolve(&crate::config::Env::default()).unwrap();
        paths.state_dir = temp.path().join("state");
        paths.datadir = temp.path().join("cache");
        paths.history_dir = temp.path().join("history");
        paths.rundir = temp.path().join("runtime");
        paths.log = paths.state_dir.join("dictate.log");
        paths.pidfile = temp.path().join("recording.pid");
        paths.ensure_private_dirs().unwrap();
        let ctx = Ctx::new(crate::config::Config::default(), paths, true).unwrap();
        let target = DraftTarget {
            window_pid: 1,
            pane: "%12".into(),
            pane_pid: 2,
            app: "codex".into(),
            app_pid: 3,
            app_started: "456".into(),
            tty: "/dev/pts/123".into(),
        };
        let entry = ctx.history.dir.join("2026-10-26_12-00-00-123-0");
        fs::create_dir(&entry).unwrap();
        fs::write(entry.join("audio.wav"), b"fixture audio, never modified").unwrap();
        (temp, ctx, target, entry)
    }

    fn edited_capture(
        ctx: &Ctx,
        target: DraftTarget,
        entry: &Path,
        aligned: bool,
    ) -> (Response, PathBuf) {
        let original = "Please ask the worker to fix the calling bug before we run the next test.";
        let edited = "Please ask the worker to fix the culling bug before we run the next test.";
        fs::write(entry.join("transcript.raw.txt"), original).unwrap();
        record_delivery(ctx, entry, original, target.clone(), aligned).unwrap();
        let now = pipeline::now_unix();
        let pending = Pending {
            nonce: "0123456789abcdef0123456789abcdef".into(),
            target: target.clone(),
            created: now,
            expires: now + 8.0,
            deliveries: recent(ctx, &target, now)
                .into_iter()
                .map(|d| d.id)
                .collect(),
        };
        let draft = ctx.paths.datadir.join("draft with spaces.md");
        fs::write(&draft, edited).unwrap();
        let result = save_capture(
            ctx,
            &pending,
            &[OsString::from("+3"), draft.clone().into_os_string()],
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&draft).unwrap(), edited);
        assert_eq!(result.words, 1);
        assert!(result
            .feedback
            .contains("1 word correction\ncalling → culling"));
        assert!(result.feedback.contains(edited));
        (result, draft)
    }

    #[test]
    fn capture_preserves_draft_and_requires_explicit_gold_approval() {
        let (_temp, ctx, target, entry) = fixture();
        let (result, _) = edited_capture(&ctx, target, &entry, true);
        assert!(!entry.join("transcript.gold.txt").exists());
        let entry_id = entry.file_name().unwrap().to_str().unwrap();
        show(&ctx, &result.id, Some(entry_id)).unwrap();
        assert!(fs::read_to_string(entry.join("transcript.gold.txt"))
            .unwrap()
            .contains("culling bug"));
        assert_eq!(
            fs::read(entry.join("audio.wav")).unwrap(),
            b"fixture audio, never modified"
        );
        fs::write(
            entry.join("transcript.gold.txt"),
            "a previously checked transcript",
        )
        .unwrap();
        assert!(show(&ctx, &result.id, Some(entry_id)).is_err());
        assert_eq!(
            fs::read_to_string(entry.join("transcript.gold.txt")).unwrap(),
            "a previously checked transcript"
        );
    }

    #[test]
    fn spoken_edit_audio_cannot_be_approved_as_a_full_transcript() {
        let (_temp, ctx, target, entry) = fixture();
        let (result, _) = edited_capture(&ctx, target, &entry, false);
        assert!(show(
            &ctx,
            &result.id,
            Some(entry.file_name().unwrap().to_str().unwrap())
        )
        .is_err());
        assert!(!entry.join("transcript.gold.txt").exists());
    }

    #[test]
    fn deliveries_are_scoped_to_the_same_cli_instance_and_time_window() {
        let (_temp, ctx, mut target, entry) = fixture();
        record_delivery(&ctx, &entry, "some words", target.clone(), true).unwrap();
        let now = pipeline::now_unix();
        assert_eq!(recent(&ctx, &target, now).len(), 1);
        assert!(recent(&ctx, &target, now + MAX_AGE + 1.0).is_empty());
        target.app_started = "new instance".into();
        assert!(recent(&ctx, &target, now).is_empty());
        target.app_started = "456".into();
        target.pane = "%13".into();
        assert!(recent(&ctx, &target, now).is_empty());
    }
    #[test]
    fn context_removed_only_for_the_exact_claude_header() {
        let raw = "# ─── Claude's last response (for reference; removed on save) ───\n# old\n# ─── Write your reply below this line ───\n\nfix the culling bug";
        assert_eq!(draft_without_context(raw, "claude"), "fix the culling bug");
        assert_eq!(draft_without_context(raw, "codex"), raw);
        assert_eq!(
            draft_without_context("# My heading\nfix it", "claude"),
            "# My heading\nfix it"
        );
    }
    #[test]
    fn identifiers_cannot_escape_the_capture_store() {
        for id in ["", "..", "../other", "/tmp/a", "%2"] {
            assert!(!safe_id(id));
        }
        assert!(safe_id("2026-10-26_13-00-52-792402-0"));
    }

    #[test]
    fn capture_receipt_distinguishes_unchanged_formatting_and_review() {
        let source = Source {
            id: "a".into(),
            text: "Please run cargo test.".into(),
        };
        for (draft, expected) in [
            (
                "Please run cargo test.",
                "No changes found in the matched dictation.",
            ),
            ("please run `cargo test`!", "1 formatting change"),
            (
                "The weather is clear and the train leaves at noon.",
                "1 passage saved for review",
            ),
        ] {
            let receipt = capture_feedback(
                &learn_match::match_draft(std::slice::from_ref(&source), draft),
                draft,
            );
            assert!(receipt.starts_with(expected), "{receipt}");
            assert!(receipt.ends_with(draft), "{receipt}");
        }
        let long = "日本語 ".repeat(200);
        let receipt = capture_feedback(&learn_match::match_draft(&[source], &long), &long);
        assert!(receipt.ends_with('…'));
        assert!(receipt.chars().count() < 300);
    }
}
