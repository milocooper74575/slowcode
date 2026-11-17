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
const TABLE_COLUMN_MIN_WIDTH: usize = 4;
const TABLE_COLUMN_GAP: &str = " │ ";
const TABLE_COLUMN_GAP_WIDTH: usize = 3;

/// Render one Markdown document into terminal lines.
#[must_use]
pub fn render(text: &str, base: Color, available: usize) -> Vec<Line<'static>> {
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
            let (table, consumed) = render_table(&lines[index..], base, available);
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

fn render_table(lines: &[&str], base: Color, available: usize) -> (Vec<Line<'static>>, usize) {
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
    let header_widths = header
        .iter()
        .map(|cell| visible_width(cell))
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
    let widths = table_column_widths(&header_widths, &rows, available);

    let mut out = Vec::new();
    push_table_row(
        &mut out,
        &header,
        &widths,
        Color::Cyan,
        Style::default().add_modifier(Modifier::BOLD),
    );
    let mut separator = Vec::new();
    for (column, width) in widths.iter().enumerate() {
        if column > 0 {
            separator.push(Span::styled("─┼─", Style::default().fg(Color::DarkGray)));
        }
        separator.push(Span::styled(
            "─".repeat(*width),
            Style::default().fg(Color::DarkGray),
        ));
    }
    out.push(Line::from(separator));
    for row in rows {
        push_table_row(&mut out, &row, &widths, base, Style::default());
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

/// Width of a cell after inline markers are interpreted.
fn visible_width(cell: &str) -> usize {
    inline_spans(cell, Color::Reset)
        .iter()
        .map(Span::width)
        .sum()
}

/// Pick a width per column that fits `available` without truncating text.
fn table_column_widths(header: &[usize], rows: &[Vec<&str>], available: usize) -> Vec<usize> {
    let count = header.len();
    if count == 0 {
        return Vec::new();
    }
    let mut natural = vec![TABLE_COLUMN_MIN_WIDTH; count];
    for (column, width) in header.iter().enumerate() {
        natural[column] = natural[column].max(*width);
    }
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            natural[column] = natural[column].max(visible_width(cell));
        }
    }
    let gaps = TABLE_COLUMN_GAP_WIDTH * count.saturating_sub(1);
    let budget = available.saturating_sub(gaps).max(1);
    if natural.iter().sum::<usize>() <= budget {
        return natural;
    }
    distribute_widths(&natural, budget)
}

/// Fill `budget` one column at a time, always serving the widest unmet need.
fn distribute_widths(natural: &[usize], budget: usize) -> Vec<usize> {
    let mut widths = vec![1usize; natural.len()];
    let mut remaining = budget.saturating_sub(widths.len());
    while remaining > 0 {
        let mut pick = None;
        for index in 0..natural.len() {
            if widths[index] >= natural[index] {
                continue;
            }
            if pick.is_none_or(|best: usize| {
                natural[index] - widths[index] > natural[best] - widths[best]
            }) {
                pick = Some(index);
            }
        }
        let Some(index) = pick else {
            break;
        };
        widths[index] += 1;
        remaining -= 1;
    }
    widths
}

/// Append every visual line of one logical table row.
fn push_table_row(
    out: &mut Vec<Line<'static>>,
    cells: &[&str],
    widths: &[usize],
    base: Color,
    line_style: Style,
) {
    let wrapped = cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| wrap_spans(&inline_spans(cell, base), *width))
        .collect::<Vec<_>>();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    for line_index in 0..height {
        let mut spans = Vec::new();
        for (column, width) in widths.iter().enumerate() {
            if column > 0 {
                spans.push(Span::styled(
                    TABLE_COLUMN_GAP,
                    Style::default().fg(Color::DarkGray),
                ));
            }
            let mut used = 0;
            if let Some(segments) = wrapped[column].get(line_index) {
                for span in segments {
                    used += span.width();
                    spans.push(span.clone());
                }
            }
            if used < *width {
                spans.push(Span::raw(" ".repeat(width - used)));
            }
        }
        out.push(Line::from(spans).style(line_style));
    }
}

/// Break styled text into chunks no wider than `width` display cells.
fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    for span in spans {
        let mut chunk = String::new();
        for character in span.content.chars() {
            let character_width = display_width(&character.to_string());
            if used > 0 && used + character_width > width {
                push_chunk(&mut lines, &mut chunk, span.style);
                lines.push(Vec::new());
                used = 0;
            }
            chunk.push(character);
            used += character_width;
        }
        push_chunk(&mut lines, &mut chunk, span.style);
    }
    lines
}

