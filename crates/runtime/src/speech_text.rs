pub(super) fn normalize_speech_text(input: &str) -> Option<String> {
    let mut normalized = Vec::new();

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") && trimmed.trim_matches('`').is_empty() {
            continue;
        }

        let (line, is_heading) = strip_line_prefixes(trimmed);
        let line = strip_paired_delimiters(line);
        let line = normalize_decorative_speech(&line, is_heading, trimmed.len());
        let line = line.trim();
        if !line.is_empty() {
            normalized.push(line.to_owned());
        }
    }

    let normalized = normalized.join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

fn strip_line_prefixes(line: &str) -> (&str, bool) {
    if line == "#" || is_thematic_break(line) {
        return ("", false);
    }

    let heading_length = line.bytes().take_while(|byte| *byte == b'#').count();
    let is_heading = (1..=6).contains(&heading_length)
        && line[heading_length..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
    let line = if is_heading {
        line[heading_length..].trim_start()
    } else {
        line
    };

    if let Some(rest) = line.strip_prefix('*') {
        if rest.chars().next().is_some_and(char::is_whitespace) {
            return (rest.trim_start(), is_heading);
        }
    }

    (line, is_heading)
}

fn is_thematic_break(line: &str) -> bool {
    let mut marker = None;
    let mut count = 0;

    for character in line.chars().filter(|character| !character.is_whitespace()) {
        if !matches!(character, '*' | '-' | '_') {
            return false;
        }
        if marker.is_some_and(|marker| marker != character) {
            return false;
        }
        marker = Some(character);
        count += 1;
    }

    count >= 3
}

fn normalize_decorative_speech(input: &str, is_heading: bool, source_byte_limit: usize) -> String {
    let input = if is_heading {
        strip_heading_ordinal(input)
    } else {
        input
    };
    let mut normalized = String::with_capacity(input.len());
    let mut converted_heading_colon = false;

    for character in input.chars() {
        match character {
            '《' | '》' | '〈' | '〉' | '「' | '」' | '『' | '』' | '【' | '】' if is_heading =>
                {}
            '：' if is_heading && !converted_heading_colon => {
                normalized.push('，');
                converted_heading_colon = true;
            }
            _ => normalized.push(character),
        }
    }

    let normalized = normalized.trim();
    if is_heading
        && !normalized.is_empty()
        && !normalized
            .chars()
            .next_back()
            .is_some_and(is_sentence_ending)
    {
        let mut sentence = normalized.to_owned();
        if sentence.len().saturating_add('。'.len_utf8()) <= source_byte_limit {
            sentence.push('。');
        } else if sentence.len().saturating_add(1) <= source_byte_limit {
            sentence.push('.');
        }
        sentence
    } else {
        normalized.to_owned()
    }
}

fn strip_heading_ordinal(input: &str) -> &str {
    let digit_count = input.bytes().take_while(u8::is_ascii_digit).count();
    if digit_count == 0 {
        return input;
    }

    let rest = &input[digit_count..];
    for separator in [".", "、"] {
        if let Some(rest) = rest.strip_prefix(separator) {
            if rest.chars().next().is_some_and(char::is_whitespace) {
                return rest.trim_start();
            }
        }
    }

    input
}

fn is_sentence_ending(character: char) -> bool {
    matches!(character, '.' | '!' | '?' | '。' | '！' | '？')
}

fn strip_paired_delimiters(input: &str) -> String {
    let mut normalized = String::new();
    let mut cursor = 0;

    while cursor < input.len() {
        let remaining = &input[cursor..];
        if let Some(rest) = remaining.strip_prefix('`') {
            if let Some(end) = rest.find('`') {
                normalized.push_str(&rest[..end]);
                cursor += end + 2;
                continue;
            }
        }

        if remaining.starts_with('*') {
            let width = asterisk_run_length(remaining);
            if matches!(width, 1 | 2) && can_open_asterisks(input, cursor, width) {
                if let Some(end) = find_closing_asterisks(input, cursor + width, width) {
                    normalized.push_str(&strip_paired_delimiters(&input[cursor + width..end]));
                    cursor = end + width;
                    continue;
                }
            }

            normalized.push_str(&remaining[..width]);
            cursor += width;
            continue;
        }

        let character = remaining
            .chars()
            .next()
            .expect("cursor remains on a character boundary");
        normalized.push(character);
        cursor += character.len_utf8();
    }

    normalized
}

fn find_closing_asterisks(input: &str, mut cursor: usize, width: usize) -> Option<usize> {
    while cursor < input.len() {
        let remaining = &input[cursor..];
        if let Some(rest) = remaining.strip_prefix('`') {
            if let Some(end) = rest.find('`') {
                cursor += end + 2;
                continue;
            }
        }

        if remaining.starts_with('*') {
            let run_length = asterisk_run_length(remaining);
            if run_length == width && can_close_asterisks(input, cursor) {
                return Some(cursor);
            }
            cursor += run_length;
            continue;
        }

        let character = remaining
            .chars()
            .next()
            .expect("cursor remains on a character boundary");
        cursor += character.len_utf8();
    }

    None
}

fn asterisk_run_length(input: &str) -> usize {
    input.bytes().take_while(|byte| *byte == b'*').count()
}

fn can_open_asterisks(input: &str, start: usize, width: usize) -> bool {
    let previous = input[..start].chars().next_back();
    let following = input[start + width..].chars().next();

    following.is_some_and(|character| !character.is_whitespace())
        && !(width == 1
            && previous.is_some_and(|character| character.is_ascii_digit())
            && following.is_some_and(|character| character.is_ascii_digit()))
}

fn can_close_asterisks(input: &str, start: usize) -> bool {
    input[..start]
        .chars()
        .next_back()
        .is_some_and(|character| !character.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::normalize_speech_text;

    #[test]
    fn removes_supported_markdown_markers_and_collapses_layout() {
        assert_eq!(
            normalize_speech_text("# 标题\n* 第一项\n这是**重点**，也是`代码`。\n```\n示例\n```")
                .as_deref(),
            Some("标题. 第一项 这是重点，也是代码。 示例")
        );
    }

    #[test]
    fn converts_story_headings_to_spoken_prose() {
        assert_eq!(
            normalize_speech_text(
                "### 故事名：《第25小时的雨》\n\n#### 1. 初遇：咖啡馆的旧雨伞\n\n林默是一家专门"
            )
            .as_deref(),
            Some("故事名，第25小时的雨。 初遇，咖啡馆的旧雨伞。 林默是一家专门")
        );
    }

    #[test]
    fn preserves_decorative_quotes_outside_markdown_headings() {
        let input = "他说：「保留 C#」；另见【附录】。";

        assert_eq!(normalize_speech_text(input).as_deref(), Some(input));
    }

    #[test]
    fn preserves_literal_ascii_colons_inside_markdown_headings() {
        assert_eq!(
            normalize_speech_text("### API: https://host/a at 10:30").as_deref(),
            Some("API: https://host/a at 10:30。")
        );
    }

    #[test]
    fn heading_normalization_never_expands_past_the_source_byte_length() {
        let input = "# abcdef";
        let normalized = normalize_speech_text(input).unwrap();

        assert_eq!(normalized, "abcdef.");
        assert!(normalized.len() <= input.len());
    }

    #[test]
    fn preserves_literal_hash_star_and_hashtag_content() {
        assert_eq!(
            normalize_speech_text("C#、#topic 和 2*3 保持原样。").as_deref(),
            Some("C#、#topic 和 2*3 保持原样。")
        );
    }

    #[test]
    fn preserves_literal_asterisk_pairs_outside_emphasis_context() {
        for input in [
            "Use * as a wildcard and * as a marker.",
            "使用 * 作为通配符，另一个 * 作为标记。",
        ] {
            assert_eq!(normalize_speech_text(input).as_deref(), Some(input));
        }
    }

    #[test]
    fn preserves_unsupported_inline_asterisk_runs() {
        for input in ["***nested***", "***nested**", "**unfinished*"] {
            assert_eq!(normalize_speech_text(input).as_deref(), Some(input));
        }
    }

    #[test]
    fn removes_supported_delimiters_nested_inside_emphasis() {
        assert_eq!(
            normalize_speech_text("**say `hello`** and *keep **nested** natural*.").as_deref(),
            Some("say hello and keep nested natural.")
        );
        assert_eq!(
            normalize_speech_text("*say `2*3`*").as_deref(),
            Some("say 2*3")
        );
    }

    #[test]
    fn preserves_unmatched_delimiters_with_ascii_text() {
        for input in ["*unfinished", "**unfinished", "`unfinished"] {
            assert_eq!(normalize_speech_text(input).as_deref(), Some(input));
        }
    }

    #[test]
    fn preserves_unmatched_delimiters_with_utf8_text() {
        for input in ["你好*未闭合", "你好**未闭合", "你好`未闭合"] {
            assert_eq!(normalize_speech_text(input).as_deref(), Some(input));
        }
    }

    #[test]
    fn preserves_unsupported_hash_runs_without_heading_whitespace() {
        assert_eq!(normalize_speech_text("##").as_deref(), Some("##"));
        assert_eq!(normalize_speech_text("#######").as_deref(), Some("#######"));
        assert_eq!(
            normalize_speech_text("###### 标题").as_deref(),
            Some("标题。")
        );
    }

    #[test]
    fn formatting_only_input_is_skipped() {
        assert_eq!(normalize_speech_text("#\n***\n```"), None);
    }
}
