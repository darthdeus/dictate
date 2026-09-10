//! Deterministic transcript post-processing.
//!
//! Ports `restyle()` and the `NOISE_PHRASES` gate in `bin/dictate`.
//! Recognition is untouched: style is cosmetic and must not perturb word
//! choice, so it runs last. The noise blocklist matches whole-string only.

use std::collections::HashSet;

/// Default stock-hallucination blocklist (lowercased, punctuation stripped).
pub const DEFAULT_NOISE_PHRASES: &str = "you|thank you|thanks|thanks for watching|thank you for watching|bye|goodbye|okay|so|uh|um|hmm|mm|ah|oh|please subscribe|subtitles by the amaraorg community|amaraorg";

/// Rewrite sentence-initial capitals to lowercase and `. ` to ` ... `,
/// preserving glossary words (`prompt`), `I`/`I'm`, internal caps
/// (`SvelteKit`, `NixOS`) and all-caps words.
pub fn restyle(text: &str, enabled: bool, prompt: &str) -> String {
    if !enabled {
        return text.to_string();
    }
    let keep: HashSet<String> = prompt
        .split([',', ' ', '\t', '\n'])
        .map(|w| w.trim().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    // Collapse whitespace first (matches `tr -s` + python split/join).
    let collapsed: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    let mut out = collapsed.join(" ");
    // Lowercase sentence-initial words: start or after `. `, `? `, `! `.
    out = lowercase_initials(&out, &keep);
    // Sentence-separating periods become " ... " (only with trailing space,
    // so "itch.io" and "3.5" are safe). Final period is dropped.
    out = replace_sentence_periods(&out);
    out.trim().to_string()
}

fn is_simple_capitalized(word: &str) -> bool {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => (),
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c == '\'')
}

