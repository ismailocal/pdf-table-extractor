//! Tables with repeated horizontal rules and text-aligned columns.
use crate::source_text::{
    glyph_lines, glyph_right as right, raised_digit, visual_glyph_line_text, word_gap,
};
use pdf_oxide::geometry::Rect;
use pdf_oxide::layout::TextChar;
use pdf_oxide::structure::table_extractor::{Table, TableCell, TableRow};

const EDGE_TOLERANCE: f32 = 2.0;

pub(crate) fn overlapping_fragments(tables: &[Table], bounds: Rect) -> Option<Vec<usize>> {
    let mut fragments = Vec::new();
    for (index, table) in tables.iter().enumerate() {
        let Some(area) = table.bbox.filter(|area| area.intersects(&bounds)) else {
            continue;
        };
        // Keep cells with source coordinates from a physical grid or PDF tags.
        if !bounds.contains_rect(&area)
            || table.rows.is_empty()
            || table
                .rows
                .iter()
                .flat_map(|row| &row.cells)
                .any(|cell| cell.bbox.is_some())
        {
            return None;
        }
        fragments.push(index);
    }
    Some(fragments)
}

pub(crate) fn select_table(
    tables: &mut Vec<Table>,
    candidate: Table,
    checked_bounds: Rect,
    mut fragments: Vec<usize>,
) {
    let Some(bounds) = candidate.bbox else {
        return;
    };
    if bounds != checked_bounds {
        // A heading above the first rule can extend the candidate.
        let Some(updated) = overlapping_fragments(tables, bounds) else {
            return;
        };
        fragments = updated;
    }
    if fragments.is_empty() {
        tables.push(candidate);
        return;
    }
    if candidate.rows.len() <= fragments.iter().map(|&i| tables[i].rows.len()).sum()
        || fragments
            .iter()
            .any(|&i| tables[i].col_count != candidate.col_count)
    {
        return;
    }
    fragments.sort_by(|&a, &b| {
        let top = |index: usize| {
            let area = tables[index].bbox.expect("overlapping table bounds");
            area.y + area.height
        };
        top(b).total_cmp(&top(a))
    });
    let mut remaining = candidate.rows.as_slice();
    for &index in &fragments {
        let rows = &tables[index].rows;
        // Match whole rows in order. The provisional heading of an isolated
        // fragment can be a data row in the complete table.
        let Some(offset) = remaining.windows(rows.len()).position(|window| {
            window.iter().zip(rows).all(|(new, old)| {
                new.cells.len() == old.cells.len()
                    && new.cells.iter().zip(&old.cells).all(|(a, b)| {
                        a.rowspan == b.rowspan
                            && a.colspan == b.colspan
                            && a.text.split_whitespace().eq(b.text.split_whitespace())
                    })
            })
        }) else {
            return;
        };
        remaining = &remaining[offset + rows.len()..];
    }
    fragments.sort_unstable();
    for index in fragments.into_iter().rev() {
        tables.remove(index);
    }
    tables.push(candidate);
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn table(rows: &[&str], bounds: Rect) -> Table {
        Table {
            rows: rows
                .iter()
                .map(|value| TableRow {
                    is_header: false,
                    cells: vec![
                        TableCell::new((*value).into(), false),
                        TableCell::new("100".into(), false),
                    ],
                })
                .collect(),
            col_count: 2,
            has_header: false,
            bbox: Some(bounds),
        }
    }

    fn consider(tables: &mut Vec<Table>, candidate: Table) {
        let bounds = candidate.bbox.unwrap();
        if let Some(fragments) = overlapping_fragments(tables, bounds) {
            select_table(tables, candidate, bounds, fragments);
        }
    }

    #[test]
    fn chooses_the_complete_table_without_losing_overlapping_rows() {
        let full = table(
            &["A", "B", "C", "D", "E"],
            Rect::new(10.0, 0.0, 80.0, 100.0),
        );
        let upper = table(&["B", "C"], Rect::new(12.0, 50.0, 76.0, 40.0));
        let lower = table(&["E"], Rect::new(12.0, 5.0, 76.0, 20.0));
        let mut tables = vec![lower.clone(), upper.clone()];
        consider(&mut tables, full.clone());
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].rows.len(), 5);

        let mut changed = full.clone();
        changed.rows[2].cells.swap(0, 1);
        let mut tables = vec![upper.clone(), lower.clone()];
        consider(&mut tables, changed);
        assert_eq!(tables.len(), 2);

        let mut protected = upper.clone();
        protected.rows[0].cells[0].bbox = Some(Rect::new(12.0, 75.0, 30.0, 15.0));
        let mut tables = vec![protected, lower.clone()];
        consider(&mut tables, full.clone());
        assert_eq!(tables.len(), 2);

        let mut merged = upper;
        merged.rows[0].cells[0].rowspan = 2;
        let mut tables = vec![merged, lower];
        consider(&mut tables, full);
        assert_eq!(tables.len(), 2);
    }
}

