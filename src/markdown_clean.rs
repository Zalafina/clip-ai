//! Lightweight plain-text cleaner that strips noisy Markdown markup while keeping code intact.

/// Clean common Markdown syntax into readable plain text for clipboard pasting.
pub fn clean_markdown(input: &str) -> String {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut out_lines: Vec<String> = Vec::new();
    let mut in_fence = false;

    for raw_line in normalized.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out_lines.push(raw_line.to_string());
            continue;
        }

        if is_horizontal_rule(trimmed) {
            continue;
        }

        let mut line = strip_heading_prefix(raw_line);
        line = strip_blockquote_prefix(&line);
        line = strip_markdown_links(&line);
        line = strip_paired_emphasis(&line, "**");
        line = strip_paired_emphasis(&line, "__");
        line = strip_inline_backticks(&line);
        out_lines.push(line);
    }

    collapse_blank_lines(&out_lines).trim().to_string()
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    if trimmed.len() < 3 {
        return false;
    }
    let ch = trimmed.chars().next().unwrap_or(' ');
    if !matches!(ch, '-' | '*' | '_') {
        return false;
    }
    trimmed.chars().all(|c| c == ch || c == ' ') && trimmed.chars().filter(|&c| c == ch).count() >= 3
}

fn strip_heading_prefix(line: &str) -> String {
    let trimmed_start = line.trim_start();
    let hash_count = trimmed_start.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hash_count) {
        let rest = &trimmed_start[hash_count..];
        if rest.starts_with(' ') || rest.starts_with('\t') {
            return rest.trim_start().to_string();
        }
    }
    line.to_string()
}

fn strip_blockquote_prefix(line: &str) -> String {
    let mut current = line.trim_start();
    let mut stripped = false;
    while let Some(after) = current.strip_prefix('>') {
        stripped = true;
        current = after.strip_prefix(' ').unwrap_or(after).trim_start();
    }
    if stripped {
        current.to_string()
    } else {
        line.to_string()
    }
}

fn strip_markdown_links(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Handle optional image prefix `![alt](url)` -> `alt` or `[text](url)` -> `text (url)`
        let is_image = chars[i] == '!' && i + 1 < chars.len() && chars[i + 1] == '[';
        if chars[i] == '[' || is_image {
            let label_start = if is_image { i + 2 } else { i + 1 };
            if let Some(close_bracket) = find_char(&chars, label_start, ']') {
                if close_bracket + 1 < chars.len() && chars[close_bracket + 1] == '(' {
                    if let Some(close_paren) = find_char(&chars, close_bracket + 2, ')') {
                        let label: String = chars[label_start..close_bracket].iter().collect();
                        let url: String = chars[close_bracket + 2..close_paren].iter().collect();
                        if !url.contains(' ') {
                            if is_image {
                                result.push_str(&label);
                            } else if label.trim().is_empty() || label.trim() == url.trim() {
                                result.push_str(url.trim());
                            } else {
                                result.push_str(&format!("{label} ({url})"));
                            }
                            i = close_paren + 1;
                            continue;
                        }
                    }
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

fn find_char(chars: &[char], start: usize, target: char) -> Option<usize> {
    (start..chars.len()).find(|&idx| chars[idx] == target)
}

fn strip_paired_emphasis(line: &str, marker: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find(marker) {
        let after_open = &rest[start + marker.len()..];
        if let Some(end) = after_open.find(marker) {
            if end > 0 {
                // Avoid stripping identifiers like `foo__bar__baz` when marker is `__`
                let before_char = rest[..start].chars().last();
                let inside_first = after_open.chars().next();
                let inside_last = after_open[..end].chars().last();
                let after_char = after_open[end + marker.len()..].chars().next();
                let touches_alnum = marker == "__"
                    && (before_char.is_some_and(|c| c.is_alphanumeric())
                        || after_char.is_some_and(|c| c.is_alphanumeric()));
                let whitespace_padded = inside_first.is_some_and(char::is_whitespace)
                    || inside_last.is_some_and(char::is_whitespace);

                if !touches_alnum && !whitespace_padded {
                    out.push_str(&rest[..start]);
                    out.push_str(&after_open[..end]);
                    rest = &after_open[end + marker.len()..];
                    continue;
                }
            }
        }
        out.push_str(&rest[..start + marker.len()]);
        rest = after_open;
    }
    out.push_str(rest);
    out
}

fn strip_inline_backticks(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find('`') {
        let after_open = &rest[start + 1..];
        if let Some(end) = after_open.find('`') {
            if end > 0 {
                out.push_str(&rest[..start]);
                out.push_str(&after_open[..end]);
                rest = &after_open[end + 1..];
                continue;
            }
        }
        out.push_str(&rest[..start + 1]);
        rest = after_open;
    }
    out.push_str(rest);
    out
}

fn collapse_blank_lines(lines: &[String]) -> String {
    let mut result = String::new();
    let mut blank_run = 0usize;
    for line in lines {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !result.is_empty() {
                result.push('\n');
            }
        } else {
            blank_run = 0;
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            } else if result.ends_with('\n') {
                // already ends with newline(s), push line directly
            }
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(line.trim_end());
            result.push('\n');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_headings_bold_links_and_keeps_code_contents() {
        let md = "### 标题\n\n这是 **加粗** 和 `inline_code` 以及 [官网](https://example.com)。\n\n```python\ndef my_func_name():\n    return 42\n```\n";
        let cleaned = clean_markdown(md);
        assert_eq!(
            cleaned,
            "标题\n\n这是 加粗 和 inline_code 以及 官网 (https://example.com)。\n\ndef my_func_name():\n    return 42"
        );
    }

    #[test]
    fn preserves_snake_case_and_dunder_identifiers() {
        let text = "变量 foo__bar__baz 和 __init__ 保持不变，但 __重点__ 去除标记。";
        let cleaned = clean_markdown(text);
        assert!(cleaned.contains("foo__bar__baz"));
        assert!(cleaned.contains("重点"));
        assert!(!cleaned.contains("__重点__"));
    }

    #[test]
    fn strips_blockquote_and_horizontal_rule() {
        let text = "> 引用内容\n---\n正文";
        let cleaned = clean_markdown(text);
        assert_eq!(cleaned, "引用内容\n正文");
    }
}
