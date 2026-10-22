//! Glossary phrase boosting for the live Nemotron preview.
//!
//! transcribe.cpp biases the streaming transducer's greedy decoder towards
//! `RunOptions::vocabulary` (a per-token logit bonus along each term's
//! token path). Distinctive terms (`NixOS`, `nixpkgs`, `AGENTS.md`) gain the
//! most; short or everyday words (`AI`, `master`, `diff`, `prompt`) mostly
//! cost collateral substitutions in unrelated speech, so the default preview
//! glossary is `DICTATE_TERMS` without them.
//!
//! - `DICTATE_PREVIEW_BOOST`: weight, default [`DEFAULT_WEIGHT`]; `0` or
//!   `false` turns boosting off.
//! - `DICTATE_PREVIEW_TERMS`: comma-separated list used as is instead of
//!   the filtered `DICTATE_TERMS`.
//! - `DICTATE_PREVIEW_STOP`: extra comma-separated terms to leave out of
//!   the filtered `DICTATE_TERMS` (case-insensitive).
//!
//! Boosting cannot produce a spelling the model has no tokens for (Locika,
//! mlef, MÁG): `preview-fixes.txt` next to the keys file maps what the
//! preview hears to the term (`Lotsika -> Locika`), whole words, ignoring
//! case, on the shown text only.

use crate::config::Config;

/// Per-token logit bonus. Tuned on 52 dictations (streaming 160 ms chunks,
/// right context 1) with the filtered glossary: distinctive-term hits went
/// 11/25 -> 18/25 with 11 unrelated words changed; weight 3 gained one more
/// hit for 14 changes, weight 4 none more for 27.
pub const DEFAULT_WEIGHT: f32 = 2.0;

/// Everyday English (and everyday developer English) words: boosting them
/// rewrites unrelated speech more often than it fixes the term.
const COMMON: &[&str] = &[
    // general English
    "a", "about", "after", "again", "air", "all", "also", "and", "any", "apple", "area", "art", "back",
    "ball", "bank", "base", "bear", "bed", "bell", "best", "bird", "black", "block", "blue", "board", "boat",
    "body", "book", "box", "boy", "brain", "bread", "bridge", "bright", "brown", "bug", "call", "car", "card",
    "care", "case", "cat", "chain", "chair", "change", "check", "city", "class", "clean", "clear", "clock",
    "close", "cloud", "coffee", "cold", "color", "cool", "core", "cost", "cover", "cull", "culling", "cup",
    "cut", "dark", "data", "day", "deal", "deep", "deny", "desk", "dog", "door", "down", "draw", "dream",
    "drive", "drop", "dust", "earth", "east", "edge", "egg", "end", "eye", "face", "fact", "fall", "farm",
    "fast", "field", "file", "fill", "fire", "fish", "flag", "flat", "floor", "flow", "fly", "food", "foot",
    "force", "form", "fox", "frame", "free", "friend", "front", "fruit", "game", "gate", "gift", "girl",
    "glass", "gold", "good", "grass", "green", "ground", "group", "guard", "hand", "hard", "head", "heart",
    "heat", "help", "high", "hill", "hold", "home", "hook", "horse", "hot", "house", "ice", "idea", "iron",
    "item", "job", "joy", "key", "kind", "king", "lake", "land", "last", "law", "lead", "leaf", "left",
    "letter", "level", "life", "light", "line", "link", "list", "lock", "long", "loop", "love", "low",
    "luck", "main", "man", "map", "mark", "master", "match", "mind", "moon", "move", "music", "name",
    "net", "new", "night", "node", "north", "note", "oak", "ocean", "office", "oil", "open", "opus", "order",
    "page", "pair", "paper", "park", "part", "path", "peace", "pen", "phone", "piece", "pipe", "place",
    "plan", "plant", "point", "pool", "port", "post", "power", "press", "price", "queen", "quick", "rain",
    "range", "rate", "red", "rest", "rich", "right", "ring", "river", "road", "rock", "roof", "room",
    "root", "rose", "round", "rule", "run", "safe", "salt", "sand", "save", "scale", "school", "sea",
    "seed", "shape", "ship", "shop", "side", "sign", "silver", "sky", "small", "smart", "snow",
    "song", "sonnet", "sort", "sound", "south", "space", "spark", "speed", "spring", "square", "stack",
    "stage", "star", "start", "state", "step", "stick", "stone", "stop", "store", "storm", "story",
    "street", "sun", "table", "tail", "task", "team", "time", "tool", "top", "tower", "town", "track",
    "train", "tree", "true", "turn", "type", "unicorn", "unit", "user", "view", "wall", "watch", "water",
    "wave", "way", "west", "wheel", "white", "wind", "window", "wing", "winter", "wood", "word", "work",
    "worker", "world", "year", "yellow",
    // everyday developer words
    "agent", "agents", "api", "app", "blender", "branch", "build", "cache", "client", "code", "commit",
    "config", "debug", "deploy", "diff", "docs", "error", "fetch", "fix", "fork", "git", "hash", "issue",
    "log", "markdown", "merge", "model", "patch", "prompt", "pull", "push", "query", "release", "repo",
    "review", "server", "script", "shell", "test", "token", "update", "whisper",
];

/// Whether the decoder should be biased towards `term`. Keeps phrases,
/// mixed-case or dotted/hyphenated names and uncommon words; drops terms of
/// three characters or fewer (`AI`, `GPU`, `uv`) and common words.
pub fn distinctive(term: &str) -> bool {
    let t = term.trim();
    if t.chars().count() <= 3 {
        return false;
    }
    if t.contains(char::is_whitespace) {
        return true;
    }
    let lower = t.to_lowercase();
    if COMMON.contains(&lower.as_str()) {
        return false;
    }
    true
}

