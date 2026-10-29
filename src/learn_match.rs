//! Conservative matching of recent dictated captures to a final draft.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

const MAX_SOURCES: usize = 20;
const MAX_DRAFT_BYTES: usize = 1_048_576;
const MAX_DRAFT_WORDS: usize = 20_000;
const MAX_SOURCE_WORDS: usize = 256;
const MAX_FUZZY_PARAGRAPH_WORDS: usize = 512;
const ANCHOR: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    Unchanged,
    Formatting,
    Lexical,
    Review,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WordEdit {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureMatch {
    pub source_ids: Vec<String>,
    pub original: String,
    pub edited: String,
    pub kind: MatchKind,
    pub edits: Vec<WordEdit>,
    pub reason: String,
}

/// Match captures to spans in a draft. Sources must be ordered oldest first.
/// Time and application filtering are caller responsibilities; this function
/// considers at most the newest twenty sources.
pub fn match_draft(sources: &[Source], draft: &str) -> Vec<CaptureMatch> {
    let sources = &sources[sources.len().saturating_sub(MAX_SOURCES)..];
    if sources.is_empty() {
        return Vec::new();
    }
    if draft.len() > MAX_DRAFT_BYTES {
        return vec![review_all(
            sources,
            draft,
            "Draft exceeds the matching size limit; review it manually.",
        )];
    }
    let Some(draft_words) = tokenize(draft, MAX_DRAFT_WORDS) else {
        return vec![review_all(
            sources,
            draft,
            "Draft exceeds the matching word limit; review it manually.",
        )];
    };
    let source_words: Vec<Option<Vec<Word>>> = sources
        .iter()
        .map(|s| tokenize(&s.text, MAX_SOURCE_WORDS))
        .collect();
    let draft_anchors = unique_anchors(&draft_words);
    let candidate_sets: Vec<Vec<Candidate>> = source_words
        .iter()
        .map(|words| match words {
            Some(words) => find_candidates(words, &draft_words, &draft_anchors),
            None => Vec::new(),
        })
        .collect();
    let best: Vec<Vec<Candidate>> = candidate_sets.iter().map(|c| best_candidates(c)).collect();

    let mut source_counts = HashMap::<Vec<String>, usize>::new();
    for words in source_words.iter().flatten().filter(|w| !w.is_empty()) {
        *source_counts.entry(keys(words)).or_default() += 1;
    }
    let mut found = Vec::<BuiltMatch>::new();

    for (i, source) in sources.iter().enumerate() {
        if best[i].is_empty() {
            continue;
        }
        let Some(source_words) = source_words[i].as_ref() else {
            continue;
        };
        let c = &best[i][0];
        let edited = span_text(c, &draft_words, draft);
        let capture = classify(
            source,
            source_words,
            &draft_words[c.start..c.end],
            edited,
            best[i].len() > 1
                || source_counts.get(&keys(source_words)).copied().unwrap_or(0) > 1
                || joined_word_boundary(c, &draft_words, draft),
        );
        found.push(BuiltMatch {
            source_index: i,
            start: c.start,
            end: c.end,
            capture,
        });
    }

    // A paragraph shared by adjacent captures, overlapping spans, or reordering
    // across the capture sequence makes individual attribution unsafe.
    for left in 0..found.len() {
        if found[left].capture.source_ids.len() != 1 {
            continue;
        }
        for right in left + 1..found.len() {
            if found[right].capture.source_ids.len() != 1
                || found[left].source_index >= found[right].source_index
            {
                continue;
            }
            let same_para = draft_words[found[left].start].paragraph
                == draft_words[found[left].end - 1].paragraph
                && draft_words[found[right].start].paragraph
                    == draft_words[found[right].end - 1].paragraph
                && draft_words[found[left].start].paragraph
                    == draft_words[found[right].start].paragraph;
            if same_para
                || found[left].start > found[right].start
                || found[left].end > found[right].start
            {
                for index in [left, right] {
                    found[index].capture.kind = MatchKind::Review;
                    found[index].capture.edits.clear();
                    found[index].capture.reason = "Matched spans share a paragraph, overlap, or cross capture order; review attribution.".into();
                }
            }
        }
    }

    if found.is_empty() {
        return vec![review_all(
            sources,
            draft,
            "No uniquely anchored source span was found; review the draft and candidate captures.",
        )];
    }
    found.sort_by_key(|m| m.source_index);
    found.into_iter().map(|m| m.capture).collect()
}

#[derive(Debug, Clone)]
struct Word {
    key: String,
    raw: String,
    start: usize,
    end: usize,
    paragraph: usize,
}

fn tokenize(text: &str, limit: usize) -> Option<Vec<Word>> {
    let mut result = Vec::new();
    let mut start = None;
    let mut paragraph = 0;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            start.get_or_insert(i);
        } else if let Some(begin) = start.take() {
            result.push(Word {
                key: text[begin..i].to_lowercase(),
                raw: text[begin..i].into(),
                start: begin,
                end: i,
                paragraph,
            });
            if result.len() > limit {
                return None;
            }
        }
        if c == '\n' {
            paragraph += 1;
        }
    }
    if let Some(begin) = start {
        result.push(Word {
            key: text[begin..].to_lowercase(),
            raw: text[begin..].into(),
            start: begin,
            end: text.len(),
            paragraph,
        });
    }
    (result.len() <= limit).then_some(result)
}

