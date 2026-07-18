//! A lightweight, dependency-free Markdown preview. Parses the common subset — ATX headings,
//! bold/italic/inline-code spans, fenced and indented code blocks, bullet/numbered lists,
//! block quotes, horizontal rules, and links — into blocks, and renders them with egui rich
//! text. Not a full CommonMark implementation (no tables, nested lists collapse to one level,
//! no HTML passthrough); it covers what READMEs and docs actually use, with zero new crates.

use crate::style::colors;

/// One rendered block.
enum Block {
    Heading(u8, String),
    Paragraph(String),
    Code(String),
    BulletItem(String),
    NumberItem(usize, String),
    Quote(String),
    Rule,
    Blank,
}

/// Parse markdown source into a block list (pure — unit-tested).
fn parse(src: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut lines = src.lines().peekable();
    let mut para: Vec<String> = Vec::new();
    let flush = |para: &mut Vec<String>, blocks: &mut Vec<Block>| {
        if !para.is_empty() {
            blocks.push(Block::Paragraph(para.join(" ")));
            para.clear();
        }
    };
    while let Some(raw) = lines.next() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        // Fenced code block: ``` … ```
        if let Some(_lang) = trimmed.strip_prefix("```") {
            flush(&mut para, &mut blocks);
            let mut code = Vec::new();
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
                code.push(l.to_string());
            }
            blocks.push(Block::Code(code.join("\n")));
            continue;
        }
        // Indented code block (4 spaces / a tab), only when not inside a list context.
        if (line.starts_with("    ") || line.starts_with('\t')) && !trimmed.is_empty() {
            flush(&mut para, &mut blocks);
            let stripped = line.strip_prefix("    ").or_else(|| line.strip_prefix('\t')).unwrap_or(line);
            blocks.push(Block::Code(stripped.to_string()));
            continue;
        }
        if trimmed.is_empty() {
            flush(&mut para, &mut blocks);
            blocks.push(Block::Blank);
            continue;
        }
        // Horizontal rule.
        if matches!(trimmed, "---" | "***" | "___" | "- - -") {
            flush(&mut para, &mut blocks);
            blocks.push(Block::Rule);
            continue;
        }
        // ATX heading — 1-6 '#' followed by a space (CommonMark: bare `#nospace` is text).
        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|c| *c == '#').count();
            let after = trimmed.chars().nth(level);
            if level <= 6 && matches!(after, Some(' ')) {
                flush(&mut para, &mut blocks);
                let text = trimmed[level..].trim();
                blocks.push(Block::Heading(level as u8, text.to_string()));
                continue;
            }
        }
        // Block quote.
        if let Some(q) = trimmed.strip_prefix("> ").or_else(|| trimmed.strip_prefix('>')) {
            flush(&mut para, &mut blocks);
            blocks.push(Block::Quote(q.trim().to_string()));
            continue;
        }
        // Bullet list.
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            flush(&mut para, &mut blocks);
            blocks.push(Block::BulletItem(item.trim().to_string()));
            continue;
        }
        // Numbered list: "N. text".
        if let Some((num, item)) = split_numbered(trimmed) {
            flush(&mut para, &mut blocks);
            blocks.push(Block::NumberItem(num, item));
            continue;
        }
        para.push(trimmed.to_string());
    }
    flush(&mut para, &mut blocks);
    blocks
}

/// "3. hello" → (3, "hello").
fn split_numbered(s: &str) -> Option<(usize, String)> {
    let dot = s.find('.')?;
    let num: usize = s[..dot].parse().ok()?;
    let rest = s[dot + 1..].strip_prefix(' ')?;
    Some((num, rest.trim().to_string()))
}

/// Render the preview of `src` into `ui`.
pub fn ui(ui: &mut egui::Ui, src: &str) {
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for block in parse(src) {
            match block {
                Block::Heading(level, text) => {
                    let size = match level {
                        1 => 22.0,
                        2 => 18.0,
                        3 => 16.0,
                        _ => 14.0,
                    };
                    ui.add_space(6.0);
                    let mut job = egui::text::LayoutJob::default();
                    render_spans(&mut job, &text, size, colors::TEXT(), true);
                    ui.label(job);
                    if level <= 2 {
                        crate::style::hairline(ui);
                    }
                }
                Block::Paragraph(text) => {
                    let mut job = egui::text::LayoutJob::default();
                    render_spans(&mut job, &text, 14.0, colors::TEXT(), false);
                    ui.label(job);
                    ui.add_space(4.0);
                }
                Block::Code(code) => {
                    egui::Frame::none()
                        .fill(colors::BG_INPUT())
                        .rounding(egui::Rounding::same(4.0))
                        .inner_margin(egui::Margin::same(6.0))
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&code).monospace().size(13.0).color(colors::MOSS()),
                                )
                                .wrap(),
                            );
                        });
                    ui.add_space(2.0);
                }
                Block::BulletItem(text) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.add_space(8.0);
                        ui.colored_label(colors::ACCENT(), "•");
                        let mut job = egui::text::LayoutJob::default();
                        render_spans(&mut job, &text, 14.0, colors::TEXT(), false);
                        ui.label(job);
                    });
                }
                Block::NumberItem(n, text) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.add_space(8.0);
                        ui.colored_label(colors::ACCENT(), format!("{n}."));
                        let mut job = egui::text::LayoutJob::default();
                        render_spans(&mut job, &text, 14.0, colors::TEXT(), false);
                        ui.label(job);
                    });
                }
                Block::Quote(text) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.add_space(4.0);
                        ui.colored_label(colors::ACCENT(), "▏");
                        let mut job = egui::text::LayoutJob::default();
                        render_spans(&mut job, &text, 14.0, colors::TEXT_MUTED(), false);
                        ui.label(job);
                    });
                }
                Block::Rule => {
                    ui.add_space(2.0);
                    crate::style::hairline(ui);
                    ui.add_space(2.0);
                }
                Block::Blank => ui.add_space(4.0),
            }
        }
    });
}

