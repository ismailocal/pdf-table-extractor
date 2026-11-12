//! Read tables whose columns are marked by header backgrounds or text alignment.
use pdf_oxide::elements::{PathContent, PathOperation};
use pdf_oxide::geometry::Rect;
use pdf_oxide::layout::{TextChar, Word};
use pdf_oxide::structure::table_extractor::{Table, TableCell, TableRow};

use crate::source_text::{restore_glyph_spacing, word_text};

const CELL_GAP: f32 = 4.0;
const BASELINE_TOLERANCE: f32 = 3.5;
const COLUMN_ALIGNMENT_TOLERANCE: f32 = 3.0;
const MIN_ALIGNED_COLUMNS: usize = 3;
const MIN_ALIGNED_BODY_ROWS: usize = 3;

fn right(rect: Rect) -> f32 {
    rect.x + rect.width
}

fn top(rect: Rect) -> f32 {
    rect.y + rect.height
}

pub(crate) fn header_cells(paths: &[PathContent]) -> Vec<Rect> {
    let mut cells = Vec::new();
    for path in paths {
        let Some(fill) = &path.fill_color else {
            continue;
        };
        if fill.r.min(fill.g).min(fill.b) >= 0.97 {
            continue;
        }
        for op in &path.operations {
            if let PathOperation::Rectangle(x, y, width, height) = *op
                && width.abs() >= 16.0
                && (7.0..=64.0).contains(&height.abs())
            {
                cells.push(Rect::from_points(x, y, x + width, y + height));
            }
        }
    }

    // Some producers paint the same cell background twice, with a small inset.
    cells.sort_by(|a, b| (b.width * b.height).total_cmp(&(a.width * a.height)));
    let mut outer: Vec<Rect> = Vec::new();
    for cell in cells {
        if !outer.iter().any(|r| r.contains_rect(&cell)) {
            outer.push(cell);
        }
    }
    outer
}

fn column_words<'a>(cells: &[Rect], words: &[&'a Word]) -> Option<Vec<Vec<&'a Word>>> {
    let mut columns = vec![Vec::new(); cells.len()];
    for word in words {
        let index = cells.iter().position(|cell| {
            word.bbox.x >= cell.x - CELL_GAP / 2.0
                && right(word.bbox) <= right(*cell) + CELL_GAP / 2.0
        })?;
        columns[index].push(*word);
    }
    columns
        .iter()
        .all(|column| !column.is_empty())
        .then_some(columns)
}

fn table_row(columns: Vec<Vec<&Word>>, is_header: bool) -> TableRow {
    table_row_with_glyphs(columns, is_header, None)
}

fn table_row_with_glyphs(
    columns: Vec<Vec<&Word>>,
    is_header: bool,
    glyphs: Option<&[TextChar]>,
) -> TableRow {
    let cells = columns
        .into_iter()
        .map(|words| TableCell {
            text: glyphs
                .and_then(|glyphs| restore_glyph_spacing(&words, glyphs))
                .unwrap_or_else(|| word_text(words)),
            spans: Vec::new(),
            colspan: 1,
            rowspan: 1,
            mcids: Vec::new(),
            bbox: None,
            is_header,
        })
        .collect();
    TableRow { cells, is_header }
}

fn read_columns(cells: &[Rect], text: &PageText<'_>, occupied: &[Rect]) -> Option<Table> {
    let header = Rect::from_points(
        cells.first()?.x,
        cells[0].y,
        right(*cells.last()?),
        top(cells[0]),
    );
    if occupied.iter().any(|r| r.intersects(&header)) {
        return None;
    }
    let header_words: Vec<_> = text
        .words
        .iter()
        .filter(|word| {
            let point = word.bbox.center();
            (header.y..=top(header)).contains(&point.y)
                && (header.x..=right(header)).contains(&point.x)
        })
        .collect();
    let mut rows = vec![table_row(column_words(cells, &header_words)?, true)];

    // Use full page baselines so a paragraph stretching beyond the columns
    // cannot become a table row merely because some words lie inside it.
    let start = text
        .ordered
        .partition_point(|word| word.bbox.center().y >= header.y);
    let mut body = &text.ordered[start..];
    let mut previous = header.y;
    let mut bottom = header.y;
    let max_gap = (header.height * 1.8).clamp(20.0, 30.0);
    while let Some(first) = body.first() {
        let y = first.bbox.center().y;
        if previous - y > max_gap {
            break;
        }
        let end = body.partition_point(|word| y - word.bbox.center().y <= BASELINE_TOLERANCE);
        let Some(columns) = column_words(cells, &body[..end]) else {
            break;
        };
        bottom = body[..end].iter().map(|word| word.bbox.y).fold(y, f32::min);
        rows.push(table_row(columns, false));
        body = &body[end..];
        previous = y;
    }
    if rows.len() < 2 {
        return None;
    }
    let bbox = Rect::from_points(header.x, bottom, right(header), top(header));
    if occupied.iter().any(|r| r.intersects(&bbox)) {
        return None;
    }
    Some(Table {
        rows,
        has_header: true,
        col_count: cells.len(),
        bbox: Some(bbox),
    })
}

