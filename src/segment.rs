//! Incremental final pass (`DICTATE_INCREMENTAL=1`): the recording is cut
//! into segments inside the pauses, and each segment is recognized once,
//! while you pause. `stop` then recognizes only the audio after the last
//! cut. The segments tile the clip exactly (each starts where the previous
//! one ended), so every second of audio is sent to the recognizer once,
//! instead of every pause pass resending the whole recording.
//!
//! Correction is text-only and cheap, so it always runs on the whole text.

use crate::wav::RATE;

/// Audio the daemon reads per step: 160 ms.
pub const CHUNK: usize = RATE as usize * 160 / 1000;

/// Characters of the previous segments' text passed to whisper as context.
/// whisper keeps the last 224 prompt tokens, so this stays well inside them
/// with the glossary prompt in front.
pub const CONTEXT_CHARS: usize = 200;

/// Default `DICTATE_SEGMENT_MIN_MS` with whisper: a cut is kept once the
/// segment before it holds this much audio up to its last speech. On 90
/// saved dictations (whisper large-v3-turbo, word errors against what was
/// pasted, see `dictate segbench`), cutting at every pause cost 3.3% vs
/// 1.7% for whole clips; at 3 s 2.8%, at 6 s 2.7%. Shorter segments still
/// get a speculative pass, so pausing at the end stays instant; their audio
/// is recognized again when speech resumes (about 1.3-1.5x the audio, still
/// free with local whisper). ElevenLabs defaults to 0: every cut kept, each
/// second sent once.
pub const MIN_SEGMENT_MS: &str = "6000";

/// A sample position inside the pause `[speech_end, audio.len())`: the
/// quietest 100 ms in its later two thirds, so the cut stays clear of a
/// word's fading end and of the next word's onset.
pub fn cut_in_pause(audio: &[i16], speech_end: usize) -> usize {
    let len = audio.len();
    let step = RATE as usize / 50; // 20 ms
    let window = step * 5; // 100 ms
    let lo = speech_end + (len.saturating_sub(speech_end)) / 3;
    if len < lo + window {
        return speech_end + (len.saturating_sub(speech_end)) / 2;
    }
    (lo..=len - window)
        .step_by(step)
        .min_by_key(|&w| audio[w..w + window].iter().map(|&s| (s as i64 * s as i64) as u64).sum::<u64>())
        .map(|w| w + window / 2)
        .unwrap_or(lo)
}

/// The cuts the daemon keeps in `samples` if every pause pass finished
/// instantly (the most cuts it can make): a chunk whose peak is at or above
/// `level` dBFS is speech, and `pause` samples below it after new speech
/// make a cut, once the segment holds `min` samples up to its last speech.
/// Offline benches use this to measure accuracy at the joins.
pub fn simulate_cuts(samples: &[i16], level: f32, pause: usize, min: usize) -> Vec<usize> {
    let mut cuts = Vec::new();
    let (mut speech_end, mut cut, mut len) = (0, 0, 0);
    while len < samples.len() {
        len = (len + CHUNK).min(samples.len());
        if crate::wav::peak_db(&samples[len.saturating_sub(CHUNK)..len]) >= level {
            speech_end = len;
        } else if speech_end > cut && len - speech_end >= pause && speech_end - cut >= min {
            cut = cut_in_pause(&samples[..len], speech_end);
            cuts.push(cut);
        }
    }
    cuts
}

/// The end of `text`, at most `max` bytes, starting at a word boundary.
pub fn context_tail(text: &str, max: usize) -> &str {
    let text = text.trim();
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    if text[..start].ends_with(char::is_whitespace) {
        return &text[start..];
    }
    match text[start..].find(char::is_whitespace) {
        Some(i) => text[start + i..].trim_start(),
        None => &text[start..],
    }
}

/// Whisper's prompt for a segment: the configured prompt, then the text
/// just before the segment, which whisper reads as what preceded it.
pub fn prompt_with_context(prompt: &str, context: &str) -> String {
    let tail = context_tail(context, CONTEXT_CHARS);
    match (prompt.trim().is_empty(), tail.is_empty()) {
        (_, true) => prompt.to_string(),
        (true, false) => tail.to_string(),
        (false, false) => format!("{} {tail}", prompt.trim_end()),
    }
}

/// Segment texts joined with one space, empty ones skipped.
pub fn join(a: &str, b: &str) -> String {
    match (a.trim(), b.trim()) {
        ("", b) => b.to_string(),
        (a, "") => a.to_string(),
        (a, b) => format!("{a} {b}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(secs: f64) -> Vec<i16> {
        (0..(secs * RATE as f64) as usize).map(|i| if i % 2 == 0 { 8000 } else { -8000 }).collect()
    }

    fn quiet(secs: f64) -> Vec<i16> {
        vec![10; (secs * RATE as f64) as usize]
    }

    #[test]
    fn cuts_land_in_pauses_only() {
        let mut s = tone(2.0);
        s.extend(quiet(1.0)); // pause 2.0-3.0
        s.extend(tone(1.5));
        s.extend(quiet(0.3)); // too short to cut
        s.extend(tone(1.0));
        s.extend(quiet(1.0)); // trailing pause 5.8-6.8
        let cuts = simulate_cuts(&s, -30.0, RATE as usize / 2, 0);
        assert_eq!(simulate_cuts(&s, -30.0, RATE as usize / 2, 3 * RATE as usize).len(), 1, "2 s is too short to keep");
        assert_eq!(cuts.len(), 2, "{cuts:?}");
        let at = |c: usize| c as f64 / RATE as f64;
        assert!((2.1..3.0).contains(&at(cuts[0])), "{}", at(cuts[0]));
        assert!((5.9..6.8).contains(&at(cuts[1])), "{}", at(cuts[1]));
    }

    #[test]
    fn no_cut_without_new_speech() {
        let mut s = quiet(2.0);
        s.extend(tone(1.0));
        s.extend(quiet(3.0));
        assert_eq!(simulate_cuts(&s, -30.0, RATE as usize / 2, 0).len(), 1);
        assert!(simulate_cuts(&quiet(3.0), -30.0, RATE as usize / 2, 0).is_empty());
    }

    #[test]
    fn context_is_a_word_aligned_tail() {
        assert_eq!(context_tail("one two three", 100), "one two three");
        assert_eq!(context_tail("alpha beta gamma delta", 11), "gamma delta");
        assert_eq!(context_tail("alpha beta gamma delta", 10), "delta");
        assert_eq!(context_tail("žluťoučký kůň", 5), "kůň");
        assert_eq!(prompt_with_context("Glossary.", ""), "Glossary.");
        assert_eq!(prompt_with_context("Glossary.", "said this"), "Glossary. said this");
        assert_eq!(prompt_with_context("", "said this"), "said this");
    }

    #[test]
    fn join_skips_empty() {
        assert_eq!(join("a b", "c"), "a b c");
        assert_eq!(join("", " c "), "c");
        assert_eq!(join("a", ""), "a");
    }
}