fn numeric_layout(lines: &[Vec<&TextChar>]) -> Option<Vec<(f32, f32)>> {
    if lines.len() < 3 {
        return None;
    }
    let mut samples = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        for phrase in
            line.chunk_by(|a, b| b.origin_x - right(a) <= 0.75 * a.font_size.max(b.font_size))
        {
            if phrase[0].char.is_numeric()
                || matches!(phrase[0].char, '-' | '–' | '—' | '+' | '(')
                    && phrase.iter().all(|c| !c.char.is_alphabetic())
            {
                let start = phrase[0];
                let baseline_end = phrase.iter().rfind(|c| raised_digit(c, start).is_none())?;
                samples.push((
                    right(baseline_end),
                    row,
                    start.origin_x,
                    right(phrase.last()?),
                ));
            }
        }
    }
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut remaining = samples.as_slice();
    let mut columns = Vec::new();
    while let Some(&(anchor, _, _, _)) = remaining.first() {
        let end = remaining.partition_point(|&(x, _, _, _)| x - anchor <= EDGE_TOLERANCE);
        let (group, rest) = remaining.split_at(end);
        let mut rows: Vec<_> = group.iter().map(|&(_, row, _, _)| row).collect();
        rows.sort_unstable();
        rows.dedup();
        if rows.len() >= 3 && rows.len() * 2 >= lines.len() {
            columns.push((
                group.iter().map(|&(_, _, left, _)| left).reduce(f32::min)?,
                group
                    .iter()
                    .map(|&(_, _, _, right)| right)
                    .reduce(f32::max)?,
            ));
        }
        remaining = rest;
    }
    if columns.len() < 2 {
        return None;
    }
    let first_value = columns[0].0;
    let label_end = lines
        .iter()
        .flat_map(|line| line.iter())
        .filter(|c| c.origin_x < first_value && right(c) < first_value)
        .map(|c| right(c))
        .reduce(f32::max)?;
    Some(
        std::iter::once((label_end, first_value))
            .chain(columns.windows(2).map(|pair| (pair[0].1, pair[1].0)))
            .collect(),
    )
}

fn column_cut(gap: (f32, f32), header_words: &[(f32, f32)]) -> Option<f32> {
    let (left, right) = gap;
    if left >= right {
        return None;
    }
    let preferred = left + (right - left).min(6.0) / 2.0;
    // A gap between values can still cross a wider heading. Move the cut
    // within the data gap to keep that word intact.
    let overlaps: Vec<_> = header_words
        .iter()
        .copied()
        .filter(|&(start, end)| start < right && end > left)
        .collect();
    std::iter::once(preferred)
        .chain(overlaps.iter().flat_map(|&(start, end)| [start, end]))
        .filter(|&x| {
            (left..=right).contains(&x)
                && !overlaps.iter().any(|&(start, end)| start < x && x < end)
        })
        .min_by(|a, b| (a - preferred).abs().total_cmp(&(b - preferred).abs()))
}

pub(crate) fn read_table(bounds: Rect, rules: &[f32], glyphs: &[TextChar]) -> Option<Table> {
    let lines = glyph_lines(
        glyphs
            .iter()
            .filter(|c| bounds.contains_point(&c.bbox.center()))
            .collect(),
    );
    let mut bands = Vec::new();
    let mut remaining = lines.as_slice();
    for pair in rules.windows(2) {
        let count = remaining.partition_point(|line| line[0].bbox.center().y > pair[1]);
        let (band, rest) = remaining.split_at(count);
        bands.push(band);
        remaining = rest;
    }
    read_numeric_table(bounds, &bands).or_else(|| read_record_bands(bounds, &bands, glyphs))
}

fn line_columns<'a>(line: &[&'a TextChar], cuts: &[f32]) -> Vec<Vec<&'a TextChar>> {
    let mut cells = vec![Vec::new(); cuts.len() + 1];
    for &glyph in line {
        let column = cuts.partition_point(|&edge| edge <= glyph.bbox.center().x);
        cells[column].push(glyph);
    }
    cells
}