#[derive(Debug, Clone)]
struct Candidate {
    start: usize,
    end: usize,
    score: isize,
}

fn keys(words: &[Word]) -> Vec<String> {
    words.iter().map(|w| w.key.clone()).collect()
}

fn exact_candidates(source: &[Word], draft: &[Word]) -> Vec<Candidate> {
    let pattern = keys(source);
    if pattern.is_empty() {
        return Vec::new();
    }
    let mut failure = vec![0; pattern.len()];
    for i in 1..pattern.len() {
        let mut j = failure[i - 1];
        while j > 0 && pattern[i] != pattern[j] {
            j = failure[j - 1];
        }
        if pattern[i] == pattern[j] {
            j += 1;
        }
        failure[i] = j;
    }
    let mut result = Vec::new();
    let mut j = 0;
    for (i, word) in draft.iter().enumerate() {
        while j > 0 && word.key != pattern[j] {
            j = failure[j - 1];
        }
        if word.key == pattern[j] {
            j += 1;
        }
        if j == pattern.len() {
            result.push(Candidate {
                start: i + 1 - j,
                end: i + 1,
                score: (pattern.len() * 100) as isize,
            });
            if result.len() == 8 {
                break;
            }
            j = failure[j - 1];
        }
    }
    result
}

fn anchor_hash(words: &[Word], start: usize) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for word in &words[start..start + ANCHOR] {
        word.key.hash(&mut h);
    }
    h.finish()
}

fn unique_anchors(words: &[Word]) -> HashMap<u64, usize> {
    let mut found = HashMap::<u64, Option<usize>>::new();
    for i in 0..words.len().saturating_sub(ANCHOR - 1) {
        if words[i].paragraph != words[i + ANCHOR - 1].paragraph {
            continue;
        }
        let hash = anchor_hash(words, i);
        found
            .entry(hash)
            .and_modify(|place| *place = None)
            .or_insert(Some(i));
    }
    found
        .into_iter()
        .filter_map(|(hash, place)| place.map(|i| (hash, i)))
        .collect()
}