fn lowercase_initials(text: &str, keep: &HashSet<String>) -> String {
    // Mirror the Python `(^|[.!?]\s+)(word)`: only sentence-initial words are
    // candidates for lowercasing. Word spans keep every byte covered exactly
    // once, so the loop always advances.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(st) = start.take() {
                spans.push((st, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start.take() {
        spans.push((st, text.len()));
    }
    let mut out = String::with_capacity(text.len());
    let mut last_end = 0;
    for (idx, (a, b)) in spans.iter().enumerate() {
        out.push_str(&text[last_end..*a]);
        let word = &text[*a..*b];
        let initial = if idx == 0 {
            true
        } else {
            text[..*a].trim_end().chars().last().map(|c| matches!(c, '.' | '?' | '!')).unwrap_or(false)
        };
        if initial {
            out.push_str(&maybe_lower(word, keep));
        } else {
            out.push_str(word);
        }
        last_end = *b;
    }
    out.push_str(&text[last_end..]);
    out
}

fn maybe_lower(word: &str, keep: &HashSet<String>) -> String {
    if keep.contains(&word.to_lowercase()) {
        return word.to_string();
    }
    if word == "I" || word.starts_with("I'") {
        return word.to_string();
    }
    if is_simple_capitalized(word) {
        let mut c = word.chars();
        let first = c.next().unwrap().to_lowercase().to_string();
        return format!("{first}{}", c.as_str());
    }
    word.to_string()
}

fn replace_sentence_periods(text: &str) -> String {
    // ". " -> " ... ", but not inside "a.b" (no space after).
    let mut out = String::with_capacity(text.len() + 8);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '.'
            && i + 1 < chars.len()
            && chars[i + 1].is_whitespace()
        {
            out.push_str(" ... ");
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    // Drop trailing " ." / "." (final period, optional spaces).
    let trimmed = out.trim_end();
    let without_final = trimmed.strip_suffix('.').unwrap_or(trimmed);
    // Only strip if it was a sentence-final period, not "..." we inserted:
    // inserted form ends with "... " pattern; a lone trailing '.' is dropped.
    if without_final != trimmed {
        without_final.trim_end().to_string()
    } else {
        out.trim().to_string()
    }
}

/// Normalize for noise matching: lowercase, *delete* ASCII punctuation,
/// collapse spaces. Deletion (not space replacement) mirrors
/// `tr -d '[:punct:]'`, so `Amara.org` becomes `amaraorg` and still matches
/// the stock-hallucination blocklist.
pub fn normalize_noise(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| !c.is_ascii_punctuation())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// True when the transcript is empty or whole-string matches the noise
/// blocklist. `DICTATE_NOISE` is an ERE (bash `grep -qxE`), so matching goes
/// through `grep` itself; only when `grep` is unavailable do we fall back to
/// exact-alternative comparison.
pub fn is_noise(text: &str, blocklist_ere: &str) -> bool {
    let norm = normalize_noise(text);
    if norm.is_empty() {
        return true;
    }
    match grep_whole_match(&norm, blocklist_ere) {
        Some(matched) => matched,
        None => blocklist_ere.split('|').any(|p| p.trim() == norm),
    }
}

fn grep_whole_match(line: &str, ere: &str) -> Option<bool> {
    use std::io::Write;
    let mut child = std::process::Command::new("grep")
        .args(["-qxE", "--", ere])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(line.as_bytes()).ok()?;
    child.stdin.as_mut()?.write_all(b"\n").ok()?;
    drop(child.stdin.take());
    Some(child.wait().ok()?.success())
}

/// Collapse whisper whitespace/bracket cleanup done in `stop()`:
/// newlines -> space, strip `[..]` and `(..)`, trim, squeeze spaces.
pub fn clean_whisper_text(text: &str) -> String {
    let no_brackets = strip_delimited(text, '[', ']');
    let no_parens = strip_delimited(&no_brackets, '(', ')');
    no_parens
        .replace(['\n', '\t', '\r'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_delimited(s: &str, open: char, close: char) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0;
    for c in s.chars() {
        if c == open {
            depth += 1;
        } else if c == close && depth > 0 {
            depth -= 1;
        } else if depth == 0 {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_restyle_is_identity() {
        assert_eq!(restyle("Hello World.", false, ""), "Hello World.");
    }

    #[test]
    fn sentence_periods_become_ellipsis_and_final_dropped() {
        assert_eq!(
            restyle("Hello world. This is a test.", true, ""),
            "hello world ... this is a test"
        );
    }

    #[test]
    fn keeps_glossary_and_proper_forms() {
        let out = restyle("Hello SvelteKit and NixOS world. I am here.", true, "");
        assert!(out.contains("SvelteKit"), "{out}");
        assert!(out.contains("NixOS"), "{out}");
        assert!(out.contains('I'), "{out}");
    }

    #[test]
    fn version_numbers_and_domains_safe() {
        let out = restyle("See itch.io version 3.5 today.", true, "");
        assert!(out.contains("itch.io"), "{out}");
        assert!(out.contains("3.5"), "{out}");
    }

    #[test]
    fn noise_matches_whole_string_only() {
        assert!(is_noise("Thank you.", DEFAULT_NOISE_PHRASES));
        assert!(is_noise("  so  ", DEFAULT_NOISE_PHRASES));
        assert!(!is_noise("So this is a real sentence", DEFAULT_NOISE_PHRASES));
        assert!(!is_noise("Okay, here is my dictation", DEFAULT_NOISE_PHRASES));
    }

    #[test]
    fn noise_deletes_punctuation_like_tr() {
        // `tr -d '[:punct:]'` turns `Amara.org` into `amaraorg`.
        assert_eq!(normalize_noise("Subtitles by the Amara.org community!"), "subtitles by the amaraorg community");
        assert!(is_noise("Subtitles by the Amara.org community.", DEFAULT_NOISE_PHRASES));
        assert!(is_noise("Amara.org", "amaraorg"));
    }

    #[test]
    fn noise_honors_ere_when_grep_exists() {
        if !crate::doctor::lookup_in_path("grep") {
            return;
        }
        assert!(is_noise("foobar", "foo.*"));
        assert!(!is_noise("baz", "foo.*"));
        // Transcript metacharacters are literal subjects, not patterns.
        assert!(!is_noise("a+b", "a"));
    }

    #[test]
    fn whisper_cleanup_strips_brackets() {
        assert_eq!(
            clean_whisper_text("  hello [music] (laughs)\nworld  "),
            "hello world"
        );
    }
}
