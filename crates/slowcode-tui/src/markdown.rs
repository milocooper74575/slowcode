//! Minimal Markdown rendering for the message area.
//!
//! The renderer is line based and deliberately small. It covers the shapes
//! Plan and assistant output actually use: headings, lists, tables, fenced
//! code and inline emphasis. It never mutates the stored text, so the complete
//! Markdown remains the authoritative transcript.

use ratatui::{
    layout::Alignment,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Render one Markdown document into terminal lines.
#[must_use]
pub fn render(text: &str, base: Color) -> Vec<Line<'static>> {
    let lines = text.lines().collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut index = 0;
    let mut fence: Option<usize> = None;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();

        if let Some(marker) = fence_marker(trimmed) {
            let ticks = marker
                .chars()
                .take_while(|character| *character == '`')
                .count();
            match &fence {
                Some(open) if *open == ticks => {
                    fence = None;
                    out.push(Line::from(Span::styled(
                        "──",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                Some(_) => {
                    out.push(code_line(line));
                }
                None => {
                    fence = Some(ticks);
                    let language = trimmed[ticks..].trim();
                    out.push(Line::from(Span::styled(
                        if language.is_empty() {
                            "──".to_owned()
                        } else {
                            format!("── {language} ──")
                        },
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            index += 1;
            continue;
        }

        if fence.is_some() {
            out.push(code_line(line));
            index += 1;
            continue;
        }

        if is_table_start(&lines, index) {
            let (table, consumed) = render_table(&lines[index..], base);
            out.extend(table);
            index += consumed;
            continue;
        }

        if let Some((level, heading)) = heading(trimmed) {
            out.push(heading_line(level, heading));
        } else if let Some(quote) = trimmed.strip_prefix('>') {
            let quote = quote.strip_prefix(' ').unwrap_or(quote);
            let mut spans = vec![Span::styled("│ ", Style::default().fg(Color::DarkGray))];
            spans.extend(inline_spans(quote, Color::DarkGray));
            out.push(Line::from(spans));
        } else if is_rule(trimmed) {
            out.push(Line::from(Span::styled(
                "─".repeat(24),
                Style::default().fg(Color::DarkGray),
            )));
        } else if let Some((indent, marker, rest)) = list_item(line) {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            spans.push(Span::styled(
                marker,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.extend(inline_spans(rest, base));
            out.push(Line::from(spans));
        } else if trimmed.is_empty() {
            out.push(Line::default());
        } else {
            out.push(Line::from(inline_spans(line, base)));
        }
        index += 1;
    }
    for line in &mut out {
        line.alignment = Some(Alignment::Left);
    }
    out
}

fn fence_marker(line: &str) -> Option<&str> {
    let trimmed = line.trim_end();
    if trimmed.starts_with("```") {
        Some(trimmed)
    } else {
        None
    }
}

fn code_line(line: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {line}"),
        Style::default().fg(Color::LightGreen),
    ))
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = line[level..].trim_start();
    if rest.is_empty() {
        return None;
    }
    let rest = rest.trim_end_matches('#').trim_end();
    Some((level, rest))
}

fn heading_line(level: usize, text: &str) -> Line<'static> {
    let style = match level {
        1 => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        2 => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        _ => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    };
    Line::from(Span::styled(text.to_owned(), style))
}

fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 3
        && trimmed
            .chars()
            .all(|character| matches!(character, '-' | '*' | '_'))
}

fn list_item(line: &str) -> Option<(usize, &'static str, &str)> {
    let indent = line.len().saturating_sub(line.trim_start().len());
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("- ") {
        return Some((indent, "• ", rest));
    }
    if let Some(rest) = trimmed.strip_prefix("* ") {
        return Some((indent, "• ", rest));
    }
    if let Some(rest) = trimmed.strip_prefix("+ ") {
        return Some((indent, "• ", rest));
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && trimmed[digits..].starts_with(". ") {
        return Some((indent, "› ", trimmed[digits + 2..].trim_start()));
    }
    None
}

fn is_table_start(lines: &[&str], index: usize) -> bool {
    let Some(header) = lines.get(index) else {
        return false;
    };
    let Some(separator) = lines.get(index + 1) else {
        return false;
    };
    header.contains('|') && is_table_separator(separator)
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return false;
    }
    trimmed.trim_matches('|').split('|').all(|cell| {
        let cell = cell.trim();
        !cell.is_empty()
            && cell
                .chars()
                .all(|character| matches!(character, '-' | ':' | ' '))
            && cell.contains('-')
    })
}

fn table_cells(line: &str) -> Vec<&str> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);
    trimmed.split('|').map(str::trim).collect()
}

fn render_table(lines: &[&str], base: Color) -> (Vec<Line<'static>>, usize) {
    let header = table_cells(lines[0]);
    let mut widths = header
        .iter()
        .map(|cell| display_width(cell).clamp(3, 40))
        .collect::<Vec<_>>();
    let mut index = 2;
    let mut rows = Vec::new();
    while index < lines.len() && lines[index].contains('|') {
        let cells = table_cells(lines[index]);
        if cells.len() != header.len() {
            break;
        }
        for (column, cell) in cells.iter().enumerate() {
            widths[column] = widths[column].max(display_width(cell).min(40));
        }
        rows.push(cells);
        index += 1;
    }

    let mut out = Vec::new();
    out.push(table_row(
        &header,
        &widths,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    let mut separator = Vec::new();
    for width in &widths {
        separator.push(Span::styled(
            "─".repeat(*width),
            Style::default().fg(Color::DarkGray),
        ));
        separator.push(Span::styled("─┼─", Style::default().fg(Color::DarkGray)));
    }
    separator.pop();
    out.push(Line::from(separator));
    for row in rows {
        out.push(table_row(&row, &widths, Style::default().fg(base)));
    }
    (out, index)
}

fn table_row(cells: &[&str], widths: &[usize], style: Style) -> Line<'static> {
    let mut spans = Vec::new();
    for (column, cell) in cells.iter().enumerate() {
        if column > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        }
        let width = widths[column];
        let cell_width = display_width(cell);
        spans.push(Span::styled((*cell).to_owned(), style));
        if cell_width < width {
            spans.push(Span::raw(" ".repeat(width - cell_width)));
        }
    }
    Line::from(spans)
}

fn display_width(text: &str) -> usize {
    Line::from(text).width()
}

fn inline_spans(text: &str, base: Color) -> Vec<Span<'static>> {
    let chars = text.chars().collect::<Vec<_>>();
    let mut spans = Vec::new();
    let mut buffer = String::new();
    let mut index = 0;

    while index < chars.len() {
        let character = chars[index];
        if character == '`'
            && let Some(end) = find(&chars, index + 1, '`')
        {
            flush(&mut spans, &mut buffer, base, Style::default());
            spans.push(Span::styled(
                chars[index + 1..end].iter().collect::<String>(),
                Style::default().fg(Color::Yellow),
            ));
            index = end + 1;
            continue;
        }
        if character == '*'
            && chars.get(index + 1) == Some(&'*')
            && let Some(end) = find_pair(&chars, index + 2, '*', '*')
        {
            flush(&mut spans, &mut buffer, base, Style::default());
            spans.push(Span::styled(
                chars[index + 2..end].iter().collect::<String>(),
                Style::default().fg(base).add_modifier(Modifier::BOLD),
            ));
            index = end + 2;
            continue;
        }
        if character == '_'
            && chars.get(index + 1) == Some(&'_')
            && let Some(end) = find_pair(&chars, index + 2, '_', '_')
        {
            flush(&mut spans, &mut buffer, base, Style::default());
            spans.push(Span::styled(
                chars[index + 2..end].iter().collect::<String>(),
                Style::default().fg(base).add_modifier(Modifier::BOLD),
            ));
            index = end + 2;
            continue;
        }
        if character == '*'
            && let Some(end) = find(&chars, index + 1, '*')
        {
            flush(&mut spans, &mut buffer, base, Style::default());
            spans.push(Span::styled(
                chars[index + 1..end].iter().collect::<String>(),
                Style::default().fg(base).add_modifier(Modifier::ITALIC),
            ));
            index = end + 1;
            continue;
        }
        if character == '['
            && let Some((label, url, next)) = link(&chars, index)
        {
            flush(&mut spans, &mut buffer, base, Style::default());
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::UNDERLINED),
            ));
            spans.push(Span::styled(
                format!(" ({url})"),
                Style::default().fg(Color::DarkGray),
            ));
            index = next;
            continue;
        }
        buffer.push(character);
        index += 1;
    }
    flush(&mut spans, &mut buffer, base, Style::default());
    spans
}

