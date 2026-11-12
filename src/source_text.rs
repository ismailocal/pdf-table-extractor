use pdf_oxide::layout::{TextChar, Word};
use pdf_oxide::structure::table_extractor::Table;
use unicode_bidi::{BidiClass, BidiInfo, Level, bidi_class};

fn rtl_base(chars: impl Iterator<Item = char>) -> Option<Level> {
    let (mut rtl, mut ltr) = (0, 0);
    for c in chars {
        match bidi_class(c) {
            BidiClass::R | BidiClass::AL => rtl += 1,
            BidiClass::L => ltr += 1,
            _ => {}
        }
    }
    (rtl > 0).then(|| {
        if rtl >= ltr {
            Level::rtl()
        } else {
            Level::ltr()
        }
    })
}

pub(crate) fn glyph_right(glyph: &TextChar) -> f32 {
    glyph.origin_x + glyph.bbox.width
}

pub(crate) fn raised_digit(glyph: &TextChar, baseline: &TextChar) -> Option<u32> {
    if glyph.font_size < 0.8 * baseline.font_size
        && glyph.origin_y - baseline.origin_y > 0.15 * baseline.font_size
    {
        glyph.char.to_digit(10)
    } else {
        None
    }
}

pub(crate) fn glyph_lines(mut glyphs: Vec<&TextChar>) -> Vec<Vec<&TextChar>> {
    glyphs.retain(|c| !c.char.is_whitespace() && c.rotation_degrees.abs() < 1.0);
    glyphs.sort_by(|a, b| b.bbox.center().y.total_cmp(&a.bbox.center().y));
    let mut lines = Vec::new();
    let mut remaining = glyphs.as_slice();
    while let Some(first) = remaining.first() {
        let y = first.bbox.center().y;
        let end = remaining.partition_point(|c| y - c.bbox.center().y <= 3.5);
        let mut line = remaining[..end].to_vec();
        line.sort_by(|a, b| a.origin_x.total_cmp(&b.origin_x));
        lines.push(line);
        remaining = &remaining[end..];
    }
    lines
}

pub(crate) fn word_gap(left: &TextChar, right: &TextChar) -> bool {
    right.origin_x - glyph_right(left) > left.font_size.max(right.font_size) * 0.2
}

pub(crate) fn glyph_line_text(chars: &[&TextChar]) -> String {
    let mut text = String::new();
    let baseline = chars
        .iter()
        .max_by(|a, b| a.font_size.total_cmp(&b.font_size));
    for (index, c) in chars.iter().enumerate() {
        if index > 0 && word_gap(chars[index - 1], c) {
            text.push(' ');
        }
        if let Some(digit) = baseline.and_then(|base| raised_digit(c, base)) {
            text.push(['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'][digit as usize]);
        } else {
            text.push(c.char);
        }
    }
    text
}

pub(crate) fn visual_glyph_line_text(chars: &[&TextChar]) -> String {
    let text = glyph_line_text(chars);
    if !pdf_oxide::text::bidi::looks_rtl(&text) {
        return text;
    }
    let Some(base) = rtl_base(text.chars()) else {
        return text;
    };
    let bidi = BidiInfo::new(&text, Some(base));
    match bidi.paragraphs.first() {
        Some(line) => bidi.reorder_line(line, line.range.clone()).into_owned(),
        None => text,
    }
}

pub(crate) fn restore_glyph_spacing(words: &[&Word], source: &[TextChar]) -> Option<String> {
    let bounds = words
        .iter()
        .map(|word| word.bbox)
        .reduce(|a, b| a.union(&b))?;
    let glyphs = source
        .iter()
        .filter(|c| bounds.contains_point(&c.bbox.center()))
        .collect();
    let text = glyph_lines(glyphs)
        .iter()
        .map(|line| glyph_line_text(line))
        .collect::<Vec<_>>()
        .join("\n");
    // Geometry can correct inserted spaces only when the source characters
    // corroborate the complete cell, including the characters at both edges.
    text.chars()
        .filter(|c| !c.is_whitespace())
        .eq(words
            .iter()
            .flat_map(|word| word.text.chars())
            .filter(|c| !c.is_whitespace()))
        .then_some(text)
}

pub(crate) fn positioned_word_text(words: Vec<&Word>, source: &[TextChar]) -> String {
    // Keep ordinary word spacing unless a smaller, raised numeral could
    // move a baseline or change the order of text in this cell.
    let chars = words.iter().flat_map(|word| word.chars.iter());
    let max_font = chars.clone().map(|c| c.font_size).fold(0.0, f32::max);
    let lines = glyph_lines(
        chars
            .map(|c| {
                if c.char.is_ascii_digit() && c.font_size < 0.8 * max_font {
                    // The word API can move a raised glyph to the main baseline.
                    let delta = |raw: &TextChar| (raw.origin_y - c.origin_y).abs();
                    let start = source.partition_point(|raw| raw.origin_x < c.origin_x - 0.35);
                    return source[start..]
                        .iter()
                        .take_while(|raw| raw.origin_x <= c.origin_x + 0.35)
                        .filter(|raw| {
                            raw.char == c.char
                                && (raw.font_size - c.font_size).abs() <= 0.25
                                && delta(raw) <= 0.6 * max_font
                        })
                        .min_by(|a, b| delta(a).total_cmp(&delta(b)))
                        .unwrap_or(c);
                }
                c
            })
            .collect(),
    );
    if lines.iter().any(|line| {
        line.iter()
            .max_by(|a, b| a.font_size.total_cmp(&b.font_size))
            .is_some_and(|base| line.iter().any(|c| raised_digit(c, base).is_some()))
    }) && words
        .iter()
        .all(|word| word.chars.iter().map(|c| c.char).eq(word.text.chars()))
    {
        lines
            .iter()
            .map(|line| glyph_line_text(line))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        word_text(words)
    }
}