pub(crate) fn detect(fills: &[Rect], text: &PageText<'_>, occupied: &[Rect]) -> Vec<Table> {
    let mut bands: Vec<Vec<Rect>> = Vec::new();
    for &cell in fills {
        match bands.iter_mut().find(|band| {
            (cell.y - band[0].y).abs() <= 2.0 && (top(cell) - top(band[0])).abs() <= 2.0
        }) {
            Some(band) => band.push(cell),
            None => bands.push(vec![cell]),
        }
    }
    let mut tables = Vec::new();
    let mut occupied = occupied.to_vec();
    for mut band in bands {
        band.sort_by(|a, b| a.x.total_cmp(&b.x));
        for cells in band.chunk_by(|left, next| {
            let gap = next.x - right(*left);
            (-1.0..=CELL_GAP).contains(&gap)
        }) {
            if cells.len() >= 2
                && let Some(table) = read_columns(cells, text, &occupied)
            {
                occupied.extend(table.bbox);
                tables.push(table);
            }
        }
    }
    tables.extend(detect_alignment(&text.lines, &mut occupied));
    tables
}

pub(crate) struct PageText<'a> {
    pub(crate) words: &'a [Word],
    pub(crate) lines: Vec<Vec<&'a Word>>,
    ordered: Vec<&'a Word>,
}

impl<'a> PageText<'a> {
    pub(crate) fn new(words: &'a [Word]) -> Self {
        let mut ordered: Vec<_> = words.iter().collect();
        ordered.sort_by(|a, b| b.bbox.center().y.total_cmp(&a.bbox.center().y));
        let lines = group_lines(&ordered);
        Self {
            words,
            lines,
            ordered,
        }
    }

    pub(crate) fn lines_in(
        &self,
        bounds: Rect,
        include: impl Fn(&Word) -> bool,
    ) -> Vec<Vec<&'a Word>> {
        let start = self
            .ordered
            .partition_point(|w| w.bbox.center().y > top(bounds));
        let end = self
            .ordered
            .partition_point(|w| w.bbox.center().y >= bounds.y);
        // Group after filtering so words outside the region cannot set the baseline.
        let words: Vec<_> = self.ordered[start..end]
            .iter()
            .copied()
            .filter(|word| include(word))
            .collect();
        group_lines(&words)
    }
}

fn group_lines<'a>(mut remaining: &[&'a Word]) -> Vec<Vec<&'a Word>> {
    let mut lines = Vec::new();
    while let Some(first) = remaining.first() {
        let baseline = first.bbox.center().y;
        let end = remaining.partition_point(|w| baseline - w.bbox.center().y <= BASELINE_TOLERANCE);
        let mut line = remaining[..end].to_vec();
        line.sort_by(|a, b| a.bbox.x.total_cmp(&b.bbox.x));
        lines.push(line);
        remaining = &remaining[end..];
    }
    lines
}