fn read_numeric_table(bounds: Rect, bands: &[&[Vec<&TextChar>]]) -> Option<Table> {
    let (body, gaps) = bands
        .iter()
        .enumerate()
        .skip(1)
        .filter_map(|(i, band)| numeric_layout(band).map(|gaps| (i, gaps)))
        .max_by_key(|(i, _)| bands[*i].len())?;
    let header_words: Vec<_> = bands[..body]
        .iter()
        .flat_map(|band| band.iter())
        .flat_map(|line| line.chunk_by(|a, b| !word_gap(a, b)))
        .map(|word| (word[0].origin_x, right(word[word.len() - 1])))
        .collect();
    let cuts = gaps
        .into_iter()
        .map(|gap| column_cut(gap, &header_words))
        .collect::<Option<Vec<_>>>()?;
    let mut table = Table::new();
    table.bbox = Some(bounds);
    table.col_count = cuts.len() + 1;
    for (i, &band) in bands.iter().enumerate() {
        if band.is_empty() {
            return None;
        }
        let mut headers = vec![String::new(); table.col_count];
        for line in band {
            let cells = line_columns(line, &cuts);
            let cells: Vec<_> = cells
                .iter()
                .map(|cell| visual_glyph_line_text(cell))
                .collect();
            if i < body {
                for (header, value) in headers.iter_mut().zip(cells) {
                    if !value.is_empty() {
                        if !header.is_empty() {
                            header.push('\n');
                        }
                        header.push_str(&value);
                    }
                }
            } else {
                if cells[0].is_empty() || cells[1..].iter().all(|cell| cell.is_empty()) {
                    return None;
                }
                table.rows.push(TableRow {
                    is_header: false,
                    cells: cells
                        .into_iter()
                        .map(|cell| TableCell::new(cell, false))
                        .collect(),
                });
            }
        }
        if i < body {
            if headers
                .iter()
                .filter(|text| !text.is_empty())
                .take(2)
                .count()
                < 2
            {
                return None;
            }
            table.rows.push(TableRow {
                is_header: true,
                cells: headers
                    .into_iter()
                    .map(|cell| TableCell::new(cell, true))
                    .collect(),
            });
        }
    }
    if !table.rows[0]
        .cells
        .iter()
        .any(|cell| cell.text.chars().any(char::is_alphabetic))
    {
        return None;
    }
    table.has_header = true;
    Some(table)
}

fn read_record_bands(
    bounds: Rect,
    bands: &[&[Vec<&TextChar>]],
    glyphs: &[TextChar],
) -> Option<Table> {
    if bands.len() < 3 {
        return None;
    }
    // Learn column starts on the first baseline of each record so wrapped
    // lines inside a band cannot create extra columns.
    let mut starts = Vec::new();
    for (row, band) in bands.iter().enumerate() {
        for run in band
            .first()?
            .chunk_by(|a, b| b.origin_x - right(a) < 0.75 * a.font_size.max(b.font_size))
        {
            starts.push((run[0].origin_x, row));
        }
    }
    starts.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut remaining = starts.as_slice();
    let mut anchors = Vec::new();
    while let Some(&(x, _)) = remaining.first() {
        let count = remaining.partition_point(|&(other, _)| other - x <= EDGE_TOLERANCE);
        let (group, rest) = remaining.split_at(count);
        let mut rows: Vec<_> = group.iter().map(|&(_, row)| row).collect();
        rows.sort_unstable();
        rows.dedup();
        if rows.len() >= 3 && rows.len() * 2 >= bands.len() {
            anchors.push(x);
        }
        remaining = rest;
    }
    if anchors.len() < 2 {
        return None;
    }
    let cuts: Vec<_> = anchors.iter().skip(1).map(|x| x - EDGE_TOLERANCE).collect();
    let read_row = |band: &[Vec<&TextChar>], is_header| {
        let mut cells = vec![String::new(); anchors.len()];
        for line in band {
            if line.windows(2).any(|pair| {
                !word_gap(pair[0], pair[1])
                    && cuts
                        .iter()
                        .any(|&x| pair[0].bbox.center().x < x && x <= pair[1].bbox.center().x)
            }) {
                return None;
            }
            for ((text, column), anchor) in cells
                .iter_mut()
                .zip(line_columns(line, &cuts))
                .zip(&anchors)
            {
                let Some(first) = column.first() else {
                    continue;
                };
                if (first.origin_x - anchor).abs() > EDGE_TOLERANCE {
                    return None;
                }
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&visual_glyph_line_text(&column));
            }
        }
        Some(TableRow {
            is_header,
            cells: cells
                .into_iter()
                .map(|text| TableCell::new(text, is_header))
                .collect(),
        })
    };
    let mut rows = bands
        .iter()
        .map(|band| read_row(band, false))
        .collect::<Option<Vec<_>>>()?;
    if rows
        .iter()
        .filter(|row| {
            row.cells
                .iter()
                .filter(|cell| !cell.text.is_empty())
                .count()
                >= 2
        })
        .take(3)
        .count()
        < 3
    {
        return None;
    }
    let top = bounds.y + bounds.height;
    let font = bands[0][0][0].font_size;
    let above = glyph_lines(
        glyphs
            .iter()
            .filter(|c| {
                let point = c.bbox.center();
                (bounds.x..=bounds.x + bounds.width).contains(&point.x)
                    && point.y > top
                    && point.y <= top + 2.0 * font
            })
            .collect(),
    );
    let header = above.last().and_then(|line| {
        let row = read_row(std::slice::from_ref(line), true)?;
        row.cells
            .iter()
            .all(|c| c.text.chars().any(char::is_alphabetic))
            .then_some(row)
    });
    if anchors.len() == 2 && header.is_none() {
        return None;
    }
    let mut bounds = bounds;
    let has_header = header.is_some();
    if let Some(row) = header {
        rows.insert(0, row);
        let header_top = above
            .last()?
            .iter()
            .map(|c| c.bbox.y + c.bbox.height)
            .reduce(f32::max)?;
        bounds.height = header_top - bounds.y;
    }
    Some(Table {
        rows,
        has_header,
        col_count: anchors.len(),
        bbox: Some(bounds),
    })
}
