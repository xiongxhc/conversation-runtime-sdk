//! Conservative cleanup of a recognized final transcript before it can start a turn.
//!
//! The rules and phrase lists are adapted from `whisper-guard` in silverstein/minutes
//! (MIT, see crates/runtime/THIRD_PARTY_NOTICES.md). They run on sentence units within one final
//! instead of transcript lines.

use std::sync::{Mutex, PoisonError};

/// Content-free counts of what cleanup removed from finals.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TranscriptCleanupRemovals {
    pub known_hallucinations: u64,
    pub repeated_units: u64,
    pub interleaved_loop_units: u64,
    pub trailing_noise_units: u64,
    pub noise_markers: u64,
}

/// Content-free totals across the finals a voice runtime has cleaned.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TranscriptCleanupStats {
    pub finals_cleaned: u64,
    pub finals_dropped: u64,
    pub removals: TranscriptCleanupRemovals,
}

#[derive(Debug, Default)]
pub(crate) struct TranscriptCleanupCounters(Mutex<TranscriptCleanupStats>);

impl TranscriptCleanupCounters {
    pub(crate) fn record(&self, cleanup: &TranscriptCleanup) {
        let (removals, dropped) = match cleanup {
            TranscriptCleanup::Unchanged => return,
            TranscriptCleanup::Cleaned { removals, .. } => (removals, false),
            TranscriptCleanup::Dropped { removals } => (removals, true),
        };
        let mut stats = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if dropped {
            stats.finals_dropped += 1;
        } else {
            stats.finals_cleaned += 1;
        }
        let total = &mut stats.removals;
        total.known_hallucinations += removals.known_hallucinations;
        total.repeated_units += removals.repeated_units;
        total.interleaved_loop_units += removals.interleaved_loop_units;
        total.trailing_noise_units += removals.trailing_noise_units;
        total.noise_markers += removals.noise_markers;
    }

    pub(crate) fn snapshot(&self) -> TranscriptCleanupStats {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranscriptCleanup {
    Unchanged,
    Cleaned {
        text: String,
        removals: TranscriptCleanupRemovals,
    },
    Dropped {
        removals: TranscriptCleanupRemovals,
    },
}

/// Non-speech tokens whisper emits as `[music]` / `(crying)` style annotations.
const NOISE_WORDS: &[&str] = &[
    "crying",
    "laughter",
    "laughing",
    "applause",
    "growling",
    "music",
    "sobbing",
    "cheering",
    "sighing",
    "clapping",
    "coughing",
    "sneezing",
    "gasping",
    "whispering",
    "mumbling",
    "humming",
    "breathing",
    "silence",
    "snoring",
    "yelling",
    "screaming",
    "blank_audio",
    "inaudible",
    "noise",
    "crosstalk",
    "typing",
    "static",
    "beep",
    "ringing",
    "śmiech",
    "risas",
    "musik",
    "musique",
    "musica",
    "música",
    "muzyka",
    "applaus",
    "aplausos",
    "applausi",
    "oklaski",
    "ruido",
    "geräusch",
    "stille",
    "silencio",
    "cisza",
    "rires",
    "rire",
    "gelächter",
    "weeping",
];

/// Subtitle-credit leaks matched against a whole unit. Upstream also lists
/// "see you next time", which a speaker can say to an assistant, so it is omitted.
const KNOWN_HALLUCINATION_PHRASES: &[&str] = &[
    "thank you for watching",
    "thanks for watching",
    "thank you so much for watching",
    "please subscribe to our channel",
    "please subscribe",
    "please like and subscribe",
    "like and subscribe",
    "smash that like button",
    "don't forget to subscribe",
    "see you in the next video",
    "subtitles by the amara.org community",
    "transcribed by the amara.org community",
    "translated by the amara.org community",
    "the amara.org community",
    "amara.org community",
    "captions by the cyclope",
];

/// Credit prefixes matched at the start of a unit. Upstream's "transcribed by" and
/// "translated by" also begin real questions, so they are omitted.
const HALLUCINATION_UNIT_PREFIXES: &[&str] = &[
    "transcripted by",
    "captions by",
    "captioned by",
    "subtitles by",
];

const MIN_REPEAT_RUN: usize = 3;
const LOOP_WINDOW: usize = 10;
const LOOP_MIN_OCCURRENCES: usize = 5;
const LOOP_MIN_REGION: usize = 6;
const MIN_TRAILING_FILLER_RUN: usize = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Rule {
    KnownHallucination,
    Repeated,
    InterleavedLoop,
    TrailingNoise,
    NoiseMarker,
}

struct Unit<'a> {
    raw: &'a str,
    noise_marker: bool,
    key: Vec<String>,
    removed: Option<Rule>,
}

impl Unit<'_> {
    fn content(&self) -> &str {
        self.raw.trim()
    }
}