// The header supplies the columns. Group words on each baseline before
// assigning them so a wrapped phrase can extend past a header divider.
pub(crate) fn read_framed_body(
    bounds: Rect,
    edges: &[f32],
    text: &PageText<'_>,
    snap: f32,
) -> Option<Vec<TableRow>> {
    let lines = text.lines_in(bounds, |word| {
        let point = word.bbox.center();
        bounds.y < point.y && (bounds.x..right(bounds)).contains(&point.x)
    });
    let mut rows: Vec<Vec<Vec<&Word>>> = Vec::new();
    let mut anchors = vec![Vec::<(f32, f32)>::new(); edges.len() - 1];
    let mut previous_y = 0.0;
    for line in &lines {
        let height = line[0].bbox.height;
        let y = line[0].bbox.center().y;
        let mut cells = vec![Vec::new(); edges.len() - 1];
        for phrase in line.chunk_by(|a, b| b.bbox.x - right(a.bbox) < height * 0.8) {
            let x = phrase[0].bbox.x + snap;
            let col = edges[1..edges.len() - 1].partition_point(|edge| *edge <= x);
            cells[col].extend_from_slice(phrase);
        }
        let mut populated = cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| !cell.is_empty());
        let (column, phrase) = populated.next()?;
        if populated.next().is_some() {
            for (anchors, cell) in anchors.iter_mut().zip(&cells) {
                if let (Some(first), Some(last)) = (cell.first(), cell.last()) {
                    anchors.push((first.bbox.x, right(last.bbox)));
                }
            }
            rows.push(cells);
        } else {
            let previous = &mut rows.last_mut()?[column];
            let same_start = previous
                .first()
                .is_some_and(|first| (first.bbox.x - phrase[0].bbox.x).abs() <= snap);
            if !same_start || previous_y - y > height * 1.8 {
                return None;
            }
            previous.extend_from_slice(phrase);
        }
        previous_y = y;
    }
    let stable_columns = anchors
        .iter()
        .filter(|column| {
            [true, false].into_iter().any(|left| {
                let mut edges: Vec<_> = column
                    .iter()
                    .map(|&(l, r)| if left { l } else { r })
                    .collect();
                edges.sort_by(f32::total_cmp);
                edges.windows(3).any(|sample| sample[2] - sample[0] <= snap)
            })
        })
        .take(2)
        .count();
    (stable_columns == 2).then(|| rows.into_iter().map(|row| table_row(row, false)).collect())
}

fn aligned_cells<'a>(anchors: &[f32], line: &[&'a Word]) -> Option<Vec<Vec<&'a Word>>> {
    if (line.first()?.bbox.x - anchors[0]).abs() > COLUMN_ALIGNMENT_TOLERANCE {
        return None;
    }
    let mut remaining = line;
    let mut cells = Vec::with_capacity(anchors.len());
    for &anchor in &anchors[1..] {
        let split = remaining.partition_point(|w| w.bbox.x < anchor - COLUMN_ALIGNMENT_TOLERANCE);
        if split == 0
            || (remaining.get(split)?.bbox.x - anchor).abs() > COLUMN_ALIGNMENT_TOLERANCE
            || right(remaining[split - 1].bbox) + COLUMN_ALIGNMENT_TOLERANCE >= anchor
        {
            return None;
        }
        cells.push(remaining[..split].to_vec());
        remaining = &remaining[split..];
    }
    cells.push(remaining.to_vec());
    Some(cells)
}

fn read_alignment(lines: &[Vec<&Word>], occupied: &[Rect]) -> Option<Table> {
    let header = lines.first()?;
    // Repeated numeric labels can be axis marks on a drawing. Require a
    // textual heading before treating the aligned numbers as a table.
    if !header
        .iter()
        .any(|word| word.text.chars().any(char::is_alphabetic))
    {
        return None;
    }
    let font_height = header[0].bbox.height;
    let starts: Vec<_> = header
        .chunk_by(|a, b| b.bbox.x - right(a.bbox) < 2.0 * font_height)
        .map(|group| group[0].bbox.x)
        .collect();
    if starts.len() < MIN_ALIGNED_COLUMNS {
        return None;
    }

    let mut rows = vec![aligned_cells(&starts, header)?];
    let mut right_edges: Vec<_> = rows[0]
        .iter()
        .map(|cell| {
            cell.iter()
                .map(|w| right(w.bbox))
                .fold(f32::NEG_INFINITY, f32::max)
        })
        .collect();
    let mut previous_y = header[0].bbox.center().y;
    let mut bottom = header[0].bbox.y;
    for line in &lines[1..] {
        let y = line[0].bbox.center().y;
        let gap = previous_y - y;
        if gap > 2.65 * font_height {
            break;
        }
        if let Some(cells) = aligned_cells(&starts, line) {
            for (edge, cell) in right_edges.iter_mut().zip(&cells) {
                *edge = cell.iter().map(|w| right(w.bbox)).fold(*edge, f32::max);
            }
            rows.push(cells);
        } else if rows.len() > 1 && gap <= 1.75 * font_height {
            // A wrapped cell can start to the left of its first line. The
            // preceding column's right edge still separates the two.
            let left = line[0].bbox.x;
            let right = line
                .iter()
                .map(|w| right(w.bbox))
                .fold(f32::NEG_INFINITY, f32::max);
            let column = (1..starts.len()).find(|&c| {
                let limit = starts
                    .get(c + 1)
                    .copied()
                    .unwrap_or(right_edges[c] + font_height);
                left >= right_edges[c - 1] + COLUMN_ALIGNMENT_TOLERANCE && right <= limit
            });
            let Some(column) = column else { break };
            rows.last_mut()?[column].extend(line);
            right_edges[column] = right_edges[column].max(right);
        } else {
            break;
        }
        previous_y = y;
        bottom = line.iter().map(|w| w.bbox.y).fold(y, f32::min);
    }
    if rows.len() <= MIN_ALIGNED_BODY_ROWS {
        return None;
    }

    let bbox = Rect::from_points(starts[0], bottom, *right_edges.last()?, top(header[0].bbox));
    if occupied.iter().any(|rect| rect.intersects(&bbox)) {
        return None;
    }
    Some(Table {
        rows: rows
            .into_iter()
            .enumerate()
            .map(|(i, row)| table_row(row, i == 0))
            .collect(),
        has_header: true,
        col_count: starts.len(),
        bbox: Some(bbox),
    })
}

