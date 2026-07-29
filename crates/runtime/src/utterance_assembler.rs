use conversation_protocol::{RuntimeError, RuntimeErrorKind, RuntimeStage};

const DEFAULT_SOFT_LIMIT_BYTES: usize = 384;
const DEFAULT_HARD_LIMIT_BYTES: usize = 1_024;
const MAX_UTF8_SCALAR_BYTES: usize = 4;

pub struct UtteranceAssembler {
    soft_limit_bytes: usize,
    hard_limit_bytes: usize,
    buffer: String,
}

impl UtteranceAssembler {
    pub fn new(soft_limit_bytes: usize, hard_limit_bytes: usize) -> Result<Self, RuntimeError> {
        if soft_limit_bytes == 0 {
            return Err(configuration_error(
                "utterance soft byte limit must be non-zero",
            ));
        }
        if hard_limit_bytes == 0 {
            return Err(configuration_error(
                "utterance hard byte limit must be non-zero",
            ));
        }
        if soft_limit_bytes > hard_limit_bytes {
            return Err(configuration_error(
                "utterance soft byte limit must not exceed hard byte limit",
            ));
        }
        if hard_limit_bytes < MAX_UTF8_SCALAR_BYTES {
            return Err(configuration_error(
                "utterance hard byte limit must be at least 4 to preserve UTF-8 scalar boundaries",
            ));
        }

        Ok(Self {
            soft_limit_bytes,
            hard_limit_bytes,
            buffer: String::new(),
        })
    }

    pub const fn soft_limit_bytes(&self) -> usize {
        self.soft_limit_bytes
    }

    pub const fn hard_limit_bytes(&self) -> usize {
        self.hard_limit_bytes
    }

    pub fn push_delta(&mut self, delta: &str) -> Vec<String> {
        self.buffer.push_str(delta);

        let mut utterances = Vec::new();
        while let Some(end) = self.next_utterance_end() {
            if let Some(utterance) = self.drain_utterance(end, false) {
                utterances.push(utterance);
            }
            if self.buffer.len() < self.hard_limit_bytes {
                break;
            }
        }
        utterances
    }

    pub fn finish(mut self) -> Option<String> {
        self.drain_utterance(self.buffer.len(), true)
    }

    fn next_utterance_end(&self) -> Option<usize> {
        if self.buffer.len() < self.soft_limit_bytes {
            return None;
        }

        let paragraph_boundaries = paragraph_boundaries(&self.buffer, self.hard_limit_bytes);
        if let Some(end) = paragraph_boundaries
            .iter()
            .copied()
            .rfind(|end| *end <= self.soft_limit_bytes)
        {
            return Some(end);
        }
        if let Some(end) = paragraph_boundaries
            .iter()
            .copied()
            .find(|end| *end <= self.hard_limit_bytes)
        {
            return Some(end);
        }

        let safe_boundaries = safe_boundaries(&self.buffer, self.hard_limit_bytes);
        if self.buffer.len() < self.hard_limit_bytes {
            return safe_boundaries
                .into_iter()
                .rfind(|end| *end >= self.soft_limit_bytes);
        }

        safe_boundaries
            .into_iter()
            .next_back()
            .or_else(|| Some(self.hard_split_end()))
    }

    fn hard_split_end(&self) -> usize {
        if self.buffer.len() <= self.hard_limit_bytes {
            return self.buffer.len();
        }

        self.buffer
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= self.hard_limit_bytes)
            .last()
            .filter(|end| *end > 0)
            .unwrap_or_else(|| {
                self.buffer
                    .char_indices()
                    .nth(1)
                    .map_or(self.buffer.len(), |(index, _)| index)
            })
    }

    fn drain_utterance(&mut self, end: usize, trim: bool) -> Option<String> {
        let utterance: String = self.buffer.drain(..end).collect();
        if utterance.trim().is_empty() {
            return None;
        }

        if trim {
            Some(utterance.trim().to_owned())
        } else {
            Some(utterance)
        }
    }
}

impl Default for UtteranceAssembler {
    fn default() -> Self {
        Self {
            soft_limit_bytes: DEFAULT_SOFT_LIMIT_BYTES,
            hard_limit_bytes: DEFAULT_HARD_LIMIT_BYTES,
            buffer: String::new(),
        }
    }
}

fn paragraph_boundaries(input: &str, hard_limit_bytes: usize) -> Vec<usize> {
    input
        .match_indices("\n\n")
        .map(|(index, boundary)| index + boundary.len())
        .take_while(|end| *end <= hard_limit_bytes)
        .collect()
}

fn safe_boundaries(input: &str, hard_limit_bytes: usize) -> Vec<usize> {
    input
        .char_indices()
        .filter_map(|(index, character)| {
            let end = index + character.len_utf8();
            (end <= hard_limit_bytes && is_safe_boundary(character)).then_some(end)
        })
        .collect()
}

fn is_safe_boundary(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '.' | '!' | '?' | '。' | '！' | '？' | ',' | ':' | ';' | '，' | '：' | '；'
        )
}

fn configuration_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Configuration,
        RuntimeStage::Runtime,
        message,
    )
}