fn find_candidates(
    source: &[Word],
    draft: &[Word],
    draft_anchors: &HashMap<u64, usize>,
) -> Vec<Candidate> {
    if source.is_empty() || source.len() > MAX_SOURCE_WORDS {
        return Vec::new();
    }
    let exact = exact_candidates(source, draft);
    if !exact.is_empty() {
        return exact;
    }
    if source.len() < ANCHOR {
        return Vec::new();
    }

    let source_anchors = unique_anchors(source);
    let mut paragraph_scores = HashMap::<usize, usize>::new();
    for hash in source_anchors.keys() {
        if let Some(&draft_at) = draft_anchors.get(hash) {
            *paragraph_scores
                .entry(draft[draft_at].paragraph)
                .or_default() += 1;
        }
    }
    let mut paragraphs: Vec<_> = paragraph_scores.into_iter().collect();
    paragraphs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut result = Vec::new();
    for (paragraph, _) in paragraphs.into_iter().take(2) {
        let Some(start) = draft.iter().position(|w| w.paragraph == paragraph) else {
            continue;
        };
        let Some(last) = draft.iter().rposition(|w| w.paragraph == paragraph) else {
            continue;
        };
        let end = last + 1;
        let target = &draft[start..end];
        if target.len() > MAX_FUZZY_PARAGRAPH_WORDS {
            continue;
        }
        let alignment = align(source, target);
        let largest = source.len().max(target.len());
        if alignment.matched < ANCHOR || alignment.matched * 100 < largest * 40 {
            continue;
        }
        let distance = source.len() + target.len() - 2 * alignment.matched;
        let score = (alignment.matched * 100) as isize
            - (distance * 10) as isize
            - source.len().abs_diff(target.len()) as isize * 5;
        result.push(Candidate { start, end, score });
    }
    result
}

fn best_candidates(candidates: &[Candidate]) -> Vec<Candidate> {
    let Some(score) = candidates.iter().map(|c| c.score).max() else {
        return Vec::new();
    };
    let mut best: Vec<_> = candidates
        .iter()
        .filter(|c| c.score == score)
        .cloned()
        .collect();
    best.sort_by_key(|c| (c.start, c.end));
    best.dedup_by_key(|c| (c.start, c.end));
    best
}

struct Alignment {
    matched: usize,
    edits: Vec<(usize, usize)>,
    gap: bool,
}

fn align(a: &[Word], b: &[Word]) -> Alignment {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in dp.iter_mut().enumerate().skip(1) {
        row[0] = i;
    }
    for (j, cell) in dp[0].iter_mut().enumerate().skip(1) {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let sub = dp[i - 1][j - 1] + if a[i - 1].key == b[j - 1].key { 0 } else { 2 };
            dp[i][j] = sub.min((dp[i - 1][j] + 1).min(dp[i][j - 1] + 1));
        }
    }
    let (mut i, mut j) = (n, m);
    let (mut matched, mut gap) = (0, false);
    let mut edits = Vec::new();
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1].key == b[j - 1].key && dp[i][j] == dp[i - 1][j - 1] {
            matched += 1;
            i -= 1;
            j -= 1;
        } else if i > 0 && j > 0 && dp[i][j] == dp[i - 1][j - 1] + 2 {
            edits.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if i > 0 && dp[i][j] == dp[i - 1][j] + 1 {
            gap = true;
            i -= 1;
        } else {
            gap = true;
            j -= 1;
        }
    }
    edits.reverse();
    Alignment {
        matched,
        edits,
        gap,
    }
}