fn flush(spans: &mut Vec<Span<'static>>, buffer: &mut String, base: Color, style: Style) {
    if !buffer.is_empty() {
        spans.push(Span::styled(std::mem::take(buffer), style.fg(base)));
    }
}

fn find(chars: &[char], start: usize, target: char) -> Option<usize> {
    (start..chars.len()).find(|index| chars[*index] == target)
}

fn find_pair(chars: &[char], start: usize, first: char, second: char) -> Option<usize> {
    (start..chars.len().saturating_sub(1))
        .find(|index| chars[*index] == first && chars[index + 1] == second)
}

fn link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close = (start + 1..chars.len()).find(|index| chars[*index] == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = (close + 2..chars.len()).find(|index| chars[*index] == ')')?;
    let label = chars[start + 1..close].iter().collect();
    let url = chars[close + 2..end].iter().collect();
    Some((label, url, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn headings_and_lists_drop_their_control_markers() {
        let rendered = render("# Title\n\n- first\n2. second", Color::Reset);
        let flat = text(&rendered);
        assert!(flat.contains("Title"));
        assert!(!flat.contains("# Title"));
        assert!(flat.contains("• first"));
        assert!(flat.contains("› second"));
    }

    #[test]
    fn fenced_code_keeps_its_content() {
        let rendered = render("```rust\nlet x = 1;\n```", Color::Reset);
        let flat = text(&rendered);
        assert!(flat.contains("rust"));
        assert!(flat.contains("let x = 1;"));
        assert!(!flat.contains("```"));
    }

    #[test]
    fn tables_render_cells_in_columns() {
        let rendered = render("| a | b |\n|---|---|\n| 1 | 2 |", Color::Reset);
        let flat = text(&rendered);
        assert!(flat.contains("a"));
        assert!(flat.contains("1"));
        assert!(flat.contains("│"));
        assert!(!flat.contains("---"));
    }

    #[test]
    fn inline_code_bold_and_links_are_styled() {
        let rendered = render(
            "use `write` and **bold** and [docs](https://x.test)",
            Color::Reset,
        );
        let flat = text(&rendered);
        assert!(flat.contains("write"));
        assert!(flat.contains("bold"));
        assert!(flat.contains("docs"));
        assert!(flat.contains("https://x.test"));
        assert!(!flat.contains("**"));
    }

    #[test]
    fn cjk_table_cells_keep_the_same_visual_width() {
        let rendered = render(
            "| 任务 | 状态 |\n|---|---|\n| 读取输入 | 可执行 |",
            Color::Reset,
        );
        assert!(rendered.len() >= 3);
        assert_eq!(rendered[0].width(), rendered[2].width());
    }

    #[test]
    fn unclosed_fence_renders_the_remainder_as_code() {
        let rendered = render("```rust\nlet x = 1;\nlet y = 2;", Color::Reset);
        let flat = text(&rendered);
        assert!(flat.contains("let x = 1;"));
        assert!(flat.contains("let y = 2;"));
        assert!(!flat.contains("```"));
    }

    #[test]
    fn lone_inline_markers_do_not_panic_or_swallow_text() {
        let rendered = render("2 * 3 and a _ b and [not a link", Color::Reset);
        let flat = text(&rendered);
        assert!(flat.contains("2 * 3"));
        assert!(flat.contains("_ b"));
        assert!(flat.contains("[not a link"));
    }
}