pub(crate) fn clean_final_transcript(text: &str) -> TranscriptCleanup {
    let mut units: Vec<Unit<'_>> = split_units(text)
        .into_iter()
        .map(|raw| Unit {
            raw,
            noise_marker: is_noise_marker(raw.trim()),
            key: repetition_key(raw),
            removed: None,
        })
        .collect();

    strip_known_hallucinations(&mut units);
    collapse_repeated_units(&mut units);
    collapse_interleaved_loops(&mut units);
    trim_trailing_noise(&mut units);
    strip_noise_markers(&mut units);

    let mut removals = TranscriptCleanupRemovals::default();
    for rule in units.iter().filter_map(|unit| unit.removed) {
        let count = match rule {
            Rule::KnownHallucination => &mut removals.known_hallucinations,
            Rule::Repeated => &mut removals.repeated_units,
            Rule::InterleavedLoop => &mut removals.interleaved_loop_units,
            Rule::TrailingNoise => &mut removals.trailing_noise_units,
            Rule::NoiseMarker => &mut removals.noise_markers,
        };
        *count += 1;
    }
    if removals == TranscriptCleanupRemovals::default() {
        return TranscriptCleanup::Unchanged;
    }

    let kept: String = units
        .iter()
        .filter(|unit| unit.removed.is_none())
        .map(|unit| unit.raw)
        .collect();
    let kept = kept.trim();
    if kept.chars().any(char::is_alphanumeric) {
        TranscriptCleanup::Cleaned {
            text: kept.to_owned(),
            removals,
        }
    } else {
        TranscriptCleanup::Dropped { removals }
    }
}

/// Splits text into contiguous slices: sentences ending in terminal punctuation,
/// and bracketed noise markers on their own. Concatenating the slices gives the text.
fn split_units(text: &str) -> Vec<&str> {
    let mut units = Vec::new();
    let mut start = 0;
    let mut cut_at_next_content = false;
    let mut chars = text.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        let marker_end = noise_marker_end(text, index);
        // Every cut lands on a non-whitespace character, so only leading whitespace
        // could form an empty unit; it stays with the unit that follows it.
        if ((cut_at_next_content && !character.is_whitespace()) || marker_end.is_some())
            && !text[start..index].trim().is_empty()
        {
            units.push(&text[start..index]);
            start = index;
        }
        if !character.is_whitespace() {
            cut_at_next_content = false;
        }
        if let Some(end) = marker_end {
            while chars.next_if(|(next, _)| *next < end).is_some() {}
            cut_at_next_content = true;
            continue;
        }
        cut_at_next_content |= match character {
            '。' | '！' | '？' => true,
            '.' | '!' | '?' | '…' => chars.peek().is_none_or(|(_, next)| next.is_whitespace()),
            _ => false,
        };
    }
    if start < text.len() {
        units.push(&text[start..]);
    }
    units
}

/// The byte index just past a noise marker starting at `start`, if one starts there.
fn noise_marker_end(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start..];
    let close = match rest.chars().next()? {
        '[' => ']',
        '(' => ')',
        _ => return None,
    };
    let close_at = rest.find(close)?;
    let marker = &rest[..=close_at];
    if !is_noise_marker(marker) {
        return None;
    }
    let mut end = start + close_at + 1;
    if text[end..].starts_with('.') {
        end += 1;
    }
    Some(end)
}

