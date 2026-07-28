use conversation_protocol::{RuntimeError, RuntimeErrorKind, RuntimeStage};

const DEFAULT_SOFT_LIMIT_BYTES: usize = 96;
const DEFAULT_HARD_LIMIT_BYTES: usize = 192;
const MAX_UTF8_SCALAR_BYTES: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhraseChunkingConfig {
    soft_limit_bytes: usize,
    hard_limit_bytes: usize,
}

impl PhraseChunkingConfig {
    pub fn new(soft_limit_bytes: usize, hard_limit_bytes: usize) -> Result<Self, RuntimeError> {
        if soft_limit_bytes == 0 {
            return Err(configuration_error(
                "phrase soft byte limit must be non-zero",
            ));
        }
        if hard_limit_bytes == 0 {
            return Err(configuration_error(
                "phrase hard byte limit must be non-zero",
            ));
        }
        if soft_limit_bytes > hard_limit_bytes {
            return Err(configuration_error(
                "phrase soft byte limit must not exceed hard byte limit",
            ));
        }
        if hard_limit_bytes < MAX_UTF8_SCALAR_BYTES {
            return Err(configuration_error(
                "phrase hard byte limit must be at least 4 to preserve UTF-8 scalar boundaries",
            ));
        }

        Ok(Self {
            soft_limit_bytes,
            hard_limit_bytes,
        })
    }

    pub fn soft_limit_bytes(&self) -> usize {
        self.soft_limit_bytes
    }

    pub fn hard_limit_bytes(&self) -> usize {
        self.hard_limit_bytes
    }
}

impl Default for PhraseChunkingConfig {
    fn default() -> Self {
        Self {
            soft_limit_bytes: DEFAULT_SOFT_LIMIT_BYTES,
            hard_limit_bytes: DEFAULT_HARD_LIMIT_BYTES,
        }
    }
}

pub(super) struct PhraseChunker {
    config: PhraseChunkingConfig,
    buffer: String,
}

impl PhraseChunker {
    pub(super) fn new(config: PhraseChunkingConfig) -> Self {
        Self {
            config,
            buffer: String::new(),
        }
    }

    pub(super) fn push_delta(&mut self, delta: &str) -> Vec<String> {
        self.buffer.push_str(delta);

        let mut phrases = Vec::new();
        while let Some(end) = self.next_segment_end() {
            if let Some(phrase) = self.drain_segment(end) {
                phrases.push(phrase);
            }
        }
        phrases
    }

    pub(super) fn finish(mut self) -> Option<String> {
        self.drain_segment(self.buffer.len())
    }

    fn next_segment_end(&self) -> Option<usize> {
        let mut preferred_end = None;
        let mut soft_end = None;

        for (index, character) in self.buffer.char_indices() {
            let end = index + character.len_utf8();

            if end > self.config.hard_limit_bytes {
                return preferred_end
                    .or(soft_end)
                    .or_else(|| Some(self.hard_split_end()));
            }
            if character == '\n' || Self::is_sentence_boundary(character) {
                preferred_end = Some(end);
            }
            if Self::is_soft_boundary(character) {
                soft_end = Some(end);
            }
            if end >= self.config.soft_limit_bytes {
                if character == '\n' || Self::is_sentence_boundary(character) {
                    return preferred_end;
                }
                if preferred_end.is_none() && Self::is_soft_boundary(character) {
                    return Some(end);
                }
            }
            if end >= self.config.hard_limit_bytes {
                return preferred_end
                    .or(soft_end)
                    .or_else(|| Some(self.hard_split_end()));
            }
        }

        None
    }

    fn hard_split_end(&self) -> usize {
        if self.buffer.len() <= self.config.hard_limit_bytes {
            return self.buffer.len();
        }

        let mut end = 0;
        for (index, _) in self.buffer.char_indices() {
            if index > self.config.hard_limit_bytes {
                break;
            }
            end = index;
        }

        if end == 0 {
            self.buffer
                .char_indices()
                .nth(1)
                .map_or(self.buffer.len(), |(index, _)| index)
        } else {
            end
        }
    }

