use pdf_oxide::PdfDocument;
use pdf_oxide::elements::{PathContent, PathOperation};
use pdf_oxide::geometry::Rect;
use pdf_oxide::layout::{TextChar, Word};
use std::error::Error;

pub(crate) fn normalize_quarter_turn<'a>(
    pdf: &PdfDocument,
    page: usize,
    rotation: i32,
    paths: &mut [PathContent],
    words: &mut Vec<Word>,
    bounds: &mut Option<Rect>,
    read_glyphs: impl FnOnce() -> Result<&'a [TextChar], String>,
) -> Result<Option<Vec<TextChar>>, Box<dyn Error>> {
    if !matches!(rotation, 90 | 270) {
        return Ok(None);
    }
    let horizontal_after_turn = |angle: f32| {
        let delta = (angle - rotation as f32).rem_euclid(360.0);
        delta <= 5.0 || delta >= 355.0
    };
    // A /Rotate entry alone is insufficient: some PDFs store an already
    // horizontal table in raw coordinates. Turn the paths only when the
    // dominant text direction also becomes horizontal in the display frame.
    if words
        .iter()
        .filter(|word| horizontal_after_turn(word.rotation_degrees))
        .count()
        <= words.len() / 2
    {
        return Ok(None);
    }
    let (x0, y0, x1, y1) = pdf.get_page_media_box(page)?;
    let point = |x: f32, y: f32| {
        if rotation == 90 {
            (x0 + y - y0, y0 + x1 - x)
        } else {
            (x0 + y1 - y, y0 + x - x0)
        }
    };
    let rect = |rect: Rect| {
        let (ax, ay) = point(rect.x, rect.y);
        let (bx, by) = point(rect.x + rect.width, rect.y + rect.height);
        Rect::from_points(ax, ay, bx, by)
    };
    let mut chars: Vec<_> = read_glyphs()?
        .iter()
        .filter(|ch| horizontal_after_turn(ch.rotation_degrees))
        .cloned()
        .collect();
    for ch in &mut chars {
        let (x, y) = point(ch.origin_x, ch.origin_y);
        (ch.origin_x, ch.origin_y) = (x, y);
        // In the raw frame the horizontal advance of vertical glyphs is
        // zero. After rotation their true origins determine token spacing.
        ch.bbox = Rect::new(x, y, ch.font_size * 0.5, ch.bbox.height);
        ch.rotation_degrees = 0.0;
    }
    chars.sort_by(|a, b| b.origin_y.total_cmp(&a.origin_y));
    let mut remaining = chars.as_mut_slice();
    let mut upright = Vec::new();
    while let Some(first) = remaining.first() {
        let baseline = first.origin_y;
        let count = remaining.partition_point(|ch| baseline - ch.origin_y <= 0.5);
        let (line, rest) = remaining.split_at_mut(count);
        line.sort_by(|a, b| a.origin_x.total_cmp(&b.origin_x));
        for run in line.chunk_by(|a, b| b.origin_x - a.origin_x <= a.font_size.max(b.font_size)) {
            upright.extend(
                run.split(|ch| ch.char.is_whitespace())
                    .filter(|token| !token.is_empty())
                    .map(|token| Word::from_chars(token.to_vec())),
            );
        }
        remaining = rest;
    }
    if upright.is_empty() {
        return Ok(None);
    }
    for path in paths {
        path.bbox = rect(path.bbox);
        for op in &mut path.operations {
            *op = match *op {
                PathOperation::MoveTo(x, y) => {
                    let (x, y) = point(x, y);
                    PathOperation::MoveTo(x, y)
                }
                PathOperation::LineTo(x, y) => {
                    let (x, y) = point(x, y);
                    PathOperation::LineTo(x, y)
                }
                PathOperation::CurveTo(x1, y1, x2, y2, x3, y3) => {
                    let (x1, y1) = point(x1, y1);
                    let (x2, y2) = point(x2, y2);
                    let (x3, y3) = point(x3, y3);
                    PathOperation::CurveTo(x1, y1, x2, y2, x3, y3)
                }
                PathOperation::Rectangle(x, y, w, h) => {
                    let r = rect(Rect::new(x, y, w, h));
                    PathOperation::Rectangle(r.x, r.y, r.width, r.height)
                }
                PathOperation::ClosePath => PathOperation::ClosePath,
            };
        }
    }
    *words = upright;
    *bounds = bounds.map(rect);
    Ok(Some(chars))
}