fn classify(
    source: &Source,
    a: &[Word],
    b: &[Word],
    edited: String,
    ambiguous: bool,
) -> CaptureMatch {
    let original = source.text.clone();
    let ids = vec![source.id.clone()];
    if ambiguous {
        return review(
            ids,
            original,
            edited,
            "The source has multiple equally strong spans in the draft.",
        );
    }
    let source_breaks = source.text.trim().matches('\n').count();
    let target_breaks = b.last().unwrap().paragraph - b.first().unwrap().paragraph;
    if source_breaks < target_breaks {
        return review(
            ids,
            original,
            edited,
            "The matched span crosses paragraph boundaries; review the split or merge.",
        );
    }
    if source.text.trim() == edited.trim() {
        return CaptureMatch {
            source_ids: ids,
            original,
            edited,
            kind: MatchKind::Unchanged,
            edits: Vec::new(),
            reason: "The dictated text matches the draft span exactly.".into(),
        };
    }
    let alignment = align(a, b);
    if alignment.matched == a.len() && alignment.matched == b.len() {
        return CaptureMatch {
            source_ids: ids,
            original,
            edited,
            kind: MatchKind::Formatting,
            edits: Vec::new(),
            reason: "Only capitalization, punctuation, backticks, or whitespace differs.".into(),
        };
    }
    let localized = !alignment.gap
        && !alignment.edits.is_empty()
        && alignment.edits.len() <= 2
        && alignment.edits.last().unwrap().0 - alignment.edits[0].0 + 1 == alignment.edits.len()
        && alignment.edits[0].0 >= 2
        && a.len()
            .saturating_sub(alignment.edits.last().unwrap().0 + 1)
            >= 2
        && alignment.matched >= ANCHOR
        && alignment.matched * 100 >= a.len().max(b.len()) * 80;
    if !localized {
        return review(
            ids,
            original,
            edited,
            "The span contains a rewrite, insertion, deletion, or weakly anchored change.",
        );
    }
    let edits = alignment
        .edits
        .iter()
        .map(|&(x, y)| WordEdit {
            from: a[x].raw.clone(),
            to: b[y].raw.clone(),
        })
        .collect();
    CaptureMatch {
        source_ids: ids,
        original,
        edited,
        kind: MatchKind::Lexical,
        edits,
        reason: "A small, localized replacement is anchored by unchanged context.".into(),
    }
}

fn joined_word_boundary(c: &Candidate, words: &[Word], draft: &str) -> bool {
    let before = &draft[..words[c.start].start];
    let left = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace() || c.is_alphanumeric())
        .is_some_and(|(_, c)| c.is_alphanumeric());
    let after = &draft[words[c.end - 1].end..];
    let right = after
        .char_indices()
        .find(|(_, c)| c.is_whitespace() || c.is_alphanumeric())
        .is_some_and(|(_, c)| c.is_alphanumeric());
    left || right
}

fn span_text(c: &Candidate, words: &[Word], draft: &str) -> String {
    let mut start = words[c.start].start;
    let mut end = words[c.end - 1].end;
    let prefix = &draft[..start];
    let run: Vec<_> = prefix
        .char_indices()
        .rev()
        .take_while(|(_, c)| !c.is_alphanumeric() && !c.is_whitespace())
        .collect();
    if let Some(&(at, _)) = run.last() {
        if prefix[..at].chars().last().is_none_or(char::is_whitespace) {
            start = at;
        }
    }
    let suffix = &draft[end..];
    let run: Vec<_> = suffix
        .char_indices()
        .take_while(|(_, c)| !c.is_alphanumeric() && !c.is_whitespace())
        .collect();
    if let Some(&(at, ch)) = run.last() {
        let after = at + ch.len_utf8();
        if suffix[after..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric())
        {
            end += after;
        }
    }
    draft[start..end].trim().into()
}

fn review(ids: Vec<String>, original: String, edited: String, reason: &str) -> CaptureMatch {
    CaptureMatch {
        source_ids: ids,
        original,
        edited,
        kind: MatchKind::Review,
        edits: Vec::new(),
        reason: reason.into(),
    }
}

fn review_all(sources: &[Source], draft: &str, reason: &str) -> CaptureMatch {
    review(
        sources.iter().map(|s| s.id.clone()).collect(),
        sources
            .iter()
            .map(|s| s.text.trim())
            .collect::<Vec<_>>()
            .join("\n"),
        draft.to_string(),
        reason,
    )
}

struct BuiltMatch {
    source_index: usize,
    start: usize,
    end: usize,
    capture: CaptureMatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(id: &str, text: &str) -> Source {
        Source {
            id: id.into(),
            text: text.into(),
        }
    }

    #[test]
    fn localized_replacement_has_five_anchors() {
        let result = match_draft(
            &[src(
                "a",
                "The quick brown fox jumps over the lazy dog beside the old oak tree today.",
            )],
            "The quick brown fox jumps over the lazy dog near the old oak tree today.",
        );
        assert_eq!(result[0].kind, MatchKind::Lexical);
        assert_eq!(
            result[0].edits,
            vec![WordEdit {
                from: "beside".into(),
                to: "near".into()
            }]
        );
    }