    fn drain_segment(&mut self, end: usize) -> Option<String> {
        let segment: String = self.buffer.drain(..end).collect();
        let segment = segment.trim();
        (!segment.is_empty()).then(|| segment.to_owned())
    }

    fn is_sentence_boundary(character: char) -> bool {
        matches!(character, '.' | '!' | '?' | '。' | '！' | '？')
    }

    fn is_soft_boundary(character: char) -> bool {
        character.is_whitespace() || matches!(character, ',' | ':' | ';' | '，' | '：' | '；')
    }
}

impl Default for PhraseChunker {
    fn default() -> Self {
        Self::new(PhraseChunkingConfig::default())
    }
}

fn configuration_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Configuration,
        RuntimeStage::Runtime,
        message,
    )
}

#[cfg(test)]
mod tests {
    use conversation_protocol::{RuntimeErrorKind, RuntimeStage};

    use super::{PhraseChunker, PhraseChunkingConfig};

    #[test]
    fn fragmented_multilingual_deltas_buffer_short_phrases() {
        let mut chunker = PhraseChunker::default();
        assert!(chunker.push_delta("你好，").is_empty());
        assert!(chunker.push_delta("世界。Next").is_empty());
        assert!(chunker.push_delta(" sentence!").is_empty());
        assert_eq!(
            chunker.finish().as_deref(),
            Some("你好，世界。Next sentence!")
        );
    }

    #[test]
    fn hard_limit_never_splits_utf8() {
        let mut chunker = PhraseChunker::new(PhraseChunkingConfig::new(6, 9).unwrap());
        assert_eq!(chunker.push_delta("你好世界"), vec!["你好世"]);
        assert_eq!(chunker.finish().as_deref(), Some("界"));
    }

    #[test]
    fn hard_limit_keeps_a_multibyte_phrase_at_an_exact_boundary() {
        let mut chunker = PhraseChunker::new(PhraseChunkingConfig::new(6, 6).unwrap());
        assert_eq!(chunker.push_delta("你好"), vec!["你好"]);
        assert_eq!(chunker.finish(), None);
    }

    #[test]
    fn hard_limit_supports_any_utf8_scalar_without_exceeding_it() {
        let config = PhraseChunkingConfig::new(1, 4).unwrap();
        let mut chunker = PhraseChunker::new(config);
        let phrases = chunker.push_delta("😀");

        assert_eq!(phrases, vec!["😀"]);
        assert!(phrases
            .iter()
            .all(|phrase| phrase.len() <= config.hard_limit_bytes()));
        assert_eq!(chunker.finish(), None);
    }

    #[test]
    fn multibyte_boundaries_never_exceed_the_hard_limit() {
        let config = PhraseChunkingConfig::new(6, 9).unwrap();
        let cases = [
            ("aaaaaaa。", vec!["aaaaaaa", "。"]),
            ("aaaaaaa，", vec!["aaaaaaa", "，"]),
            ("aaaaaaa\u{2003}", vec!["aaaaaaa"]),
        ];

        for (input, expected) in cases {
            let mut chunker = PhraseChunker::new(config);
            let mut phrases = chunker.push_delta(input);
            phrases.extend(chunker.finish());

            assert_eq!(phrases, expected, "input: {input:?}");
            assert!(
                phrases
                    .iter()
                    .all(|phrase| phrase.len() <= config.hard_limit_bytes()),
                "input: {input:?}, phrases: {phrases:?}"
            );
        }
    }

    #[test]
    fn short_sentence_boundaries_wait_for_more_context() {
        let mut chunker = PhraseChunker::default();

        assert!(chunker.push_delta("你好。").is_empty());
        assert!(chunker.push_delta("今天很好！").is_empty());
        assert_eq!(chunker.finish().as_deref(), Some("你好。今天很好！"));
    }

    #[test]
    fn latest_sentence_boundary_is_used_after_the_soft_limit() {
        let config = PhraseChunkingConfig::new(12, 24).unwrap();
        let mut chunker = PhraseChunker::new(config);

        assert_eq!(chunker.push_delta("甲。乙乙乙。丙丙"), vec!["甲。乙乙乙。"]);
        assert_eq!(chunker.finish().as_deref(), Some("丙丙"));
    }

