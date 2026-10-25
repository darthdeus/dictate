//! What follows a dictation in the focused window, and tmux navigation.
//!
//! The ending goes after the text, so the input visibly waits for the next
//! dictation instead of the next one having to guess:
//! - In a paragraph app (`DICTATE_PARAGRAPH_APPS`, default `claude|codex`),
//!   Ctrl+J twice: a new paragraph. Only these apps get it: in a shell
//!   Ctrl+J is Enter. Elsewhere a space.
//! - `stop --sentence`: the sentence ends, `". "` (just `" "` when the text
//!   already ends in `.`, `?` or `!`), so the next dictation starts a new one.
//!
//! The app is the command in the focused tmux pane: the focused X window's
//! pid leads (through `/proc`) to its `tmux: client`, and `tmux list-clients`
//! to that client's active pane. `DICTATE_JOIN=off` pastes the text alone.

use std::collections::HashMap;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    Paragraph,
    Sentence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    None,
    /// `" "` after the text.
    Space,
    /// `". "` after the text, or `" "` after its own final punctuation.
    Sentence,
    /// Ctrl+J Ctrl+J after the text, as keys: Claude Code folds a pasted
    /// multi-line text into a `[Pasted text]` placeholder.
    Paragraph,
}

impl Ending {
    pub fn apply(self, text: &str) -> String {
        match self {
            Ending::Space => format!("{text} "),
            Ending::Sentence if text.ends_with(['.', '?', '!', '…']) => format!("{text} "),
            Ending::Sentence => format!("{text}. "),
            Ending::None | Ending::Paragraph => text.to_string(),
        }
    }
}

/// Decides the ending. `command` is the focused tmux pane's, if any.
pub fn plan(command: Option<&str>, join: Join, paragraph_apps: &str) -> Ending {
    match join {
        Join::Sentence => Ending::Sentence,
        Join::Paragraph if command.is_some_and(|c| paragraph_apps.split('|').any(|a| a == c)) => Ending::Paragraph,
        Join::Paragraph => Ending::Space,
    }
}

/// `fields` (tab-separated tmux formats) of the tmux client running under
/// `window_pid`.
fn client_fields(window_pid: u32, fields: &str) -> Option<Vec<String>> {
    let clients = descendants(window_pid)
        .into_iter()
        .filter(|p| std::fs::read_to_string(format!("/proc/{p}/comm")).is_ok_and(|c| c.trim() == "tmux: client"))
        .collect::<Vec<_>>();
    if clients.is_empty() {
        return None;
    }
    let out = Command::new("tmux")
        .args(["list-clients", "-F", &format!("#{{client_pid}}\t{fields}")])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.split('\t').map(str::to_string).collect::<Vec<_>>())
        .find(|f| clients.iter().any(|c| c.to_string() == f[0]))
        .map(|f| f[1..].to_vec())
}

/// The command in the active pane of the tmux client under `window_pid`.
pub fn tmux_command(window_pid: u32) -> Option<String> {
    client_fields(window_pid, "#{pane_current_command}")?.into_iter().next()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    /// Next (+1) or previous (-1) window of the session, wrapping.
    Window(i32),
    /// Next or previous pane of the current window, wrapping.
    Pane(i32),
}

/// Moves the tmux client under `window_pid` (the macro pad's knob).
pub fn navigate(window_pid: u32, nav: Nav) -> Option<()> {
    let session = client_fields(window_pid, "#{session_id}")?.into_iter().next()?;
    let (cmd, target) = match nav {
        Nav::Window(step) => ("select-window", format!("{session}:{}", if step > 0 { "+" } else { "-" })),
        Nav::Pane(step) => ("select-pane", format!("{session}:.{}", if step > 0 { "+" } else { "-" })),
    };
    Command::new("tmux").args([cmd, "-t", &target]).status().ok()?;
    Some(())
}

/// All processes below `root`, from `/proc/*/stat`.
fn descendants(root: u32) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
        // `pid (comm) state ppid ...`; comm may hold spaces and parens.
        let ppid = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse().ok());
        if let Some(ppid) = ppid {
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut out = Vec::new();
    let mut todo = vec![root];
    while let Some(p) = todo.pop() {
        for &c in children.get(&p).into_iter().flatten() {
            out.push(c);
            todo.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPS: &str = "claude|codex";

    #[test]
    fn paragraph_only_in_paragraph_apps() {
        assert_eq!(plan(Some("claude"), Join::Paragraph, APPS), Ending::Paragraph);
        assert_eq!(plan(Some("codex"), Join::Paragraph, APPS), Ending::Paragraph);
        assert_eq!(plan(Some("zsh"), Join::Paragraph, APPS), Ending::Space);
        assert_eq!(plan(None, Join::Paragraph, APPS), Ending::Space);
    }

    #[test]
    fn sentence_ends_the_sentence() {
        assert_eq!(plan(Some("claude"), Join::Sentence, APPS), Ending::Sentence);
        assert_eq!(Ending::Sentence.apply("it works"), "it works. ");
        assert_eq!(Ending::Sentence.apply("does it work?"), "does it work? ");
        assert_eq!(Ending::Space.apply("hi"), "hi ");
        assert_eq!(Ending::Paragraph.apply("hi"), "hi");
    }
}