fn detect_alignment(lines: &[Vec<&Word>], occupied: &mut Vec<Rect>) -> Vec<Table> {
    let mut tables = Vec::new();
    for start in 0..lines.len() {
        if let Some(table) = read_alignment(&lines[start..], occupied) {
            let table = read_sparse_alignment(&lines[start..], occupied, None)
                .filter(|extended| {
                    extended.col_count == table.col_count
                        && extended.rows.len() > table.rows.len()
                        && extended.rows.iter().zip(&table.rows).all(|(new, old)| {
                            new.cells
                                .iter()
                                .zip(&old.cells)
                                .all(|(a, b)| a.text == b.text)
                        })
                })
                .unwrap_or(table);
            occupied.extend(table.bbox);
            tables.push(table);
        }
    }
    tables
}

fn text_extent(words: &[&Word]) -> Option<(f32, f32)> {
    Some((
        words.first()?.bbox.x,
        words.iter().map(|word| right(word.bbox)).reduce(f32::max)?,
    ))
}

fn sectioned_gutter(
    lines: &[Vec<&Word>],
    headings: &[(f32, f32)],
    font_height: f32,
) -> Option<f32> {
    let [(label_left, label_right), (value_left, value_right)] = *headings else {
        return None;
    };
    let mut gap = (label_right, value_left);
    let mut records = 0;
    for pair in lines.windows(2) {
        let row = &pair[1];
        if pair[0][0].bbox.center().y - row[0].bbox.center().y > 2.65 * font_height
            || (row[0].bbox.x - label_left).abs() > COLUMN_ALIGNMENT_TOLERANCE
        {
            break;
        }
        let Some((_, row_right)) = text_extent(row) else {
            break;
        };
        if (row_right - value_right).abs() > font_height {
            continue;
        }
        if let Some(pair) = row.windows(2).rfind(|pair| {
            pair[1].bbox.x - right(pair[0].bbox) >= 2.0 * font_height
                && pair[1]
                    .text
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_numeric())
        }) {
            gap.0 = gap.0.max(right(pair[0].bbox));
            gap.1 = gap.1.min(pair[1].bbox.x);
            records += 1;
        }
    }
    (records >= MIN_ALIGNED_BODY_ROWS && gap.0 < gap.1).then(|| (gap.0 + gap.1) / 2.0)
}

