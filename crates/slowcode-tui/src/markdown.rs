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

const MAX_TABLE_COLUMNS: usize = 4;
const MAX_TABLE_ROWS: usize = 6;
const MAX_TABLE_CELL_WIDTH: usize = 18;

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
    let mut index = 2;
    let mut rows = Vec::new();
    while index < lines.len() && lines[index].contains('|') {
        let cells = table_cells(lines[index]);
        if cells.len() != header.len() {
            break;
        }
        rows.push(cells);
        index += 1;
    }

    let total_columns = header.len();
    let columns = select_columns(&header);
    let header = columns
        .iter()
        .map(|column| header[*column])
        .collect::<Vec<_>>();
    let mut widths = header
        .iter()
        .map(|cell| display_width(cell).clamp(3, MAX_TABLE_CELL_WIDTH))
        .collect::<Vec<_>>();
    let hidden_rows = rows.len().saturating_sub(MAX_TABLE_ROWS);
    let rows = rows
        .into_iter()
        .take(MAX_TABLE_ROWS)
        .map(|row| {
            columns
                .iter()
                .map(|column| row[*column])
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    for row in &rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(display_width(cell).min(MAX_TABLE_CELL_WIDTH));
        }
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
    let hidden_columns = total_columns.saturating_sub(columns.len());
    if hidden_rows > 0 || hidden_columns > 0 {
        let mut note = String::from("…");
        if hidden_rows > 0 {
            note.push_str(&format!(" {hidden_rows} more row(s)"));
        }
        if hidden_columns > 0 {
            if hidden_rows > 0 {
                note.push(',');
            }
            note.push_str(&format!(" {hidden_columns} more column(s)"));
        }
        note.push_str("; full table in the plan artifact");
        out.push(Line::from(Span::styled(
            note,
            Style::default().fg(Color::DarkGray),
        )));
    }
    (out, index)
}

fn select_columns(header: &[&str]) -> Vec<usize> {
    if header.len() <= MAX_TABLE_COLUMNS {
        return (0..header.len()).collect();
    }
    let priority = [
        "任务",
        "认知属性",
        "执行状态",
        "用户角色",
        "用户动作",
        "结果",
        "状态",
    ];
    let mut selected = Vec::new();
    for name in priority {
        if selected.len() == MAX_TABLE_COLUMNS {
            break;
        }
        if let Some(index) = header.iter().position(|cell| cell.contains(name))
            && !selected.contains(&index)
        {
            selected.push(index);
        }
    }
    for index in 0..header.len() {
        if selected.len() == MAX_TABLE_COLUMNS {
            break;
        }
        if !selected.contains(&index) {
            selected.push(index);
        }
    }
    selected
}

fn table_row(cells: &[&str], widths: &[usize], style: Style) -> Line<'static> {
    let mut spans = Vec::new();
    for (column, cell) in cells.iter().enumerate() {
        if column > 0 {
            spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
        }
        let width = widths[column];
        let fitted = fit_cell(cell, width);
        let cell_width = display_width(&fitted);
        spans.push(Span::styled(fitted, style));
        if cell_width < width {
            spans.push(Span::raw(" ".repeat(width - cell_width)));
        }
    }
    Line::from(spans)
}

fn fit_cell(cell: &str, width: usize) -> String {
    if display_width(cell) <= width {
        return (*cell).to_owned();
    }
    let limit = width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for character in cell.chars() {
        let character_width = display_width(&character.to_string());
        if used + character_width > limit {
            break;
        }
        out.push(character);
        used += character_width;
    }
    out.push('…');
    out
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
    fn wide_tables_keep_key_columns_and_report_the_rest() {
        let rendered = render(
            "| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |\n\
|---|---|---|---|---|---|---|---|\n\
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |",
            Color::Reset,
        );
        let flat = text(&rendered);
        assert!(flat.contains("任务"));
        assert!(flat.contains("认知属性"));
        assert!(flat.contains("执行状态"));
        assert!(flat.contains("用户角色"));
        assert!(!flat.contains("判定依据"));
        assert!(flat.contains("4 more column(s)"));
        assert!(flat.contains("plan artifact"));
    }

    #[test]
    fn long_cells_are_truncated_without_breaking_alignment() {
        let rendered = render(
            "| 任务 | 状态 |\n|---|---|\n| 这是一个非常非常非常非常非常长的任务名称 | 可执行 |",
            Color::Reset,
        );
        let flat = text(&rendered);
        assert!(flat.contains('…'), "{flat}");
        for line in &rendered {
            assert!(line.width() <= 40, "{line:?}");
        }
    }

    #[test]
    fn many_rows_are_summarized() {
        let mut table = String::from("| 任务 | 状态 |\n|---|---|\n");
        for index in 0..10 {
            table.push_str(&format!("| task {index} | ok |\n"));
        }
        let rendered = render(&table, Color::Reset);
        let flat = text(&rendered);
        assert!(flat.contains("4 more row(s)"), "{flat}");
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

    #[test]
    fn blockquotes_and_horizontal_rules_render_without_markers() {
        let rendered = render("> quoted\n\n---", Color::Reset);
        let flat = text(&rendered);
        assert!(flat.contains("quoted"));
        assert!(flat.contains("│"));
        assert!(flat.contains("─"));
        assert!(!flat.contains("> quoted"));
        assert!(!flat.contains("---"));
    }
}