    #[test]
    fn short_newlines_are_buffered_but_consumed_at_finish() {
        let mut chunker = PhraseChunker::default();
        assert!(chunker.push_delta("# 标题\n第一行\n第二行").is_empty());
        assert_eq!(chunker.finish().as_deref(), Some("# 标题\n第一行\n第二行"));
    }

    #[test]
    fn leading_space_is_not_selected_after_later_text_reaches_the_soft_limit() {
        let mut chunker = PhraseChunker::new(PhraseChunkingConfig::new(5, 12).unwrap());
        chunker.buffer.push_str(" hello");

        assert_eq!(chunker.next_segment_end(), None);
    }

    #[test]
    fn soft_limit_flushes_at_whitespace_comma_colon_and_semicolon() {
        let config = PhraseChunkingConfig::new(5, 32).unwrap();

        let mut whitespace = PhraseChunker::new(config);
        assert_eq!(whitespace.push_delta("hello world"), vec!["hello"]);
        assert_eq!(whitespace.finish().as_deref(), Some("world"));

        let mut comma = PhraseChunker::new(config);
        assert_eq!(comma.push_delta("hello, world"), vec!["hello,"]);
        assert_eq!(comma.finish().as_deref(), Some("world"));

        let mut colon = PhraseChunker::new(config);
        assert_eq!(colon.push_delta("hello: world"), vec!["hello:"]);
        assert_eq!(colon.finish().as_deref(), Some("world"));

        let mut semicolon = PhraseChunker::new(config);
        assert_eq!(semicolon.push_delta("hello; world"), vec!["hello;"]);
        assert_eq!(semicolon.finish().as_deref(), Some("world"));
    }

    #[test]
    fn one_delta_can_flush_multiple_phrases() {
        let mut chunker = PhraseChunker::new(PhraseChunkingConfig::new(12, 24).unwrap());
        assert_eq!(
            chunker.push_delta("One. Two? Three! Four. Five? Six!"),
            vec!["One. Two? Three!", "Four. Five?"]
        );
        assert_eq!(chunker.finish().as_deref(), Some("Six!"));
    }

    #[test]
    fn finish_returns_trimmed_remainder() {
        let mut chunker = PhraseChunker::default();
        assert!(chunker.push_delta("  final phrase  ").is_empty());
        assert_eq!(chunker.finish().as_deref(), Some("final phrase"));
    }

    #[test]
    fn whitespace_only_input_produces_no_phrases() {
        let mut chunker = PhraseChunker::default();
        assert!(chunker.push_delta(" \t\n ").is_empty());
        assert_eq!(chunker.finish(), None);
    }

    #[test]
    fn configuration_rejects_zero_and_reversed_limits() {
        assert!(PhraseChunkingConfig::new(0, 1).is_err());
        assert!(PhraseChunkingConfig::new(1, 0).is_err());
        assert!(PhraseChunkingConfig::new(9, 6).is_err());
    }

    #[test]
    fn configuration_requires_a_hard_limit_for_any_utf8_scalar() {
        let error = PhraseChunkingConfig::new(1, 3).unwrap_err();
        assert_eq!(error.kind(), RuntimeErrorKind::Configuration);
        assert_eq!(error.stage(), RuntimeStage::Runtime);
        assert_eq!(
            error.message(),
            "phrase hard byte limit must be at least 4 to preserve UTF-8 scalar boundaries"
        );

        assert!(PhraseChunkingConfig::new(1, 4).is_ok());
    }

    #[test]
    fn configuration_exposes_validated_limits() {
        let config = PhraseChunkingConfig::new(6, 9).unwrap();
        assert_eq!(config.soft_limit_bytes(), 6);
        assert_eq!(config.hard_limit_bytes(), 9);
    }

    #[test]
    fn default_configuration_uses_phrase_sized_limits() {
        let config = PhraseChunkingConfig::default();
        assert_eq!(config.soft_limit_bytes(), 96);
        assert_eq!(config.hard_limit_bytes(), 192);
    }
}
