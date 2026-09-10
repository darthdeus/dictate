//! Timing summaries (metadata only, never transcript text).
//!
//! Ports `release_timings.py` and the inline `timings` preview summary in
//! `bin/dictate`. Both logs are TSV with event names + timestamps.

use anyhow::Result;
use std::path::Path;

fn read_tsv(path: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.split('\t').map(str::to_string).collect())
        .collect()
}

/// `dictate timings release`: stage + elapsed table from `event<TAB>timestamp`.
pub fn summarize_release(path: &Path) -> Result<String> {
    let mut out = String::new();
    let rows = read_tsv(path);
    if rows.len() < 2 {
        return Ok("No complete timing events yet.\n".to_string());
    }
    let mut events: Vec<(String, f64)> = vec![];
    for r in rows.iter().skip(1) {
        if r.len() < 2 {
            continue;
        }
        if let Ok(stamp) = r[1].parse::<f64>() {
            if stamp.is_finite() && !r[0].is_empty() {
                events.push((r[0].clone(), stamp));
            }
        }
    }
    if events.is_empty() {
        return Ok("No complete timing events yet.\n".to_string());
    }
    let start = events[0].1;
    let mut prev = start;
    out.push_str(&format!("{:28} {:>10} {:>11}\n", "Stage completed", "stage ms", "elapsed ms"));
    for (name, stamp) in &events {
        out.push_str(&format!(
            "{:28} {:10.1} {:11.1}\n",
            name,
            (stamp - prev) * 1000.0,
            (stamp - start) * 1000.0
        ));
        prev = *stamp;
    }
    match events.iter().find(|(n, _)| n == "paste_shortcut_sent") {
        Some((_, sent)) => {
            out.push_str(&format!(
                "Command entry to paste shortcut sent: {:.1} ms\n",
                (sent - start) * 1000.0
            ));
        }
        None => out.push_str(
            "No paste shortcut recorded: still running, skipped, failed, or used typing/clipboard-only mode.\n",
        ),
    }
    out.push_str("Starts at script entry, after key-binding dispatch. Does not measure when the target app inserts text.\n");
    out.push_str(&format!("Log: {}\n", path.display()));
    Ok(out)
}

/// `dictate timings`: min/median/p95/max over preview request stages.
pub fn summarize_preview(path: &Path) -> Result<String> {
    let rows = read_tsv(path);
    if rows.is_empty() {
        return Ok("No preview timings yet. Record a dictation with live preview enabled.\n".into());
    }
    let header = &rows[0];
    let idx = |name: &str| header.iter().position(|h| h == name);
    let (Some(ip), Some(ir), Some(ifin), Some(iw), Some(ig), Some(is)) = (
        idx("prep_ms"),
        idx("request_ms"),
        idx("finish_ms"),
        idx("work_ms"),
        idx("start_gap_ms"),
        idx("status"),
    ) else {
        return Ok("No preview timings yet. Record a dictation with live preview enabled.\n".into());
    };
    let mut counts = (0usize, 0usize);
    let mut cols: std::collections::HashMap<&str, Vec<f64>> = [
        ("prep_ms", vec![]),
        ("request_ms", vec![]),
        ("finish_ms", vec![]),
        ("work_ms", vec![]),
        ("start_gap_ms", vec![]),
    ]
    .into_iter()
    .collect();
    for r in rows.iter().skip(1) {
        if r.len() <= is.max(ip).max(ir).max(ifin).max(iw).max(ig) {
            continue;
        }
        if !["ok", "prepare_error", "request_error"].contains(&r[is].as_str()) {
            continue;
        }
        let vals: Option<Vec<f64>> = [ip, ir, ifin, iw, ig]
            .iter()
            .map(|&i| r[i].parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0))
            .collect();
        let Some(v) = vals else { continue };
        counts.0 += 1;
        if r[is] != "ok" {
            counts.1 += 1;
            continue;
        }
        cols.get_mut("prep_ms").unwrap().push(v[0]);
        cols.get_mut("request_ms").unwrap().push(v[1]);
        cols.get_mut("finish_ms").unwrap().push(v[2]);
        cols.get_mut("work_ms").unwrap().push(v[3]);
        if v[4] > 0.0 {
            cols.get_mut("start_gap_ms").unwrap().push(v[4]);
        }
    }
    if counts.0 == 0 {
        return Ok("No preview timings yet. Record a dictation with live preview enabled.\n".into());
    }
    let mut out = format!("Latest recording: {} requests, {} failures\n", counts.0, counts.1);
    out.push_str(&format!("{:18} {:>8} {:>8} {:>8} {:>8}\n", "milliseconds", "min", "median", "p95", "max"));
    for (key, label) in [
        ("prep_ms", "Audio preparation"),
        ("request_ms", "ASR request"),
        ("finish_ms", "Text + submission"),
        ("work_ms", "Total work"),
        ("start_gap_ms", "Request interval"),
    ] {
        let mut v = cols[key].clone();
        if v.is_empty() {
            continue;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = if v.len() % 2 == 1 {
            v[v.len() / 2]
        } else {
            (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
        };
        let p95 = v[(v.len() as f64 * 0.95).ceil() as usize - 1];
        out.push_str(&format!("{label:18} {min:8.1} {median:8.1} {p95:8.1} {max:8.1}\n",
            min = v[0], max = v[v.len() - 1]));
    }
    out.push_str("ASR request includes HTTP and response parsing; total work excludes waiting and display rendering.\n");
    out.push_str(&format!("Log: {}\n", path.display()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_summary_marks_shortcut() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "event\ttimestamp\ncommand_started\t100.0\npaste_shortcut_sent\t100.5\n",
        )
        .unwrap();
        let s = summarize_release(tmp.path()).unwrap();
        assert!(s.contains("500.0 ms"), "{s}");
    }

    #[test]
    fn release_without_shortcut_explains() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "event\ttimestamp\ncommand_started\t100.0\n").unwrap();
        assert!(summarize_release(tmp.path()).unwrap().contains("No paste shortcut"));
    }
}
