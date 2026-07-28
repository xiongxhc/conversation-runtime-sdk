pub(super) fn normalize_speech_text(input: &str) -> Option<String> {
    let mut normalized = Vec::new();

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") && trimmed.trim_matches('`').is_empty() {
            continue;
        }

        let line = strip_line_prefixes(trimmed);
        let line = strip_paired_delimiters(line);
        let line = line.trim();
        if !line.is_empty() {
            normalized.push(line.to_owned());
        }
    }

    let normalized = normalized.join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

fn strip_line_prefixes(line: &str) -> &str {
    if line == "#" || is_thematic_break(line) {
        return "";
    }

    let heading_length = line.bytes().take_while(|byte| *byte == b'#').count();
    let line = if (1..=6).contains(&heading_length)
        && line[heading_length..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
    {
        line[heading_length..].trim_start()
    } else {
        line
    };

    if let Some(rest) = line.strip_prefix('*') {
        if rest.chars().next().is_some_and(char::is_whitespace) {
            return rest.trim_start();
        }
    }

    line
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

        let asterisk_width = [2, 1].into_iter().find(|width| {
            asterisk_run_length(remaining) == *width && can_open_asterisks(input, cursor, *width)
        });
        if let Some(width) = asterisk_width {
            if let Some(end) = find_closing_asterisks(input, cursor + width, width) {
                normalized.push_str(&strip_paired_delimiters(&input[cursor + width..end]));
                cursor = end + width;
                continue;
            }
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
            Some("标题 第一项 这是重点，也是代码。 示例")
        );
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
        assert_eq!(
            normalize_speech_text("***nested***").as_deref(),
            Some("***nested***")
        );
    }

    #[test]
    fn removes_supported_delimiters_nested_inside_emphasis() {
        assert_eq!(
            normalize_speech_text("**say `hello`** and *keep **nested** natural*.").as_deref(),
            Some("say hello and keep nested natural.")
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
            Some("标题")
        );
    }

    #[test]
    fn formatting_only_input_is_skipped() {
        assert_eq!(normalize_speech_text("#\n***\n```"), None);
    }
}
