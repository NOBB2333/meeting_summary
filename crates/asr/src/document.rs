//! Deterministic, native DOCX export. Markdown is data, never executable code.
use anyhow::{Context, Result};
use docx_rs::*;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::{io::Cursor, path::Path};

const PAGE_WIDTH: usize = 9026; // A4 minus 2.5 cm margins.
#[derive(Default, Clone)]
struct Span {
    text: String,
    bold: bool,
    italic: bool,
}
#[derive(Default)]
struct TextBlock {
    spans: Vec<Span>,
    heading: Option<HeadingLevel>,
    prefix: String,
}
enum Block {
    Text(TextBlock),
    Table(Vec<Vec<Vec<Span>>>),
}
fn flush_text(block: &mut TextBlock, blocks: &mut Vec<Block>) {
    if !block.spans.is_empty() {
        blocks.push(Block::Text(std::mem::take(block)));
    }
}
fn parse(markdown: &str) -> Vec<Block> {
    let trimmed = markdown.trim();
    let markdown = ["```markdown\n", "```md\n", "```\n"]
        .iter()
        .find_map(|prefix| {
            trimmed
                .strip_prefix(prefix)
                .and_then(|s| s.strip_suffix("```"))
        })
        .unwrap_or(markdown);
    let mut blocks = Vec::new();
    let mut text = TextBlock::default();
    let mut table: Option<Vec<Vec<Vec<Span>>>> = None;
    let mut row = Vec::new();
    let mut bold = 0usize;
    let mut italic = 0usize;
    let mut lists: Vec<Option<u64>> = Vec::new();
    for event in Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    ) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                flush_text(&mut text, &mut blocks);
                text.heading = Some(level);
            }
            Event::Start(Tag::Table(_)) => {
                flush_text(&mut text, &mut blocks);
                table = Some(Vec::new());
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => {
                row.clear();
            }
            Event::Start(Tag::TableCell) => {
                text = TextBlock::default();
            }
            Event::End(TagEnd::TableCell) => {
                row.push(std::mem::take(&mut text.spans));
            }
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                if let Some(t) = &mut table {
                    t.push(std::mem::take(&mut row));
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(t) = table.take() {
                    blocks.push(Block::Table(t));
                }
            }
            Event::Start(Tag::Strong) => bold += 1,
            Event::End(TagEnd::Strong) => bold = bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => italic += 1,
            Event::End(TagEnd::Emphasis) => italic = italic.saturating_sub(1),
            Event::Start(Tag::List(first)) => {
                flush_text(&mut text, &mut blocks);
                lists.push(first);
            }
            Event::End(TagEnd::List(_)) => {
                flush_text(&mut text, &mut blocks);
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                flush_text(&mut text, &mut blocks);
                text.prefix = lists
                    .last()
                    .copied()
                    .flatten()
                    .map_or("• ".into(), |n| format!("{n}. "));
            }
            Event::End(TagEnd::Item) => {
                flush_text(&mut text, &mut blocks);
                if let Some(Some(n)) = lists.last_mut() {
                    *n += 1;
                }
            }
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::CodeBlock)
                if table.is_none() =>
            {
                flush_text(&mut text, &mut blocks)
            }
            Event::Text(s) | Event::Code(s) | Event::Html(s) | Event::InlineHtml(s) => {
                text.spans.push(Span {
                    text: s.to_string(),
                    bold: bold > 0,
                    italic: italic > 0,
                })
            }
            Event::SoftBreak => text.spans.push(Span {
                text: " ".into(),
                ..Default::default()
            }),
            Event::HardBreak => text.spans.push(Span {
                text: "\n".into(),
                ..Default::default()
            }),
            Event::TaskListMarker(checked) => text.spans.push(Span {
                text: if checked { "[x] " } else { "[ ] " }.into(),
                ..Default::default()
            }),
            _ => (),
        }
    }
    flush_text(&mut text, &mut blocks);
    blocks
}
fn fonts() -> RunFonts {
    RunFonts::new()
        .ascii("Times New Roman")
        .hi_ansi("Times New Roman")
        .east_asia("宋体")
}
fn runs(mut paragraph: Paragraph, spans: &[Span], force_bold: bool) -> Paragraph {
    for s in spans {
        for (i, line) in s.text.split('\n').enumerate() {
            if i > 0 {
                paragraph = paragraph.add_run(Run::new().add_break(BreakType::TextWrapping));
            }
            let mut run = Run::new().add_text(line).color("000000");
            if s.bold || force_bold {
                run = run.bold();
            }
            if s.italic {
                run = run.italic();
            }
            paragraph = paragraph.add_run(run);
        }
    }
    paragraph
}
fn table_doc(rows: Vec<Vec<Vec<Span>>>) -> Table {
    let count = rows.first().map_or(1, Vec::len).max(1);
    let weights: Vec<usize> = (0..count)
        .map(|i| {
            let header = rows
                .first()
                .and_then(|r| r.get(i))
                .map(|c| c.iter().map(|s| s.text.as_str()).collect::<String>())
                .unwrap_or_default();
            if ["事项", "内容", "议题", "讨论", "说明"]
                .iter()
                .any(|word| header.contains(word))
            {
                3
            } else {
                2
            }
        })
        .collect();
    let total: usize = weights.iter().sum();
    let mut widths: Vec<usize> = weights.iter().map(|w| PAGE_WIDTH * w / total).collect();
    let sum: usize = widths.iter().sum();
    *widths.last_mut().unwrap() += PAGE_WIDTH - sum;
    let mut output = Vec::new();
    for (r, row) in rows.into_iter().enumerate() {
        let mut cells = Vec::new();
        for (c, width) in widths.iter().enumerate() {
            let content = row.get(c).cloned().unwrap_or_default();
            let p = runs(
                Paragraph::new()
                    .line_spacing(LineSpacing::new().line(312).after(40))
                    .align(if weights[c] > 2 {
                        AlignmentType::Left
                    } else {
                        AlignmentType::Center
                    }),
                &content,
                r == 0,
            );
            let mut cell = TableCell::new()
                .width(*width, WidthType::Dxa)
                .vertical_align(VAlignType::Center)
                .add_paragraph(p);
            if r == 0 {
                cell = cell.shading(Shading::new().fill("F1F5F9"));
            }
            cells.push(cell);
        }
        output.push(TableRow::new(cells).cant_split());
    }
    Table::new(output)
        .set_grid(widths)
        .width(PAGE_WIDTH, WidthType::Dxa)
        .layout(TableLayoutType::Fixed)
        .margins(TableCellMargins::new().margin(120, 140, 120, 140))
}
/// The receiver only needs a DOCX reader (Word/WPS/Pages), no runtime or model.
pub fn minutes_docx(markdown: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(!markdown.trim().is_empty(), "没有可导出的纪要");
    anyhow::ensure!(markdown.len() <= 4_000_000, "纪要过大，请分篇导出");
    let mut doc = Docx::new()
        .page_size(11906, 16838)
        .page_margin(
            PageMargin::new()
                .top(1440)
                .bottom(1440)
                .left(1440)
                .right(1440),
        )
        .default_fonts(fonts())
        .default_size(24)
        .default_line_spacing(LineSpacing::new().line(312).after(120))
        .add_style(
            Style::new("Normal", StyleType::Paragraph)
                .name("Normal")
                .size(24)
                .fonts(fonts())
                .color("000000"),
        )
        .add_style(
            Style::new("Title", StyleType::Paragraph)
                .name("Title")
                .size(36)
                .fonts(fonts())
                .bold()
                .color("000000")
                .align(AlignmentType::Center)
                .line_spacing(LineSpacing::new().before(80).after(300)),
        );
    for level in 1..=6 {
        doc = doc.add_style(
            Style::new(format!("Heading{level}"), StyleType::Paragraph)
                .name(format!("heading {level}"))
                .size(if level == 1 { 28 } else { 26 })
                .fonts(fonts())
                .bold()
                .color("000000")
                .outline_lvl(level - 1)
                .line_spacing(LineSpacing::new().line(312).before(240).after(120)),
        );
    }
    let mut first_heading = true;
    for block in parse(markdown) {
        match block {
            Block::Text(block) => {
                let mut p = Paragraph::new();
                if let Some(h) = block.heading {
                    let level = h as usize;
                    let style = if first_heading && h == HeadingLevel::H1 {
                        "Title".into()
                    } else {
                        format!("Heading{}", level.saturating_sub(1).max(1))
                    };
                    p = p.style(&style).keep_next(true).keep_lines(true);
                    first_heading = false;
                } else if !block.prefix.is_empty() {
                    p = p
                        .align(AlignmentType::Left)
                        .add_run(Run::new().add_text(&block.prefix));
                }
                doc = doc.add_paragraph(runs(p, &block.spans, false));
            }
            Block::Table(rows) => {
                doc = doc
                    .add_table(table_doc(rows))
                    .add_paragraph(Paragraph::new().line_spacing(LineSpacing::new().after(120)));
            }
        }
    }
    let mut bytes = Cursor::new(Vec::new());
    let mut package = doc.build();
    // docx-rs currently lacks a repeat-header setter. First rows have trPr
    // because cant_split is set above; add the standard OOXML header marker.
    let xml = String::from_utf8(package.document)?;
    let mut parts = xml.split("<w:tbl>");
    let mut xml = parts.next().unwrap_or_default().to_string();
    for part in parts {
        xml.push_str("<w:tbl>");
        xml.push_str(&part.replacen("<w:trPr>", "<w:trPr><w:tblHeader/>", 1));
    }
    package.document = xml.into_bytes();
    package.pack(&mut bytes).context("Word 文档打包失败")?;
    Ok(bytes.into_inner())
}
pub fn export_minutes_docx(markdown: &str, path: &Path) -> Result<()> {
    let bytes = minutes_docx(markdown)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::atomic_write(path, &bytes)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transcript_word_preserves_literal_markup_and_speaker_attribution() {
        let original = "[方案](https://example.com) **不能上线** <script>，预算 2.5 万元。";
        let transcript = crate::Transcript {
            complete: true,
            segments: vec![crate::Segment {
                id: 7,
                revision: 1,
                start_ms: 1200,
                end_ms: 2400,
                text: original.into(),
                finalized: true,
            }],
            speaker_turns: vec![crate::SpeakerTurn {
                start_ms: 1200,
                end_ms: 2400,
                speaker: 2,
            }],
            ..Default::default()
        };
        let text = parse(&transcript.document_markdown())
            .into_iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.spans.into_iter().map(|s| s.text).collect::<String>()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(original));
        assert!(text.contains("说话人 2 · 00:00:01.200 · 段7"));
        assert!(!text.contains("说话人 1"));
    }
    #[test]
    fn parses_tables_and_inline_marks_without_losing_chinese_or_negation() {
        let blocks = parse(
            "# 会议纪要\n\n**本周不能上线**，预算 2 万元。\n\n| 事项 | 负责人 |\n| --- | --- |\n| 音频验收 | 未明确 |",
        );
        assert_eq!(blocks.len(), 3);
        let Block::Table(rows) = &blocks[2] else {
            panic!("missing table");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][1][0].text, "未明确");
        let bytes = minutes_docx("# 会议纪要\n\n本周不能上线，预算 2 万元。").unwrap();
        assert_eq!(&bytes[..2], b"PK");
    }
    #[test]
    fn markdown_fence_is_formatting_not_a_script_to_execute() {
        assert!(
            matches!(&parse("```markdown\n# 会议纪要\n```")[0], Block::Text(t) if t.heading == Some(HeadingLevel::H1))
        );
        assert!(
            matches!(&parse("```python\nprint('hello')\n```")[0], Block::Text(t) if t.heading.is_none())
        );
    }
}