fn split(list: &str) -> impl Iterator<Item = &str> {
    list.split(',').map(str::trim).filter(|t| !t.is_empty())
}

/// The preview's phrase boost: `(terms, weight)`, or `None` when off or
/// when no term is left.
pub fn preview_boost(cfg: &Config) -> Option<(Vec<String>, f32)> {
    let weight = match cfg.get("DICTATE_PREVIEW_BOOST").map(str::trim) {
        None | Some("") => DEFAULT_WEIGHT,
        Some("false") | Some("off") => return None,
        Some(w) => w.parse::<f32>().ok().filter(|w| w.is_finite())?,
    };
    if weight <= 0.0 {
        return None;
    }
    let terms: Vec<String> = match cfg.get("DICTATE_PREVIEW_TERMS").filter(|s| !s.trim().is_empty()) {
        Some(list) => split(list).map(String::from).collect(),
        None => {
            let stop: Vec<String> = split(cfg.get("DICTATE_PREVIEW_STOP").unwrap_or("")).map(str::to_lowercase).collect();
            split(cfg.get("DICTATE_TERMS").unwrap_or(""))
                .filter(|t| distinctive(t) && !stop.contains(&t.to_lowercase()))
                .map(String::from)
                .collect()
        }
    };
    (!terms.is_empty()).then_some((terms, weight))
}

/// `heard -> meant` replacements for the preview text (see module docs).
#[derive(Default)]
pub struct PreviewFixes(Vec<(Vec<String>, String)>);

impl PreviewFixes {
    pub fn load() -> Self {
        Self::parse(&std::fs::read_to_string(crate::correct::keys_file().with_file_name("preview-fixes.txt")).unwrap_or_default())
    }

    pub fn parse(text: &str) -> Self {
        Self(text.lines()
            .map(|l| l.split('#').next().unwrap_or(""))
            .filter_map(|l| l.split_once("->"))
            .map(|(heard, meant)| (heard.split_whitespace().map(str::to_lowercase).collect::<Vec<_>>(), meant.trim().to_string()))
            .filter(|(heard, meant)| !heard.is_empty() && !meant.is_empty())
            .collect())
    }

    /// Replaces each heard phrase (punctuation after a word is kept) and
    /// keeps a leading space, which separates tentative from committed text.
    pub fn apply(&self, text: &str) -> String {
        if self.0.is_empty() {
            return text.to_string();
        }
        let toks: Vec<&str> = text.split_whitespace().collect();
        let bare = |t: &str| t.trim_end_matches(|c: char| ",.?!:;".contains(c)).to_lowercase();
        let mut out: Vec<String> = Vec::new();
        let mut i = 0;
        'next: while i < toks.len() {
            for (heard, meant) in &self.0 {
                let n = heard.len();
                if i + n <= toks.len() && (0..n).all(|k| bare(toks[i + k]) == heard[k]) {
                    let last = toks[i + n - 1];
                    out.push(format!("{meant}{}", &last[last.trim_end_matches(|c: char| ",.?!:;".contains(c)).len()..]));
                    i += n;
                    continue 'next;
                }
            }
            out.push(toks[i].to_string());
            i += 1;
        }
        let lead = if text.starts_with(char::is_whitespace) { " " } else { "" };
        format!("{lead}{}", out.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_from_env, Env};

    fn cfg(vars: &[(&str, &str)]) -> Config {
        let mut e = Env::with_vars(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        e.config_override = Some(String::new());
        load_from_env(&e).unwrap()
    }

    #[test]
    fn filters_short_and_common_terms() {
        let c = cfg(&[("DICTATE_TERMS", "Rock, AI, GPU, uv, master, NixOS, nixpkgs, AGENTS.md, home-manager, pull request, Prompt")]);
        let (terms, w) = preview_boost(&c).unwrap();
        assert_eq!(terms, ["NixOS", "nixpkgs", "AGENTS.md", "home-manager", "pull request"]);
        assert_eq!(w, DEFAULT_WEIGHT);
    }

    #[test]
    fn stoplist_and_override() {
        let c = cfg(&[("DICTATE_TERMS", "NixOS, Zorblax"), ("DICTATE_PREVIEW_STOP", "zorblax")]);
        assert_eq!(preview_boost(&c).unwrap().0, ["NixOS"]);
        let c = cfg(&[("DICTATE_TERMS", "NixOS"), ("DICTATE_PREVIEW_TERMS", "AI, master"), ("DICTATE_PREVIEW_BOOST", "1.5")]);
        assert_eq!(preview_boost(&c).unwrap(), (vec!["AI".to_string(), "master".to_string()], 1.5));
    }

    #[test]
    fn off_switches() {
        for off in ["0", "false", "off", "-1", "nonsense"] {
            assert!(preview_boost(&cfg(&[("DICTATE_TERMS", "NixOS"), ("DICTATE_PREVIEW_BOOST", off)])).is_none(), "{off}");
        }
        assert!(preview_boost(&cfg(&[("DICTATE_TERMS", "AI, master")])).is_none());
        assert!(preview_boost(&cfg(&[])).is_none());
    }

    #[test]
    fn preview_fixes_replace_whole_words() {
        let f = PreviewFixes::parse("# comment\nLotsika -> Locika\nDeep Seek -> DeepSeek\nM one -> M1\n");
        assert_eq!(f.apply("On Lotsika, ask Deep Seek about M one."), "On Locika, ask DeepSeek about M1.");
        assert_eq!(f.apply(" deep seeker stays"), " deep seeker stays");
        assert_eq!(PreviewFixes::default().apply(" x  y"), " x  y");
    }
}