    #[test]
    fn calling_to_culling_is_a_localized_edit() {
        let result = match_draft(
            &[src("a", "The caller was calling the local compiler before the next build finished without any errors yesterday.")],
            "The caller was culling the local compiler before the next build finished without any errors yesterday.",
        );
        assert_eq!(result[0].kind, MatchKind::Lexical);
        assert_eq!(
            result[0].edits,
            vec![WordEdit {
                from: "calling".into(),
                to: "culling".into()
            }]
        );
    }

    #[test]
    fn punctuation_case_and_backticks_are_formatting() {
        let result = match_draft(
            &[src("a", "Please run cargo test.")],
            "please run `cargo test`!",
        );
        assert_eq!(result[0].kind, MatchKind::Formatting);
        assert!(result[0].edits.is_empty());
    }

    #[test]
    fn two_captures_match_paragraphs_around_typed_intro() {
        let result = match_draft(&[
            src("a", "Please review the deployment plan before we send it to the team."),
            src("b", "Also update the release notes with the final version number."),
        ], "I added a note first.\nPlease review the deployment plan before we send it to the team.\nAlso update the release notes with the final version number.");
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|m| m.kind == MatchKind::Unchanged));
        assert!(!result[0].edited.contains("I added"));
    }

    #[test]
    fn unrelated_draft_becomes_one_review_with_all_candidates() {
        let result = match_draft(
            &[src(
                "a",
                "Please review the deployment plan before we send it to the team.",
            )],
            "The weather is clear and the train leaves at noon.",
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].kind, MatchKind::Review);
        assert_eq!(result[0].source_ids, vec!["a"]);
        assert!(result[0].edits.is_empty());
    }

    #[test]
    fn repeated_source_identity_is_reviewed() {
        let text = "Please review the deployment plan before we send it to the team.";
        let result = match_draft(&[src("a", text), src("b", text)], text);
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|m| m.kind == MatchKind::Review));
    }

    #[test]
    fn rewrites_additions_and_deletions_are_reviewed() {
        let rewrite = match_draft(&[src("a", "We need to carefully review this proposal before the team meeting tomorrow morning.")],
            "We need to radically redesign the entire proposal before the team meeting tomorrow morning.");
        assert_eq!(rewrite[0].kind, MatchKind::Review);
        let added = match_draft(&[src("a", "Please send the updated report to the team before our meeting tomorrow morning.")],
            "Please send the updated and carefully corrected report to the team before our meeting tomorrow morning.");
        assert_eq!(added[0].kind, MatchKind::Review);
        let deleted = match_draft(&[src("a", "Please send the final updated report to the team before our meeting tomorrow morning.")],
            "Please send the updated report to the team before our meeting tomorrow morning.");
        assert_eq!(deleted[0].kind, MatchKind::Review);
    }

    #[test]
    fn merged_captures_and_paragraph_split_are_reviewed() {
        let merged = match_draft(
            &[
                src("a", "Please update the draft"),
                src("b", "and send it tomorrow morning"),
            ],
            "Please update the draft and send it tomorrow morning.",
        );
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().all(|m| m.kind == MatchKind::Review));
        let split = match_draft(
            &[src(
                "c",
                "We should carefully revise this paragraph before submitting it tomorrow.",
            )],
            "We should carefully revise this paragraph\nbefore submitting it tomorrow.",
        );
        assert_eq!(split[0].kind, MatchKind::Review);
    }

    #[test]
    fn only_newest_twenty_source_ids_are_candidates() {
        let mut sources = vec![src(
            "old",
            "Please review the deployment plan before we send it to the team.",
        )];
        for i in 0..20 {
            sources.push(src(
                &format!("new-{i}"),
                "An unrelated newer dictated sentence.",
            ));
        }
        let result = match_draft(
            &sources,
            "Unrelated typed words without a matching dictated span.",
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source_ids.len(), 20);
        assert!(!result[0].source_ids.iter().any(|id| id == "old"));
    }
}