/// A short bracketed or parenthetical marker whose last word is a known noise token.
fn is_noise_marker(text: &str) -> bool {
    let text = text.strip_suffix('.').unwrap_or(text);
    let bracketed = (text.starts_with('[') && text.ends_with(']'))
        || (text.starts_with('(') && text.ends_with(')'));
    if !bracketed || text.len() < 2 {
        return false;
    }
    let inner = &text[1..text.len() - 1];
    if inner.chars().all(|c| c.is_ascii_digit() || c == ':') {
        return false;
    }
    let words = inner.split_whitespace().count();
    (1..=4).contains(&words)
        && inner.len() <= 40
        && inner
            .split_whitespace()
            .next_back()
            .is_some_and(|word| NOISE_WORDS.contains(&word.to_lowercase().as_str()))
}

fn is_always_noise(text: &str) -> bool {
    matches!(
        text.trim().to_lowercase().as_str(),
        "[music]" | "[blank_audio]" | "[silence]" | "music"
    )
}

/// The same ordered words, ignoring case and nonnumeric edge punctuation. Numeric
/// tokens stay exact so "turn it to 3" and "turn it to 4" never match.
fn repetition_key(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|word| {
            let word = word.to_lowercase();
            if word.chars().any(char::is_numeric) {
                word
            } else {
                word.trim_matches(|c: char| !c.is_alphanumeric()).to_owned()
            }
        })
        .filter(|word| !word.is_empty())
        .collect()
}

fn alive<'u, 'a>(units: &'u mut [Unit<'a>]) -> Vec<&'u mut Unit<'a>> {
    units
        .iter_mut()
        .filter(|unit| unit.removed.is_none())
        .collect()
}