fn read_sparse_alignment<'a>(
    lines: &[Vec<&Word>],
    occupied: &[Rect],
    section_glyphs: Option<&dyn Fn() -> Option<&'a [TextChar]>>,
) -> Option<Table> {
    let header = lines.first()?;
    let font_height = header[0].bbox.height;
    let header_cells: Vec<_> = header
        .chunk_by(|a, b| b.bbox.x - right(a.bbox) < 2.0 * font_height)
        .map(<[_]>::to_vec)
        .collect();
    if header_cells.len() < 2
        || header_cells.iter().any(|cell| {
            !cell
                .iter()
                .any(|word| word.text.chars().any(char::is_alphabetic))
        })
    {
        return None;
    }
    let header_edges: Vec<_> = header_cells
        .iter()
        .map(|cell| text_extent(cell).unwrap())
        .collect();
    let gutters: Vec<_> = if section_glyphs.is_some() {
        vec![sectioned_gutter(lines, &header_edges, font_height)?]
    } else {
        header_edges
            .windows(2)
            .map(|pair| (pair[0].1 + pair[1].0) / 2.0)
            .collect()
    };
    let mut rows = Vec::<Vec<Vec<&Word>>>::new();
    let mut starts = Vec::<Vec<Option<(f32, f32)>>>::new();
    let mut previous_y = header[0].bbox.center().y;
    let mut row_y = previous_y;
    let mut pitch = None;
    let mut multi_column_rows = 0;
    for line in &lines[1..] {
        let y = line[0].bbox.center().y;
        if previous_y - y > 2.65 * font_height || line[0].bbox.x < header_edges[0].0 - font_height {
            break;
        }
        let mut cells = vec![Vec::new(); header_cells.len()];
        let mut crosses = false;
        for word in line {
            let column = gutters.partition_point(|&x| x < word.bbox.center().x);
            crosses |= gutters
                .get(column)
                .is_some_and(|&x| right(word.bbox) > x + COLUMN_ALIGNMENT_TOLERANCE)
                || column
                    .checked_sub(1)
                    .is_some_and(|c| word.bbox.x < gutters[c] - COLUMN_ALIGNMENT_TOLERANCE);
            cells[column].push(*word);
        }
        if crosses {
            break;
        }
        let mut populated = cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| !cell.is_empty());
        let (column, cell) = populated.next()?;
        if populated.next().is_some() {
            pitch.get_or_insert(row_y - y);
            multi_column_rows += 1;
        } else if let Some(pitch) = pitch {
            let gap = row_y - y;
            if gap < pitch * 0.75 {
                let previous = &mut rows.last_mut()?[column];
                if previous
                    .first()
                    .is_none_or(|word| (word.bbox.x - cell[0].bbox.x).abs() > font_height)
                {
                    break;
                }
                previous.extend_from_slice(cell);
                previous_y = y;
                continue;
            }
            if (gap - pitch).abs() > pitch * 0.25 {
                break;
            }
        } else if section_glyphs.is_none()
            || column != 0
            || (cell[0].bbox.x - header_edges[0].0).abs() > COLUMN_ALIGNMENT_TOLERANCE
        {
            break;
        }
        starts.push(cells.iter().map(|cell| text_extent(cell)).collect());
        rows.push(cells);
        previous_y = y;
        row_y = y;
    }
    if rows.len() < 3 || multi_column_rows < 2 {
        return None;
    }

    // The body may align to a different edge than the header. Blanks do
    // not vote for an anchor. Two-column prose needs stronger evidence.
    let mut data_column = false;
    for (column, &heading) in header_edges.iter().enumerate() {
        let extents: Vec<_> = starts.iter().filter_map(|row| row[column]).collect();
        let anchor = *extents.first()?;
        let reference = if extents.len() == 1 { heading } else { anchor };
        let aligned = |edge: fn((f32, f32)) -> f32| {
            extents
                .iter()
                .all(|&value| (edge(value) - edge(reference)).abs() <= COLUMN_ALIGNMENT_TOLERANCE)
        };
        let left = aligned(|(l, _)| l);
        let right = aligned(|(_, r)| r);
        let center = aligned(|(l, r)| (l + r) / 2.0);
        if !(left || right || center) {
            return None;
        }
        data_column |= (right && !left)
            || (extents.len() >= 2
                && rows
                    .iter()
                    .map(|row| &row[column])
                    .filter(|cell| !cell.is_empty())
                    .all(|cell| cell[0].text.chars().any(char::is_numeric)));
    }
    if header_cells.len() == 2 && !data_column {
        return None;
    }
    let bbox = header
        .iter()
        .copied()
        .chain(rows.iter().flatten().flatten().copied())
        .map(|word| word.bbox)
        .reduce(|a, b| a.union(&b))?;
    if occupied.iter().any(|rect| rect.intersects(&bbox)) {
        return None;
    }
    let glyphs = section_glyphs.and_then(|read| read());
    Some(Table {
        bbox: Some(bbox),
        col_count: header_cells.len(),
        has_header: true,
        rows: std::iter::once(table_row_with_glyphs(header_cells, true, glyphs))
            .chain(
                rows.into_iter()
                    .map(|row| table_row_with_glyphs(row, false, glyphs)),
            )
            .collect(),
    })
}