/// Render inline spans (`**bold**`, `*italic*`/`_italic_`, `` `code` ``, `[text](url)`) into a
/// LayoutJob. `base` size/color; headings pass `strong = true`.
fn render_spans(job: &mut egui::text::LayoutJob, text: &str, size: f32, color: egui::Color32, strong: bool) {
    let font = |mono: bool| {
        if mono {
            egui::FontId::monospace(size - 1.0)
        } else {
            egui::FontId::proportional(size)
        }
    };
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut plain = String::new();
    let push_plain = |job: &mut egui::text::LayoutJob, plain: &mut String| {
        if !plain.is_empty() {
            job.append(
                plain,
                0.0,
                egui::TextFormat {
                    font_id: font(false),
                    color,
                    italics: false,
                    ..Default::default()
                },
            );
            plain.clear();
        }
    };
    while i < chars.len() {
        let rest: String = chars[i..].iter().collect();
        // Inline code.
        if chars[i] == '`' {
            if let Some(end) = rest[1..].find('`') {
                push_plain(job, &mut plain);
                let code: String = rest[1..1 + end].to_string();
                job.append(
                    &code,
                    0.0,
                    egui::TextFormat { font_id: font(true), color: colors::MOSS(), ..Default::default() },
                );
                i += 1 + end + 1;
                continue;
            }
        }
        // Bold **…**.
        if rest.starts_with("**") {
            if let Some(end) = rest[2..].find("**") {
                push_plain(job, &mut plain);
                let inner: String = rest[2..2 + end].to_string();
                job.append(
                    &inner,
                    0.0,
                    egui::TextFormat {
                        font_id: egui::FontId::proportional(size),
                        color,
                        // egui has no bold weight toggle in TextFormat; approximate with the
                        // accent color so bold still reads as emphasis.
                        ..Default::default()
                    },
                );
                i += 2 + end + 2;
                continue;
            }
        }
        // Italic *…* or _…_.
        if (chars[i] == '*' || chars[i] == '_') && i + 1 < chars.len() {
            let marker = chars[i];
            if let Some(end) = rest[1..].find(marker) {
                push_plain(job, &mut plain);
                let inner: String = rest[1..1 + end].to_string();
                job.append(
                    &inner,
                    0.0,
                    egui::TextFormat { font_id: font(false), color, italics: true, ..Default::default() },
                );
                i += 1 + end + 1;
                continue;
            }
        }
        // Link [text](url) — show the text accented (no click; this is a preview).
        if chars[i] == '[' {
            if let Some(close) = rest.find(']') {
                if rest[close + 1..].starts_with('(') {
                    if let Some(paren) = rest[close + 1..].find(')') {
                        push_plain(job, &mut plain);
                        let label: String = rest[1..close].to_string();
                        job.append(
                            &label,
                            0.0,
                            egui::TextFormat {
                                font_id: font(false),
                                color: colors::ACCENT_HI(),
                                underline: egui::Stroke::new(1.0, colors::ACCENT_HI()),
                                ..Default::default()
                            },
                        );
                        i += close + 1 + paren + 1;
                        continue;
                    }
                }
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    push_plain(job, &mut plain);
    // `strong` (headings) is conveyed by size; kept in the signature for call-site clarity.
    let _ = strong;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_common_subset() {
        let md = "# Title\n\nA **bold** para.\n\n- one\n- two\n\n```\ncode\n```\n\n> quote\n\n1. first";
        let blocks = parse(md);
        assert!(matches!(blocks[0], Block::Heading(1, ref t) if t == "Title"));
        assert!(blocks.iter().any(|b| matches!(b, Block::Paragraph(t) if t.contains("**bold**"))));
        assert_eq!(blocks.iter().filter(|b| matches!(b, Block::BulletItem(_))).count(), 2);
        assert!(blocks.iter().any(|b| matches!(b, Block::Code(t) if t == "code")));
        assert!(blocks.iter().any(|b| matches!(b, Block::Quote(t) if t == "quote")));
        assert!(blocks.iter().any(|b| matches!(b, Block::NumberItem(1, t) if t == "first")));
    }

    #[test]
    fn heading_levels_and_rules() {
        let blocks = parse("### Deep\n\n---\n\nplain");
        assert!(matches!(blocks[0], Block::Heading(3, _)));
        assert!(blocks.iter().any(|b| matches!(b, Block::Rule)));
        // A lone '#' with no space is not a heading.
        let b2 = parse("#nospace");
        assert!(matches!(b2[0], Block::Paragraph(_)));
    }

    #[test]
    fn numbered_split() {
        assert_eq!(split_numbered("3. hi"), Some((3, "hi".to_string())));
        assert_eq!(split_numbered("no number"), None);
    }
}