fn strip_known_hallucinations(units: &mut [Unit<'_>]) {
    for unit in alive(units) {
        if unit.noise_marker {
            continue;
        }
        let lowered = unit.content().to_lowercase().replace('\u{2019}', "'");
        let normalized = lowered
            .trim_end_matches(['.', '!', '?', ',', ';', ':'])
            .trim();
        if !normalized.is_empty()
            && (KNOWN_HALLUCINATION_PHRASES.contains(&normalized)
                || HALLUCINATION_UNIT_PREFIXES
                    .iter()
                    .any(|prefix| normalized.starts_with(prefix)))
        {
            unit.removed = Some(Rule::KnownHallucination);
        }
    }
}

fn collapse_repeated_units(units: &mut [Unit<'_>]) {
    let mut units = alive(units);
    let mut index = 0;
    while index < units.len() {
        if is_always_noise(units[index].content()) || units[index].key.is_empty() {
            index += 1;
            continue;
        }
        let mut run_end = index + 1;
        while run_end < units.len() && units[run_end].key == units[index].key {
            run_end += 1;
        }
        if run_end - index >= MIN_REPEAT_RUN {
            for unit in &mut units[index + 1..run_end] {
                unit.removed = Some(Rule::Repeated);
            }
        }
        index = run_end;
    }
}

/// A/B/A/B loops: one phrase dominating a window, with filler between repetitions.
fn collapse_interleaved_loops(units: &mut [Unit<'_>]) {
    fn is_filler(key: &str) -> bool {
        matches!(
            key,
            "okay"
                | "ok"
                | "yeah"
                | "yes"
                | "right"
                | "so"
                | "and"
                | "but"
                | "well"
                | "uh"
                | "um"
                | "hmm"
                | "mhm"
        )
    }

    let mut units = alive(units);
    if units.len() < LOOP_MIN_REGION {
        return;
    }
    let texts: Vec<String> = units.iter().map(|unit| unit.key.join(" ")).collect();
    let fillers: Vec<bool> = texts.iter().map(|text| is_filler(text)).collect();
    let mut remove = vec![false; units.len()];

    let mut start = 0;
    while start + LOOP_WINDOW <= units.len() {
        let mut frequency: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for index in start..start + LOOP_WINDOW {
            if !fillers[index] && !texts[index].is_empty() {
                *frequency.entry(&texts[index]).or_default() += 1;
            }
        }
        let dominant = frequency
            .iter()
            .max_by(|(phrase_a, count_a), (phrase_b, count_b)| {
                count_a.cmp(count_b).then_with(|| phrase_a.cmp(phrase_b))
            })
            .filter(|(_, count)| **count >= LOOP_MIN_OCCURRENCES)
            .map(|(phrase, _)| (*phrase).to_owned());

        if let Some(phrase) = dominant {
            let mut region_end = start + LOOP_WINDOW;
            while region_end < units.len() {
                if texts[region_end] == phrase || fillers[region_end] {
                    region_end += 1;
                    continue;
                }
                let resume = texts[region_end..units.len().min(region_end + 3)]
                    .iter()
                    .position(|text| *text == phrase);
                match resume {
                    Some(gap) if gap <= 2 => region_end += gap + 1,
                    _ => break,
                }
            }
            let occurrences = (start..region_end)
                .filter(|&index| texts[index] == phrase)
                .count();
            if occurrences >= LOOP_MIN_OCCURRENCES && region_end - start >= LOOP_MIN_REGION {
                let mut kept_first = false;
                for index in start..region_end {
                    if !kept_first && texts[index] == phrase {
                        kept_first = true;
                    } else if texts[index] == phrase || fillers[index] {
                        remove[index] = true;
                    }
                }
                start = region_end;
                continue;
            }
        }
        start += 1;
    }

    for (unit, remove) in units.iter_mut().zip(remove) {
        if remove {
            unit.removed = Some(Rule::InterleavedLoop);
        }
    }
}

/// Always-noise at the end at any count, or a run of five or more one-word fillers.
fn trim_trailing_noise(units: &mut [Unit<'_>]) {
    fn is_filler(text: &str) -> bool {
        matches!(text.to_lowercase().as_str(), "you" | "okay." | "yeah.")
    }

    let mut units = alive(units);
    let noise_from = units.len()
        - units
            .iter()
            .rev()
            .take_while(|unit| is_always_noise(unit.content()))
            .count();
    let filler_from = noise_from
        - units[..noise_from]
            .iter()
            .rev()
            .take_while(|unit| is_filler(unit.content()))
            .count();
    let trim_from = if noise_from - filler_from >= MIN_TRAILING_FILLER_RUN {
        filler_from
    } else {
        noise_from
    };
    for unit in &mut units[trim_from..] {
        unit.removed = Some(Rule::TrailingNoise);
    }
}

/// The cleaned text is model input rather than a meeting record, so any surviving
/// marker is removed instead of being collapsed behind an annotation.
fn strip_noise_markers(units: &mut [Unit<'_>]) {
    for unit in alive(units) {
        if unit.noise_marker {
            unit.removed = Some(Rule::NoiseMarker);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cleaned(text: &str) -> Option<String> {
        match clean_final_transcript(text) {
            TranscriptCleanup::Unchanged => Some(text.to_owned()),
            TranscriptCleanup::Cleaned { text, .. } => Some(text),
            TranscriptCleanup::Dropped { .. } => None,
        }
    }

    fn removals(text: &str) -> TranscriptCleanupRemovals {
        match clean_final_transcript(text) {
            TranscriptCleanup::Unchanged => TranscriptCleanupRemovals::default(),
            TranscriptCleanup::Cleaned { removals, .. }
            | TranscriptCleanup::Dropped { removals } => removals,
        }
    }

    fn assert_unchanged(text: &str) {
        assert_eq!(
            clean_final_transcript(text),
            TranscriptCleanup::Unchanged,
            "{text:?} must survive untouched"
        );
    }

    #[test]
    fn ordinary_speech_is_returned_verbatim() {
        for text in [
            " first half second half.",
            "What is the weather in Abu Dhabi tomorrow?",
            "今天天气怎么样？",
            "",
        ] {
            assert_unchanged(text);
        }
    }

    #[test]
    fn three_identical_units_collapse_to_the_first() {
        assert_eq!(
            cleaned("I am going to go. I am going to go. i am going to go! I am going to go."),
            Some("I am going to go.".to_owned())
        );
        assert_eq!(
            removals("Okay then. Okay then. Okay then.").repeated_units,
            2
        );
        assert_eq!(cleaned("谢谢。谢谢。谢谢。"), Some("谢谢。".to_owned()));
    }

    #[test]
    fn genuine_short_repetition_survives() {
        for text in [
            "yes yes",
            "Yes yes yes, that is right.",
            "Yes. Yes.",
            "No, no, no, not that one.",
            "Turn it to 3. Turn it to 4. Turn it to 5.",
            "I said no. I said no. I did not say yes.",
        ] {
            assert_unchanged(text);
        }
    }

    #[test]
    fn alternating_loop_keeps_the_first_phrase_and_other_statements() {
        let text = "Let me pick his brain. Okay. Let me pick his brain. Okay. \
                    Let me pick his brain. Right. Let me pick his brain. Okay. \
                    Let me pick his brain. Okay.";
        assert_eq!(cleaned(text), Some("Let me pick his brain.".to_owned()));
        assert!(removals(text).interleaved_loop_units >= 8);

        let with_statement = "Ask her. Okay. Ask her. Okay. Ask her. The meeting is at noon. \
                              Ask her. Okay. Ask her. Okay.";
        assert_eq!(
            cleaned(with_statement),
            Some("Ask her. The meeting is at noon.".to_owned())
        );
    }

    #[test]
    fn a_short_alternation_survives() {
        assert_unchanged("Red. Blue. Red. Blue.");
        assert_unchanged("Left. Okay. Left. Okay. Left. Okay.");
    }

    #[test]
    fn noise_markers_are_removed_anywhere() {
        assert_eq!(cleaned("[BLANK_AUDIO]"), None);
        assert_eq!(cleaned(" [Music] (soft music) [laughter]."), None);
        assert_eq!(
            cleaned("Hello (laughter) how are you?"),
            Some("Hello how are you?".to_owned())
        );
        assert_eq!(removals("[music] hi [applause]").noise_markers, 2);
        assert_eq!(
            cleaned("[Śmiech] Dzień dobry."),
            Some("Dzień dobry.".to_owned())
        );
    }

    #[test]
    fn user_parentheticals_are_not_noise() {
        for text in [
            "Open the file (see attached) please.",
            "Play part two (2 of 3).",
            "Who is the (music director) here?",
            "Meet at [10:30] tomorrow.",
            "(applause sounds great) to me.",
        ] {
            assert_unchanged(text);
        }
    }

    #[test]
    fn known_hallucination_phrases_are_removed_as_whole_units() {
        assert_eq!(cleaned("Thanks for watching!"), None);
        assert_eq!(cleaned("Thank you for watching."), None);
        assert_eq!(
            cleaned("Subtitles by the Amara.org community"),
            None,
            "credit lines are a known whisper leak"
        );
        assert_eq!(cleaned("Captions by www.example.com"), None);
        assert_eq!(cleaned("Don\u{2019}t forget to subscribe."), None);
        assert_eq!(
            cleaned("Set a timer. Thank you for watching."),
            Some("Set a timer.".to_owned())
        );
        assert_eq!(removals("Please subscribe.").known_hallucinations, 1);
    }

    #[test]
    fn real_sentences_containing_thank_you_survive() {
        for text in [
            "Thank you.",
            "Thank you for the help.",
            "Thank you for watching the demo carefully.",
            "Thanks, see you next time.",
            "See you next time.",
            "Translated by whom?",
            "It was transcribed by a nurse.",
        ] {
            assert_unchanged(text);
        }
    }

    #[test]
    fn trailing_always_noise_and_long_filler_runs_are_trimmed() {
        assert_eq!(
            cleaned("Book the room. music"),
            Some("Book the room.".to_owned())
        );
        assert_eq!(removals("Book the room. music").trailing_noise_units, 1);
        assert_eq!(
            cleaned("Book the room. Okay. Okay. Yeah. Okay. Yeah."),
            Some("Book the room.".to_owned())
        );
        assert_eq!(
            removals("Book the room. Okay. Okay. Yeah. Okay. Yeah.").trailing_noise_units,
            5
        );
    }

    #[test]
    fn short_trailing_closings_survive() {
        for text in [
            "Book the room. Okay.",
            "you",
            "Book it. Yeah. Okay. Yeah. Okay.",
        ] {
            assert_unchanged(text);
        }
    }

    #[test]
    fn a_final_left_with_only_punctuation_is_dropped() {
        assert_eq!(cleaned("[music] ."), None);
    }

    #[test]
    fn cleanup_is_idempotent() {
        for text in [
            "Hello (laughter) how are you?",
            "Set a timer. Thank you for watching.",
            "I am going to go. I am going to go. I am going to go.",
        ] {
            let once = cleaned(text).unwrap();
            assert_unchanged(&once);
        }
    }
}