pub(crate) fn detect_sparse<'a>(
    lines: &[Vec<&Word>],
    mut occupied: Vec<Rect>,
    read_glyphs: impl Fn() -> Option<&'a [TextChar]>,
) -> Vec<Table> {
    let mut tables = Vec::new();
    for start in 0..lines.len() {
        if lines[start].iter().any(|word| {
            occupied
                .iter()
                .any(|r| r.contains_point(&word.bbox.center()))
        }) {
            continue;
        }
        if let Some(table) = read_sparse_alignment(&lines[start..], &occupied, None)
            .or_else(|| read_sparse_alignment(&lines[start..], &occupied, Some(&read_glyphs)))
        {
            occupied.extend(table.bbox);
            tables.push(table);
        }
    }
    tables
}

// A grid without headings needs more consistent rows than an aligned
// table with a heading. Otherwise axis labels can look like a small table.
const MIN_NUMERIC_BLOCK_ROWS: usize = 6;

fn numeric_word(word: &Word) -> bool {
    word.text.chars().any(|c| c.is_ascii_digit())
        && word
            .text
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '−' | '.' | ',' | '%'))
}

fn numeric_block(lines: &[Vec<&Word>], left: f32, right_edge: f32) -> Option<Table> {
    let within = |word: &&Word| (left..right_edge).contains(&word.bbox.center().x);
    let first: Vec<_> = lines.first()?.iter().copied().filter(within).collect();
    if first.len() < MIN_ALIGNED_COLUMNS {
        return None;
    }
    let mut alignments = vec![(true, true); first.len()];
    let mut rows = Vec::new();
    let mut bounds = first[0].bbox;
    let mut previous_y = first[0].bbox.center().y;
    let mut pitch: Option<f32> = None;
    for line in lines {
        let cells: Vec<_> = line.iter().copied().filter(within).collect();
        if cells.len() != first.len() || cells.iter().any(|word| !numeric_word(word)) {
            break;
        }
        let y = cells[0].bbox.center().y;
        if !rows.is_empty() {
            let gap = previous_y - y;
            let expected = *pitch.get_or_insert(gap);
            if expected <= BASELINE_TOLERANCE
                || expected > 2.65 * first[0].bbox.height
                || (gap - expected).abs() > expected * 0.2
            {
                break;
            }
        }
        let mut next = alignments.clone();
        for ((left, end), (cell, first)) in next.iter_mut().zip(cells.iter().zip(&first)) {
            *left &= (cell.bbox.x - first.bbox.x).abs() <= COLUMN_ALIGNMENT_TOLERANCE;
            *end &= (right(cell.bbox) - right(first.bbox)).abs() <= COLUMN_ALIGNMENT_TOLERANCE;
        }
        if next.iter().any(|&(left, end)| !left && !end) {
            break;
        }
        alignments = next;
        for cell in &cells {
            bounds = bounds.union(&cell.bbox);
        }
        rows.push(TableRow {
            is_header: false,
            cells: cells
                .into_iter()
                .map(|w| TableCell::new(w.text.clone(), false))
                .collect(),
        });
        previous_y = y;
    }
    (rows.len() >= MIN_NUMERIC_BLOCK_ROWS).then_some(Table {
        rows,
        has_header: false,
        col_count: first.len(),
        bbox: Some(bounds),
    })
}