pub(crate) fn word_text(mut words: Vec<&Word>) -> String {
    words.sort_by(|a, b| b.bbox.center().y.total_cmp(&a.bbox.center().y));
    let mut lines = Vec::new();
    while let Some(first) = words.first() {
        let y = first.bbox.center().y;
        let count = words.partition_point(|word| y - word.bbox.center().y <= 2.5);
        let mut line: Vec<_> = words.drain(..count).collect();
        line.sort_by(|a, b| a.bbox.x.total_cmp(&b.bbox.x));
        lines.push(
            line.into_iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    lines.join("\n")
}

/// Recover the visual order from real glyph positions. The word extractor
/// may already reorder a mixed-direction span while keeping its advances
/// in visual order, so word positions alone cannot recover the source.
pub(crate) fn restore_rtl_cells<'a>(
    table: &mut Table,
    read_glyphs: impl FnOnce() -> Option<&'a [TextChar]>,
) {
    let mut cells = table
        .rows
        .iter_mut()
        .flat_map(|row| &mut row.cells)
        .filter_map(|cell| {
            cell.bbox
                .filter(|_| pdf_oxide::text::bidi::looks_rtl(&cell.text))
                .map(|bounds| (&mut cell.text, bounds))
        })
        .peekable();
    if cells.peek().is_none() {
        return;
    }
    let Some(glyphs) = read_glyphs() else { return };
    let sorted_chars = |value: &str| {
        let mut chars: Vec<_> = value.chars().filter(|ch| !ch.is_whitespace()).collect();
        chars.sort_unstable();
        chars
    };
    for (text, bounds) in cells {
        let lines = glyph_lines(
            glyphs
                .iter()
                .filter(|ch| bounds.contains_point(&ch.bbox.center()))
                .collect(),
        );
        let candidate = lines
            .iter()
            .map(|line| visual_glyph_line_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        // Reordering must preserve the decoded content and multiplicity.
        if sorted_chars(&candidate) == sorted_chars(text) {
            *text = candidate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_oxide::geometry::Rect;

    #[test]
    fn restores_logical_order_from_positioned_rtl_glyphs() {
        for (visual, logical) in [
            ("ABC 12.50 םולש", "שלום ABC 12.50"),
            ("١٬٢٥٠٫٧٥ ابحرم", "مرحبا ١٬٢٥٠٫٧٥"),
        ] {
            let chars: Vec<_> = visual
                .chars()
                .enumerate()
                .filter(|(_, c)| !c.is_whitespace())
                .map(|(i, char)| TextChar {
                    char,
                    bbox: Rect::new(i as f32 * 4.0, 0.0, 4.0, 8.0),
                    origin_x: i as f32 * 4.0,
                    ..TextChar::default()
                })
                .collect();
            assert_eq!(
                visual_glyph_line_text(&chars.iter().collect::<Vec<_>>()),
                logical
            );
        }
    }
}

fn isolated_glyph(neighbors: &[TextChar]) -> bool {
    neighbors
        .windows(2)
        .all(|pair| pair[1].bbox.x - pair[0].bbox.x > pair[0].font_size.max(pair[1].font_size))
}

fn glyphs_at<'a>(source: &'a [&TextChar], x: f32, y: f32) -> impl Iterator<Item = &'a TextChar> {
    const POSITION_TOLERANCE: f32 = 0.35;
    let start = source.partition_point(|ch| ch.origin_x < x - POSITION_TOLERANCE);
    source[start..]
        .iter()
        .copied()
        .take_while(move |ch| ch.origin_x <= x + POSITION_TOLERANCE)
        .filter(move |ch| (ch.origin_y - y).abs() <= POSITION_TOLERANCE)
}

// The word API can synthesize a glyph in a long jump between positioned
// runs. Require exact raw glyph corroboration for the rest of the token.
pub(crate) fn verify_spaced_glyphs<'a>(
    words: &mut [Word],
    read_glyphs: impl FnOnce() -> Result<&'a [TextChar], String>,
) -> Result<(), String> {
    let mut candidates = words
        .iter_mut()
        .filter(|word| {
            word.rotation_degrees.abs() < 1.0
                && word.chars.windows(3).any(isolated_glyph)
                && word.chars.iter().map(|ch| ch.char).eq(word.text.chars())
        })
        .peekable();
    if candidates.peek().is_none() {
        return Ok(());
    }
    let mut source: Vec<_> = read_glyphs()?.iter().collect();
    source.sort_by(|a, b| a.origin_x.total_cmp(&b.origin_x));
    for word in candidates {
        let mut missing = word.chars.iter().enumerate().filter(|(_, glyph)| {
            !glyphs_at(&source, glyph.bbox.x, glyph.bbox.y).any(|ch| ch.char == glyph.char)
        });
        let Some((index, glyph)) = missing.next() else {
            continue;
        };
        if missing.next().is_some()
            || index == 0
            || index + 1 == word.chars.len()
            || !isolated_glyph(&word.chars[index - 1..=index + 1])
        {
            continue;
        }
        // A different real glyph here may reflect ligatures or an explicit
        // Unicode mapping. Keep the word API's value in that case.
        let occupied =
            glyphs_at(&source, glyph.bbox.x, glyph.bbox.y).any(|ch| !ch.char.is_whitespace());
        if !occupied {
            word.chars.remove(index);
            word.text = word.chars.iter().map(|ch| ch.char).collect();
        }
    }
    Ok(())
}