fn push_chunk(lines: &mut [Vec<Span<'static>>], chunk: &mut String, style: Style) {
    if chunk.is_empty() {
        return;
    }
    if let Some(line) = lines.last_mut() {
        line.push(Span::styled(std::mem::take(chunk), style));
    }
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

    const TEST_WIDTH: usize = 80;

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
        let rendered = render("# Title\n\n- first\n2. second", Color::Reset, TEST_WIDTH);
        let flat = text(&rendered);
        assert!(flat.contains("Title"));
        assert!(!flat.contains("# Title"));
        assert!(flat.contains("• first"));
        assert!(flat.contains("› second"));
    }

    #[test]
    fn fenced_code_keeps_its_content() {
        let rendered = render("```rust\nlet x = 1;\n```", Color::Reset, TEST_WIDTH);
        let flat = text(&rendered);
        assert!(flat.contains("rust"));
        assert!(flat.contains("let x = 1;"));
        assert!(!flat.contains("```"));
    }

    #[test]
    fn tables_render_cells_in_columns() {
        let rendered = render("| a | b |\n|---|---|\n| 1 | 2 |", Color::Reset, TEST_WIDTH);
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
            TEST_WIDTH,
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
            TEST_WIDTH,
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
            TEST_WIDTH,
        );
        let flat = text(&rendered);
        assert!(flat.contains("任务"));
        assert!(flat.contains("认知属性"));
        assert!(flat.contains("执行状态"));
        assert!(flat.contains("用户角色"));
        assert!(!flat.contains("判定依据"));
        assert!(flat.contains("4 more column(s)"));
        assert!(!flat.contains("plan artifact"));
    }

    #[test]
    fn narrow_tables_wrap_cells_without_losing_text() {
        let cell = "这是一个非常非常非常长的任务名称，需要在窄列里完整换行显示";
        let table = format!("| 任务 |\n|---|\n| {cell} |");
        let rendered = render(&table, Color::Reset, 20);
        let flat = text(&rendered);
        assert!(!flat.contains('…'), "{flat}");
        for line in &rendered {
            assert!(line.width() <= 20, "{} {line:?}", line.width());
        }
        let joined = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(joined.contains(cell), "{joined}");
    }

    #[test]
    fn table_uses_the_available_width_instead_of_a_fixed_cap() {
        let table = "| 项 | 状态 |\n|---|---|\n\
| `AGENTS.md` 新增 `## Independent judgment` | 已改，纯新增 10 行，英文，落点与 `Conventions`/`Plan workflow facts` 之间 |\n\
| 测试 | 无需，仅 Markdown 正文；不构成 `cargo test` 对象（`/test` 已记“无需测试”） |\n\
| workflow `plan-1794914179184-0` | `phase: test_fix`、`status: active` → 等你跑 `/workflow end`，`ended` 会保留 phase，不复用 `Done` |\n\
| git | **`AGENTS.md` 变更仍未提交**，工作区不再干净；本轮没有 shell 工具，我无法自己 commit |";
        let rendered = render(table, Color::Reset, 200);
        let flat = text(&rendered);
        assert!(!flat.contains('…'), "{flat}");
        assert!(flat.contains("Independent judgment"), "{flat}");
        assert!(flat.contains("Plan workflow facts"), "{flat}");
        assert!(flat.contains("不复用"), "{flat}");
        for line in &rendered {
            assert!(line.width() <= 200, "{} {line:?}", line.width());
        }
        assert!(
            rendered.iter().any(|line| line.width() > 100),
            "table still uses a narrow fixed cap: {rendered:?}"
        );
    }

    #[test]
    fn table_cells_drop_inline_markers() {
        let rendered = render(
            "| a | b |\n|---|---|\n| `code` | **bold** |",
            Color::Reset,
            TEST_WIDTH,
        );
        let flat = text(&rendered);
        assert!(flat.contains("code"), "{flat}");
        assert!(flat.contains("bold"), "{flat}");
        assert!(!flat.contains('`'), "{flat}");
        assert!(!flat.contains("**"), "{flat}");
    }

    #[test]
    fn many_rows_are_summarized() {
        let mut table = String::from("| 任务 | 状态 |\n|---|---|\n");
        for index in 0..10 {
            table.push_str(&format!("| task {index} | ok |\n"));
        }
        let rendered = render(&table, Color::Reset, TEST_WIDTH);
        let flat = text(&rendered);
        assert!(flat.contains("4 more row(s)"), "{flat}");
    }

    #[test]
    fn unclosed_fence_renders_the_remainder_as_code() {
        let rendered = render("```rust\nlet x = 1;\nlet y = 2;", Color::Reset, TEST_WIDTH);
        let flat = text(&rendered);
        assert!(flat.contains("let x = 1;"));
        assert!(flat.contains("let y = 2;"));
        assert!(!flat.contains("```"));
    }

    #[test]
    fn lone_inline_markers_do_not_panic_or_swallow_text() {
        let rendered = render("2 * 3 and a _ b and [not a link", Color::Reset, TEST_WIDTH);
        let flat = text(&rendered);
        assert!(flat.contains("2 * 3"));
        assert!(flat.contains("_ b"));
        assert!(flat.contains("[not a link"));
    }

    #[test]
    fn blockquotes_and_horizontal_rules_render_without_markers() {
        let rendered = render("> quoted\n\n---", Color::Reset, TEST_WIDTH);
        let flat = text(&rendered);
        assert!(flat.contains("quoted"));
        assert!(flat.contains("│"));
        assert!(flat.contains("─"));
        assert!(!flat.contains("> quoted"));
        assert!(!flat.contains("---"));
    }
}