pub(crate) fn detect_numeric_blocks(lines: &[Vec<&Word>], mut occupied: Vec<Rect>) -> Vec<Table> {
    let mut tables = Vec::new();
    for (start, line) in lines.iter().enumerate() {
        if line.len() < MIN_ALIGNED_COLUMNS
            || line.iter().any(|word| {
                !numeric_word(word)
                    || occupied
                        .iter()
                        .any(|r| r.contains_point(&word.bbox.center()))
            })
        {
            continue;
        }
        let gaps: Vec<_> = line
            .windows(2)
            .map(|pair| pair[1].bbox.x - pair[0].bbox.x)
            .collect();
        let mut sorted_gaps = gaps.clone();
        sorted_gaps.sort_by(f32::total_cmp);
        let typical_gap = sorted_gaps[sorted_gaps.len() / 2];
        let split = gaps
            .iter()
            .enumerate()
            .filter(|&(i, gap)| {
                i + 1 >= MIN_ALIGNED_COLUMNS
                    && line.len() - i - 1 >= MIN_ALIGNED_COLUMNS
                    && *gap > 2.25 * typical_gap
            })
            .max_by(|a, b| a.1.total_cmp(b.1));
        let adjacent = split.and_then(|(i, _)| {
            let cut = (right(line[i].bbox) + line[i + 1].bbox.x) / 2.0;
            let left = numeric_block(&lines[start..], f32::NEG_INFINITY, cut)?;
            let right = numeric_block(&lines[start..], cut, f32::INFINITY)?;
            // A wider gutter alone can also be an internal column gap.
            // Require a different vertical extent before splitting.
            (left.rows.len().abs_diff(right.rows.len()) >= 2).then_some(vec![left, right])
        });
        let candidates = adjacent.unwrap_or_else(|| {
            numeric_block(&lines[start..], f32::NEG_INFINITY, f32::INFINITY)
                .into_iter()
                .collect()
        });
        for table in candidates {
            let bounds = table.bbox.expect("numeric block bounds");
            if !occupied.iter().any(|r| r.intersects(&bounds)) {
                occupied.push(bounds);
                tables.push(table);
            }
        }
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, x: f32, y: f32) -> Word {
        Word {
            chars: Vec::new(),
            bbox: Rect::new(x, y, text.chars().count() as f32 * 5.0, 12.0),
            text: text.into(),
            avg_font_size: 12.0,
            dominant_font: String::new(),
            is_bold: false,
            is_italic: false,
            mcid: None,
            sequence: 0,
            rotation_degrees: 0.0,
        }
    }

    fn aligned_words(header: [&str; 3]) -> Vec<Word> {
        let mut words: Vec<_> = header
            .into_iter()
            .zip([100.0, 250.0, 400.0])
            .map(|(text, x)| word(text, x, 720.0))
            .collect();
        for (name, y) in [("1", 694.0), ("2", 668.0), ("3", 642.0)] {
            words.extend([
                word(name, 100.0, y),
                word("PM", 250.0, y),
                word("Filtre", 400.0, y),
            ]);
        }
        words
    }

    #[test]
    fn keeps_wrapped_value_in_its_aligned_cell() {
        let mut words = aligned_words(["Kaynak", "Kirletici", "Kontrol"]);
        words.push(word("(%99,9)", 375.0, 652.0));
        words.push(word("Sonraki", 100.0, 615.0));
        let tables = detect_alignment(&PageText::new(&words).lines, &mut Vec::new());
        let [table] = tables.as_slice() else {
            panic!("expected one table")
        };
        assert_eq!((table.rows.len(), table.col_count), (4, 3));
        assert_eq!(table.rows[2].cells[2].text, "Filtre\n(%99,9)");
        assert_eq!(table.rows[3].cells[0].text, "3");
    }

    #[test]
    fn rejects_a_grid_of_numeric_labels() {
        let words = aligned_words(["25", "5", "25"]);
        assert!(detect_alignment(&PageText::new(&words).lines, &mut Vec::new()).is_empty());
    }

    #[test]
    fn keeps_explicit_slots_when_untagged_rows_omit_values() {
        let mut words = aligned_words(["Item", "Quantity", "Amount"]);
        words.retain(|w| !(w.bbox.y == 668.0 && w.bbox.x == 400.0));
        let tables = detect_sparse(&PageText::new(&words).lines, Vec::new(), || None);
        let [table] = tables.as_slice() else {
            panic!("expected one table")
        };
        assert_eq!((table.rows.len(), table.col_count), (4, 3));
        assert_eq!(table.rows[2].cells[1].text, "PM");
        assert_eq!(table.rows[2].cells[2].text, "");
        assert_eq!(table.rows[2].cells[2].rowspan, 1);
    }

    #[test]
    fn keeps_a_sparse_last_record_after_fully_aligned_rows() {
        let mut words = aligned_words(["Item", "Quantity", "Amount"]);
        words.extend([word("4", 100.0, 616.0), word("PM", 250.0, 616.0)]);
        let tables = detect_alignment(&PageText::new(&words).lines, &mut Vec::new());
        let [table] = tables.as_slice() else {
            panic!("expected one extended table")
        };
        assert_eq!(table.rows.len(), 5);
        assert_eq!(table.rows[4].cells[0].text, "4");
        assert_eq!(table.rows[4].cells[2].text, "");
    }

    #[test]
    fn aligns_right_justified_values_and_rejects_parallel_prose() {
        let mut words = vec![word("Item", 80.0, 720.0), word("Amount", 350.0, 720.0)];
        for (i, value) in ["5,00", "150,00", "28,00"].into_iter().enumerate() {
            let y = 696.0 - 24.0 * i as f32;
            words.push(word("Example", 80.0, y));
            words.push(word(value, 400.0 - 5.0 * value.len() as f32, y));
        }
        let tables = detect_sparse(&PageText::new(&words).lines, Vec::new(), || None);
        let [table] = tables.as_slice() else {
            panic!("expected right-aligned table")
        };
        assert_eq!(table.rows[2].cells[1].text, "150,00");

        let mut prose = vec![word("Heading", 80.0, 720.0), word("Heading", 350.0, 720.0)];
        for (i, text) in ["Open", "Parallel", "Longer heading"]
            .into_iter()
            .enumerate()
        {
            let y = 696.0 - 24.0 * i as f32;
            prose.push(word(text, 80.0, y));
            prose.push(word(text, 350.0, y));
        }
        assert!(detect_sparse(&PageText::new(&prose).lines, Vec::new(), || None).is_empty());
    }

    #[test]
    fn keeps_section_labels_before_the_first_aligned_value() {
        let mut words = vec![
            word("Item", 50.0, 720.0),
            word("Amount", 360.0, 720.0),
            word("Equipment", 50.0, 700.0),
        ];
        for (row, (label, value)) in [
            ("A long label with extra details", "8.00"),
            ("A longer label with more item details", "105.00"),
            ("Another label", "24.00"),
        ]
        .into_iter()
        .enumerate()
        {
            let y = 680.0 - 20.0 * row as f32;
            words.push(word(label, 50.0, y));
            words.push(word(value, 390.0 - 5.0 * value.len() as f32, y));
        }
        words.push(word("Notes", 50.0, 587.0));

        let tables = detect_sparse(&PageText::new(&words).lines, Vec::new(), || None);
        let [table] = tables.as_slice() else {
            panic!("expected one sectioned table")
        };
        assert_eq!(table.rows.len(), 5);
        assert_eq!(table.rows[1].cells[0].text, "Equipment");
        assert_eq!(table.rows[1].cells[1].text, "");
        assert_eq!(
            table.rows[3].cells[0].text,
            "A longer label with more item details"
        );
        assert_eq!(table.rows[3].cells[1].text, "105.00");

        // A value column needs repeated numeric rows with a common right edge.
        words[6].bbox.x += 35.0;
        assert!(detect_sparse(&PageText::new(&words).lines, Vec::new(), || None).is_empty());
    }

    #[test]
    fn separates_adjacent_numeric_blocks_only_with_different_extents() {
        let block = |left: f32, count: usize| {
            (0..count).flat_map(move |row| {
                [0.0, 40.0, 80.0]
                    .into_iter()
                    .enumerate()
                    .map(move |(col, offset)| {
                        word(
                            &(row * 3 + col).to_string(),
                            left + offset,
                            720.0 - row as f32 * 18.0,
                        )
                    })
            })
        };
        let words: Vec<_> = block(30.0, 9).chain(block(250.0, 7)).collect();
        let tables = detect_numeric_blocks(&PageText::new(&words).lines, Vec::new());
        assert_eq!(
            tables
                .iter()
                .map(|t| (t.rows.len(), t.col_count))
                .collect::<Vec<_>>(),
            [(9, 3), (7, 3)]
        );
        assert!(tables.iter().all(|t| !t.has_header));
        assert_eq!(tables[1].rows[6].cells[2].text, "20");

        let words: Vec<_> = block(30.0, 9).chain(block(250.0, 9)).collect();
        let tables = detect_numeric_blocks(&PageText::new(&words).lines, Vec::new());
        assert_eq!(tables.len(), 1);
        assert_eq!((tables[0].rows.len(), tables[0].col_count), (9, 6));

        let labels: Vec<_> = block(30.0, 4).collect();
        assert!(detect_numeric_blocks(&PageText::new(&labels).lines, Vec::new()).is_empty());
    }
}
