//! Find and reconstruct ruled tables from PDF paths and words.
use crate::borderless::PageText;
use crate::source_text::{positioned_word_text, restore_rtl_cells, word_text};
use pdf_oxide::elements::{PathContent, PathOperation};
use pdf_oxide::geometry::Rect;
use pdf_oxide::layout::{TextChar, Word};
use pdf_oxide::structure::table_extractor::{Table, TableCell, TableRow};

const MAX_RULE_WIDTH: f32 = 2.5;
const MIN_RULE_LENGTH: f32 = 4.0;
const SNAP: f32 = 2.0;
const DOUBLE_RULE_SNAP: f32 = 3.5;
const MIN_DOUBLE_RULE_LENGTH: f32 = 2.5;
const PAGE_EDGE_BAND: f32 = 0.15;
const AXIS_TOLERANCE: f32 = 0.25;
const WORD_ANGLE_TOLERANCE: f32 = 5.0;
const ROW_PITCH_TOLERANCE: f32 = 0.25;

const FULL_PAGE: [f32; 4] = [
    f32::NEG_INFINITY,
    f32::NEG_INFINITY,
    f32::INFINITY,
    f32::INFINITY,
];

#[derive(Clone, Copy)]
struct Segment {
    coord: f32,
    start: f32,
    end: f32,
}

#[derive(Clone)]
struct Rule {
    coord: f32,
    intervals: Vec<(f32, f32)>,
}

impl Rule {
    fn covers(&self, a: f32, b: f32, snap: f32) -> bool {
        let mut reached = a.min(b);
        for &(start, end) in &self.intervals {
            if start > reached + snap {
                break;
            }
            reached = reached.max(end);
        }
        reached + snap >= a.max(b)
    }
}

fn snap_rules(mut segments: Vec<Segment>, snap: f32) -> Vec<Rule> {
    segments.sort_by(|a, b| a.coord.total_cmp(&b.coord));
    let mut rules = Vec::new();
    let mut remaining = segments.as_slice();
    while let Some(first) = remaining.first() {
        let count = remaining.partition_point(|s| s.coord <= first.coord + snap);
        let (group, rest) = remaining.split_at(count);
        remaining = rest;
        let mut intervals = Vec::with_capacity(count);
        let mut sum = 0.0;
        for segment in group {
            sum += segment.coord;
            intervals.push((segment.start, segment.end));
        }
        intervals.sort_by(|a, b| a.0.total_cmp(&b.0));
        rules.push(Rule {
            coord: sum / count as f32,
            intervals,
        });
    }
    rules
}

fn extract_segments(
    paths: &[PathContent],
    [left, bottom, right, top]: [f32; 4],
    min_length: f32,
) -> (Vec<Segment>, Vec<Segment>) {
    let mut horizontal = Vec::new();
    let mut vertical = Vec::new();
    for path in paths {
        // PDF producers often paint several thin rectangles in one path.
        // Use the centre of each strip, not both edges of the rectangle.
        let rectangles = path
            .operations
            .chunk_by(|_, next| {
                !matches!(
                    next,
                    PathOperation::MoveTo(..) | PathOperation::Rectangle(..)
                )
            })
            .filter_map(|ops| match ops[0] {
                PathOperation::Rectangle(x, y, width, height) => Some((x, y, width, height)),
                _ => rectangular_strip(ops),
            });
        for (x, y, width, height) in rectangles {
            let (x0, x1) = (x.min(x + width), x.max(x + width));
            let (y0, y1) = (y.min(y + height), y.max(y + height));
            let cx = (x0 + x1) / 2.0;
            let cy = (y0 + y1) / 2.0;
            if (bottom..=top).contains(&cy)
                && y1 - y0 <= MAX_RULE_WIDTH
                && x1.min(right) - x0.max(left) >= min_length
            {
                horizontal.push(Segment {
                    coord: cy,
                    start: x0.max(left),
                    end: x1.min(right),
                });
            }
            if (left..=right).contains(&cx)
                && x1 - x0 <= MAX_RULE_WIDTH
                && y1.min(top) - y0.max(bottom) >= min_length
            {
                vertical.push(Segment {
                    coord: cx,
                    start: y0.max(bottom),
                    end: y1.min(top),
                });
            }
        }
    }
    (horizontal, vertical)
}

fn rectangular_strip(operations: &[PathOperation]) -> Option<(f32, f32, f32, f32)> {
    use PathOperation::{ClosePath, LineTo, MoveTo};
    let [
        MoveTo(x0, y0),
        LineTo(x1, y1),
        LineTo(x2, y2),
        LineTo(x3, y3),
        tail @ ..,
    ] = operations
    else {
        return None;
    };
    if !matches!(tail, [] | [ClosePath])
        && !matches!(tail, [LineTo(x, y)] | [LineTo(x, y), ClosePath] if x == x0 && y == y0)
    {
        return None;
    }
    let width = (x2 - x0).abs();
    let height = (y2 - y0).abs();
    if width.min(height) > MAX_RULE_WIDTH || width.min(height) <= 0.0 {
        return None;
    }
    let points = [(*x0, *y0), (*x1, *y1), (*x2, *y2), (*x3, *y3)];
    points
        .into_iter()
        .zip(points.into_iter().cycle().skip(1))
        .all(|((ax, ay), (bx, by))| (ax == bx) != (ay == by))
        .then_some((x0.min(*x2), y0.min(*y2), width, height))
}

fn stroke_segments(paths: &[PathContent]) -> (Vec<Segment>, Vec<Segment>) {
    let mut horizontal = Vec::new();
    let mut vertical = Vec::new();
    let mut add = |(x0, y0): (f32, f32), (x1, y1): (f32, f32)| {
        if (y1 - y0).abs() <= AXIS_TOLERANCE && (x1 - x0).abs() >= MIN_RULE_LENGTH {
            horizontal.push(Segment {
                coord: (y0 + y1) / 2.0,
                start: x0.min(x1),
                end: x0.max(x1),
            });
        }
        if (x1 - x0).abs() <= AXIS_TOLERANCE && (y1 - y0).abs() >= MIN_RULE_LENGTH {
            vertical.push(Segment {
                coord: (x0 + x1) / 2.0,
                start: y0.min(y1),
                end: y0.max(y1),
            });
        }
    };
    for path in paths.iter().filter(|path| path.stroke_color.is_some()) {
        let mut origin = None;
        let mut cursor = None;
        for operation in &path.operations {
            match *operation {
                PathOperation::MoveTo(x, y) => {
                    origin = Some((x, y));
                    cursor = origin;
                }
                PathOperation::LineTo(x, y) => {
                    if let Some(from) = cursor {
                        add(from, (x, y));
                    }
                    cursor = Some((x, y));
                }
                PathOperation::CurveTo(_, _, _, _, x, y) => cursor = Some((x, y)),
                PathOperation::ClosePath => {
                    if let (Some(from), Some(to)) = (cursor, origin) {
                        add(from, to);
                    }
                    cursor = origin;
                }
                PathOperation::Rectangle(x, y, width, height) => {
                    // Thin painted strips already supply a center line.
                    if width.abs() > MAX_RULE_WIDTH && height.abs() > MAX_RULE_WIDTH {
                        let corners = [
                            (x, y),
                            (x + width, y),
                            (x + width, y + height),
                            (x, y + height),
                        ];
                        for (from, to) in
                            corners.into_iter().zip(corners.into_iter().cycle().skip(1))
                        {
                            add(from, to);
                        }
                    }
                    origin = Some((x, y));
                    cursor = origin;
                }
            }
        }
    }
    (horizontal, vertical)
}

fn table_rules(
    horizontal: Vec<Segment>,
    vertical: Vec<Segment>,
    snap: f32,
) -> (Vec<Rule>, Vec<Rule>) {
    let mut rows = snap_rules(horizontal, snap);
    rows.reverse(); // PDF y grows upwards, the table starts at the top.
    (rows, snap_rules(vertical, snap))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Horizontal,
    Vertical,
}

fn touches(
    (axis_a, a): (Direction, Segment),
    (axis_b, b): (Direction, Segment),
    snap: f32,
) -> bool {
    if axis_a == axis_b {
        return (a.coord - b.coord).abs() <= snap
            && a.start.max(b.start) <= a.end.min(b.end) + snap;
    }
    (a.start - snap..=a.end + snap).contains(&b.coord)
        && (b.start - snap..=b.end + snap).contains(&a.coord)
}

pub(crate) struct PageRules {
    painted: (Vec<Segment>, Vec<Segment>),
    combined: (Vec<Segment>, Vec<Segment>),
    grids: Vec<(Vec<Rule>, Vec<Rule>)>,
    combined_grids: Option<Vec<(Vec<Rule>, Vec<Rule>)>>,
}

impl PageRules {
    pub(crate) fn new(paths: &[PathContent]) -> Self {
        let painted = extract_segments(paths, FULL_PAGE, MIN_DOUBLE_RULE_LENGTH);
        let long = |segments: &[Segment]| -> Vec<_> {
            segments
                .iter()
                .copied()
                .filter(|edge| edge.end - edge.start >= MIN_RULE_LENGTH)
                .collect()
        };
        let (horizontal, vertical) = (long(&painted.0), long(&painted.1));
        let grids = connected_grids(&horizontal, &vertical, SNAP, 2, 1);
        let mut combined = stroke_segments(paths);
        let has_strokes = !combined.0.is_empty() || !combined.1.is_empty();
        combined.0.extend(horizontal);
        combined.1.extend(vertical);
        // With no strokes, both passes see the same connected components.
        let combined_grids = (has_strokes && combined.0.len() >= 3 && !combined.1.is_empty())
            .then(|| connected_grids(&combined.0, &combined.1, SNAP, 3, 1));
        Self {
            painted,
            combined,
            grids,
            combined_grids,
        }
    }
}

fn connected_grids(
    horizontal: &[Segment],
    vertical: &[Segment],
    snap: f32,
    min_row_rules: usize,
    min_col_rules: usize,
) -> Vec<(Vec<Rule>, Vec<Rule>)> {
    if horizontal.len() < min_row_rules || vertical.len() < min_col_rules {
        return Vec::new();
    }
    let mut remaining: Vec<_> = horizontal
        .iter()
        .copied()
        .map(|edge| (Direction::Horizontal, edge))
        .chain(
            vertical
                .iter()
                .copied()
                .map(|edge| (Direction::Vertical, edge)),
        )
        .collect();
    let mut candidates = Vec::new();

    // Adjacent parallel pieces form one rule. Crossings connect rules into
    // page-local components, so tables with aligned columns stay separate.
    while let Some(first) = remaining.pop() {
        let mut pending = vec![first];
        let (mut horizontal, mut vertical) = (Vec::new(), Vec::new());
        while let Some((axis, edge)) = pending.pop() {
            match axis {
                Direction::Horizontal => horizontal.push(edge),
                Direction::Vertical => vertical.push(edge),
            }
            let mut index = 0;
            while index < remaining.len() {
                if touches((axis, edge), remaining[index], snap) {
                    pending.push(remaining.swap_remove(index));
                } else {
                    index += 1;
                }
            }
        }
        let (ys, xs) = table_rules(horizontal, vertical, snap);
        if ys.len() >= min_row_rules && xs.len() >= min_col_rules {
            candidates.push((ys, xs));
        }
    }

    candidates
}

fn close_open_sides(ys: &[Rule], xs: &[Rule], text: &PageText<'_>) -> Option<Table> {
    let edges = |rule: &Rule| {
        Some((
            rule.intervals.first()?.0,
            rule.intervals
                .iter()
                .map(|&(_, end)| end)
                .reduce(f32::max)?,
        ))
    };
    let (left, right) = edges(ys.first()?)?;
    let aligned = |rule: &Rule| {
        edges(rule).is_some_and(|(a, b)| (a - left).abs() <= SNAP && (b - right).abs() <= SNAP)
            && rule.covers(left, right, SNAP)
    };
    if !aligned(ys.first()?)
        || !aligned(ys.last()?)
        || ys[1..ys.len() - 1]
            .iter()
            .filter(|rule| aligned(rule))
            .take(2)
            .count()
            < 2
        || xs[0].coord < left - SNAP
        || xs[xs.len() - 1].coord > right + SNAP
    {
        return None;
    }
    let (top, bottom) = (ys[0].coord, ys[ys.len() - 1].coord);
    let mut columns = xs.to_vec();
    for x in [left, right] {
        if columns.iter().any(|rule| (rule.coord - x).abs() <= SNAP) {
            continue;
        }
        // A repeated rule endpoint can delimit an undrawn side. A word
        // straddling that endpoint is evidence against the inferred side.
        if text.words.iter().any(|word| {
            let bbox = word.bbox;
            (bottom..=top).contains(&bbox.center().y)
                && bbox.x < x - SNAP
                && bbox.x + bbox.width > x + SNAP
        }) {
            return None;
        }
        columns.push(Rule {
            coord: x,
            intervals: vec![(bottom, top)],
        });
    }
    if columns.len() == xs.len() || columns.len() < 3 {
        return None;
    }
    columns.sort_by(|a, b| a.coord.total_cmp(&b.coord));
    build_detected_table(ys, &columns, text, SNAP).ok()
}

fn grid_bounds(ys: &[Rule], xs: &[Rule]) -> Rect {
    Rect::from_points(
        xs[0].coord,
        ys[ys.len() - 1].coord,
        xs[xs.len() - 1].coord,
        ys[0].coord,
    )
}

// A recurring series of aligned text lines within several ruled bands is
// evidence that the horizontal rules group records instead of separating them.
fn infer_row_rules(ys: &[Rule], xs: &[Rule], text: &PageText<'_>, snap: f32) -> Vec<Rule> {
    if ys.len() < 3 || xs.len() < 3 {
        return Vec::new();
    }
    let left = xs[0].coord;
    let right = xs[xs.len() - 1].coord;
    let bounds = grid_bounds(ys, xs);
    let lines = text.lines_in(bounds, |word| bounds.contains_point(&word.bbox.center()));
    struct RowBand {
        index: usize,
        first_baseline: f32,
        last_baseline: f32,
        pitch: f32,
        separators: Vec<Rule>,
        follows_header: bool,
    }
    let mut candidates = Vec::new();
    let mut header = None;
    let mut remaining = lines.as_slice();
    for (index, band) in ys.windows(2).enumerate() {
        let top = band[0].coord;
        let bottom = band[1].coord;
        let count = remaining.partition_point(|line| line[0].bbox.center().y > bottom);
        let (lines, rest) = remaining.split_at(count);
        remaining = rest;
        let single_header_line = index == 0 && lines.len() == 1;
        if (!single_header_line && lines.len() < 3)
            || xs.iter().any(|rule| !rule.covers(bottom, top, snap))
        {
            continue;
        }

        let mut edges = vec![vec![None::<(f32, f32)>; xs.len() - 1]; lines.len()];
        for (line, cells) in lines.iter().zip(&mut edges) {
            for word in line {
                let x = word.bbox.center().x;
                let col = xs.partition_point(|rule| rule.coord <= x);
                let start = word.bbox.x;
                let end = start + word.bbox.width;
                if col > 0 && col < xs.len() {
                    let cell = &mut cells[col - 1];
                    *cell = Some(cell.map_or((start, end), |(l, r)| (l.min(start), r.max(end))));
                }
            }
        }
        let has_frame = index <= 1 && band.iter().all(|rule| rule.covers(left, right, snap));
        if single_header_line
            && has_frame
            && edges[0]
                .iter()
                .filter(|cell| cell.is_some())
                .take(2)
                .count()
                == 2
            && lines[0]
                .iter()
                .any(|word| word.text.chars().any(char::is_alphabetic))
        {
            header = Some((lines[0][0].bbox.center().y, top - bottom));
        }
        if lines.len() < 3 {
            continue;
        }
        let aligned = (0..xs.len() - 1)
            .filter(|&col| {
                let Some((left, right)) = edges[0][col] else {
                    return false;
                };
                [(left, true), (right, false)]
                    .into_iter()
                    .any(|(anchor, check_left)| {
                        edges.iter().all(|row| {
                            row[col].is_some_and(|(l, r)| {
                                ((if check_left { l } else { r }) - anchor).abs() <= snap
                            })
                        })
                    })
            })
            .count();
        if aligned < ((xs.len() - 1) / 2).max(2) {
            continue;
        }
        let gaps: Vec<_> = lines
            .windows(2)
            .map(|pair| pair[0][0].bbox.center().y - pair[1][0].bbox.center().y)
            .collect();
        let pitch = gaps.iter().copied().sum::<f32>() / gaps.len() as f32;
        if gaps
            .iter()
            .any(|&gap| (gap - pitch).abs() > pitch * ROW_PITCH_TOLERANCE)
        {
            continue;
        }
        let mut separators = Vec::new();
        for pair in lines.windows(2) {
            let upper_bottom = pair[0].iter().map(|w| w.bbox.y).fold(top, f32::min);
            let lower_top = pair[1]
                .iter()
                .map(|w| w.bbox.y + w.bbox.height)
                .fold(bottom, f32::max);
            if upper_bottom > lower_top {
                separators.push(Rule {
                    coord: (upper_bottom + lower_top) / 2.0,
                    intervals: vec![(left, right)],
                });
            }
        }
        if separators.len() == lines.len() - 1 {
            let first_baseline = lines[0][0].bbox.center().y;
            // The header can supply the second band of evidence. Require
            // the same cadence and a matching frame height per text line.
            let follows_header = index == 1
                && has_frame
                && header.is_some_and(|(baseline, height)| {
                    [
                        baseline - first_baseline,
                        height,
                        (top - bottom) / lines.len() as f32,
                    ]
                    .iter()
                    .all(|step| (step - pitch).abs() <= pitch * ROW_PITCH_TOLERANCE)
                });
            candidates.push(RowBand {
                index,
                first_baseline,
                last_baseline: lines[lines.len() - 1][0].bbox.center().y,
                pitch,
                separators,
                follows_header,
            });
        }
    }
    // Cell padding creates a larger gap between separately wrapped cells.
    // Accept only bands whose text cadence also continues across a rule.
    let mut supported: Vec<_> = candidates.iter().map(|band| band.follows_header).collect();
    for (i, pair) in candidates.windows(2).enumerate() {
        let gap = pair[0].last_baseline - pair[1].first_baseline;
        if pair[0].index + 1 == pair[1].index
            && pair
                .iter()
                .all(|band| (gap - band.pitch).abs() <= band.pitch * ROW_PITCH_TOLERANCE)
        {
            supported[i] = true;
            supported[i + 1] = true;
        }
    }
    candidates
        .into_iter()
        .zip(supported)
        .filter(|(_, accept)| *accept)
        .flat_map(|(band, _)| band.separators)
        .collect()
}

// A group header can be joined to its children by fill alone. The children
// have vertical rules that start at the same height between the two baselines.
fn infer_header_rules(
    ys: &[Rule],
    xs: &[Rule],
    words: &[Word],
    body_top: f32,
    snap: f32,
) -> Vec<Rule> {
    let mut segments = Vec::new();
    for band in ys.windows(2).take_while(|band| band[1].coord >= body_top) {
        let (top, bottom) = (band[0].coord, band[1].coord);
        let borders: Vec<_> = xs
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.covers(bottom, top, snap))
            .map(|(i, _)| i)
            .collect();
        for group in borders.windows(2).filter(|pair| pair[1] > pair[0] + 1) {
            let (left, right) = (xs[group[0]].coord, xs[group[1]].coord);
            let ends: Vec<_> = xs[group[0] + 1..group[1]]
                .iter()
                .filter_map(|rule| {
                    rule.intervals
                        .iter()
                        .rev()
                        .map(|&(_, end)| end)
                        .find(|&end| {
                            end > bottom + snap
                                && end < top - snap
                                && rule.covers(bottom, end, snap)
                        })
                })
                .collect();
            if ends.len() != group[1] - group[0] - 1 {
                continue;
            }
            let y = ends.iter().sum::<f32>() / ends.len() as f32;
            if ends.iter().any(|end| (end - y).abs() > snap) {
                continue;
            }
            let mut children = vec![false; group[1] - group[0]];
            let mut crosses = false;
            for word in words {
                let point = word.bbox.center();
                if !(left..right).contains(&point.x) || !(bottom..top).contains(&point.y) {
                    continue;
                }
                crosses |= word.bbox.y < y && word.bbox.y + word.bbox.height > y;
                if point.y < y {
                    let col =
                        xs[group[0] + 1..group[1]].partition_point(|rule| rule.coord <= point.x);
                    children[col] = true;
                }
            }
            if !crosses
                && children
                    .iter()
                    .filter(|&&has_text| has_text)
                    .take(2)
                    .count()
                    == 2
            {
                segments.push(Segment {
                    coord: y,
                    start: left,
                    end: right,
                });
            }
        }
    }
    snap_rules(segments, snap)
}

fn build_detected_table(
    ys: &[Rule],
    xs: &[Rule],
    text: &PageText<'_>,
    snap: f32,
) -> Result<Table, String> {
    let mut table = build_detected_grid(ys, xs, text, snap)?;
    expand_framed_body(&mut table, xs, text, snap);
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use PathOperation::{ClosePath, LineTo, MoveTo};

    #[test]
    fn reads_each_thin_polygon_in_a_compound_path() {
        let compound = PathContent::from_operations(vec![
            MoveTo(50.0, 120.0),
            LineTo(150.0, 120.0),
            LineTo(150.0, 120.5),
            LineTo(50.0, 120.5),
            MoveTo(50.0, 100.0),
            LineTo(150.0, 100.0),
            LineTo(150.0, 100.5),
            LineTo(50.0, 100.5),
            LineTo(50.0, 100.0),
            ClosePath,
            MoveTo(50.0, 110.0),
            LineTo(150.0, 110.0),
            LineTo(50.0, 110.0),
            LineTo(50.0, 110.5),
        ]);
        let (horizontal, vertical) = extract_segments(&[compound], FULL_PAGE, MIN_RULE_LENGTH);
        assert_eq!(horizontal.len(), 2);
        assert_eq!(horizontal[0].coord, 120.25);
        assert_eq!(horizontal[1].coord, 100.25);
        assert_eq!((horizontal[0].start, horizontal[0].end), (50.0, 150.0));
        assert!(vertical.is_empty());
    }

    #[test]
    fn restores_open_sides_only_when_horizontal_endpoints_repeat() {
        let mut rows = [400.0, 380.0, 360.0, 340.0].map(|coord| Rule {
            coord,
            intervals: vec![(50.0, 350.0)],
        });
        let columns = [Rule {
            coord: 160.0,
            intervals: vec![(340.0, 400.0)],
        }];
        let text = PageText::new(&[]);
        let table = close_open_sides(&rows, &columns, &text).expect("supported outer sides");
        assert_eq!((table.rows.len(), table.col_count), (3, 2));
        rows[2].intervals[0].1 = 320.0;
        assert!(close_open_sides(&rows, &columns, &text).is_none());
    }

    #[test]
    fn splits_a_token_at_a_drawn_separator_and_excludes_the_clipped_suffix() {
        let rows = [40.0, 20.0].map(|coord| Rule {
            coord,
            intervals: vec![(0.0, 40.0)],
        });
        let columns = [0.0, 20.0, 40.0].map(|coord| Rule {
            coord,
            intervals: vec![(20.0, 40.0)],
        });
        let word = Word::from_chars(
            [('A', 13.0), ('b', 17.0), ('1', 21.0), ('2', 25.0)]
                .into_iter()
                .map(|(char, x)| TextChar {
                    char,
                    bbox: Rect::new(x, 25.0, 4.0, 8.0),
                    origin_x: x,
                    advance_width: 4.0,
                    ..TextChar::default()
                })
                .collect(),
        );
        let table = build_table(&rows, &columns, &[word], SNAP).unwrap();
        assert_eq!(table.rows[0].cells[0].text, "A");
        assert_eq!(table.rows[0].cells[1].text, "12");
    }

    #[test]
    fn recovers_closed_cells_beside_an_empty_open_corner() {
        let rows = [40.0, 30.0, 20.0, 10.0].map(|coord| Rule {
            coord,
            intervals: vec![(if coord == 40.0 { 20.0 } else { 0.0 }, 60.0)],
        });
        let columns = [0.0, 20.0, 40.0, 60.0].map(|coord| Rule {
            coord,
            intervals: vec![(10.0, if coord == 40.0 { 30.0 } else { 40.0 })],
        });
        let word = |char, x, y| {
            Word::from_chars(vec![TextChar {
                char,
                bbox: Rect::new(x, y, 4.0, 5.0),
                origin_x: x,
                ..TextChar::default()
            }])
        };
        let mut words = vec![
            word('G', 24.0, 32.0),
            word('A', 4.0, 24.0),
            word('B', 44.0, 24.0),
        ];
        assert!(build_table(&rows, &columns, &words, SNAP).is_err());
        let table = build_table_with_text(&rows, &columns, &words, SNAP, true, word_text).unwrap();
        assert_eq!((table.rows.len(), table.col_count), (3, 3));
        assert_eq!(table.rows[0].cells[0].text, "");
        assert_eq!(table.rows[0].cells[1].text, "G");
        assert_eq!(table.rows[0].cells[1].colspan, 2);
        words.push(word('X', 4.0, 32.0));
        assert!(build_table_with_text(&rows, &columns, &words, SNAP, true, word_text).is_err());
    }
}

fn build_detected_grid(
    ys: &[Rule],
    xs: &[Rule],
    text: &PageText<'_>,
    snap: f32,
) -> Result<Table, String> {
    let words = text.words;
    let inferred = infer_row_rules(ys, xs, text, snap);
    if let Some(body) = ys.windows(2).find(|band| {
        xs.iter()
            .all(|rule| rule.covers(band[1].coord, band[0].coord, snap))
    }) {
        let body_top = body[0].coord;
        let headers = infer_header_rules(ys, xs, words, body_top, snap);
        if !headers.is_empty() {
            let mut refined = headers;
            refined.extend_from_slice(ys);
            refined.extend_from_slice(&inferred);
            refined.sort_by(|a, b| b.coord.total_cmp(&a.coord));
            if let Ok(mut table) = build_table(&refined, xs, words, snap) {
                for (row, _) in table
                    .rows
                    .iter_mut()
                    .zip(&refined)
                    .take_while(|(_, rule)| rule.coord > body_top)
                {
                    row.is_header = true;
                    for cell in &mut row.cells {
                        cell.is_header = true;
                    }
                }
                return Ok(table);
            }
        }
    }
    if !inferred.is_empty() {
        let mut inferred = inferred;
        inferred.extend_from_slice(ys);
        inferred.sort_by(|a, b| b.coord.total_cmp(&a.coord));
        if let Ok(table) = build_table(&inferred, xs, words, snap) {
            return Ok(table);
        }
    }
    build_table(ys, xs, words, snap)
}

fn expand_framed_body(table: &mut Table, xs: &[Rule], text: &PageText<'_>, snap: f32) {
    let Some(bounds) = table.bbox else { return };
    let columns = xs.len() - 1;
    let full_width = |row: &TableRow| {
        row.cells.len() == 1
            && row.cells[0].rowspan == 1
            && row.cells[0].colspan as usize == columns
            && row.cells[0].bbox.is_some()
    };
    let body_start = table
        .rows
        .iter()
        .rposition(|row| !full_width(row))
        .map_or(0, |i| i + 1);
    if body_start == 0 || body_start == table.rows.len() {
        return;
    }
    let body_top = table.rows[body_start].cells[0].bbox.unwrap();
    let body_top = body_top.y + body_top.height;
    if !xs[1..columns].iter().all(|rule| {
        rule.intervals
            .iter()
            .any(|&(a, b)| b.min(bounds.y + bounds.height) - a.max(body_top) >= MIN_RULE_LENGTH)
    }) {
        return;
    }
    let edges: Vec<_> = xs.iter().map(|rule| rule.coord).collect();
    let mut body = Vec::new();
    for row in &table.rows[body_start..] {
        let region = row.cells[0].bbox.unwrap();
        let Some(rows) = crate::borderless::read_framed_body(region, &edges, text, snap) else {
            return;
        };
        body.extend(rows);
    }
    table.rows.truncate(body_start);
    for row in &mut table.rows {
        row.is_header = true;
        for cell in &mut row.cells {
            cell.is_header = true;
        }
    }
    table.rows.extend(body);
}

fn extend_rules_from_fills(ys: &mut [Rule], xs: &[Rule], paths: &[PathContent]) -> bool {
    let mut extended = false;
    for path in paths.iter().filter(|path| {
        path.fill_color
            .as_ref()
            .is_some_and(|c| c.r.min(c.g).min(c.b) < 0.97)
    }) {
        for op in &path.operations {
            let PathOperation::Rectangle(x, y, w, h) = *op else {
                continue;
            };
            if w.abs() <= MAX_RULE_WIDTH || h.abs() <= MAX_RULE_WIDTH {
                continue;
            }
            let (left, right) = (x.min(x + w), x.max(x + w));
            if ![left, right]
                .into_iter()
                .all(|edge| xs.iter().any(|rule| (rule.coord - edge).abs() <= SNAP))
            {
                continue;
            }
            for rule in ys
                .iter_mut()
                .filter(|rule| (rule.coord - y).abs().min((rule.coord - y - h).abs()) <= SNAP)
            {
                rule.intervals.push((left, right));
                extended = true;
            }
        }
    }
    if extended {
        for rule in ys {
            rule.intervals.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
    }
    extended
}

fn build_filled_grid<'a>(
    ys: &[Rule],
    outer: &[Rule],
    fills: &[Rect],
    words: &[Word],
    read_glyphs: impl FnOnce() -> Option<&'a [TextChar]>,
) -> Option<Table> {
    let bounds = grid_bounds(ys, outer);
    let left = bounds.x;
    let right = bounds.x + bounds.width;
    let fills: Vec<_> = fills
        .iter()
        .filter(|cell| {
            cell.x >= left - SNAP
                && cell.x + cell.width <= right + SNAP
                && cell.y >= bounds.y - SNAP
                && cell.y + cell.height <= bounds.y + bounds.height + SNAP
        })
        .collect();
    let in_band = |cell: &Rect, band: &[Rule]| {
        (cell.y - band[1].coord).abs() <= SNAP && cell.y + cell.height >= band[0].coord - SNAP
    };
    let (header, mut slots) = ys
        .windows(2)
        .map(|band| {
            fills
                .iter()
                .copied()
                .filter(|cell| {
                    in_band(cell, band)
                        && words.iter().any(|word| {
                            let point = word.bbox.center();
                            (band[1].coord..band[0].coord).contains(&point.y)
                                && cell.contains_point(&point)
                        })
                })
                .collect::<Vec<_>>()
        })
        .take_while(|cells| cells.len() >= 2)
        .enumerate()
        .max_by_key(|(row, cells)| (cells.len(), std::cmp::Reverse(*row)))?;
    slots.sort_by(|a, b| a.x.total_cmp(&b.x));
    if slots.len() < 3
        || slots[0].x - left > SNAP + MAX_RULE_WIDTH
        || right - (slots.last()?.x + slots.last()?.width) > SNAP + MAX_RULE_WIDTH
        || slots.windows(2).any(|pair| {
            let gap = pair[1].x - pair[0].x - pair[0].width;
            // A narrow unlabeled gutter between the header boxes is padding.
            !(-SNAP..=pair[0].width.min(pair[1].width) / 4.0).contains(&gap)
        })
    {
        return None;
    }

    let mut columns = vec![outer[0].clone()];
    for pair in slots.windows(2) {
        let coord = (pair[0].x + pair[0].width + pair[1].x) / 2.0;
        let intervals = ys
            .windows(2)
            .filter(|band| {
                !fills.iter().any(|cell| {
                    in_band(cell, band)
                        && cell.x <= pair[0].center().x
                        && cell.x + cell.width >= pair[1].center().x
                })
            })
            .rev()
            .map(|band| (band[1].coord, band[0].coord))
            .collect();
        columns.push(Rule { coord, intervals });
    }
    columns.push(outer[1].clone());

    // The frame establishes the row levels even when the painted separator
    // only runs under the filled columns. Header separators retain their
    // original extent so the grid can recover vertically spanning labels.
    let mut rows = ys.to_vec();
    let last = rows.len() - 1;
    for rule in &mut rows[header + 1..last] {
        rule.intervals = vec![(left, right)];
    }
    let mut source = read_glyphs().unwrap_or_default().to_vec();
    source.sort_by(|a, b| a.origin_x.total_cmp(&b.origin_x));
    let mut table = build_table_with_text(&rows, &columns, words, SNAP, false, |words| {
        positioned_word_text(words, &source)
    })
    .ok()?;
    if table.rows[header + 1..]
        .iter()
        .filter(|row| row.cells.iter().filter(|c| !c.text.is_empty()).count() >= 2)
        .take(3)
        .count()
        < 3
    {
        return None;
    }
    for row in &mut table.rows[..=header] {
        row.is_header = true;
        for cell in &mut row.cells {
            cell.is_header = true;
        }
    }
    Some(table)
}

pub fn detect<'a>(
    paths: &[PathContent],
    rules: &PageRules,
    text: &PageText<'_>,
    read_glyphs: impl Fn() -> Option<&'a [TextChar]>,
) -> Vec<Result<Table, String>> {
    let words = text.words;
    let mut tables = Vec::new();
    let mut rejected = Vec::new();
    for (ys, xs) in rules
        .grids
        .iter()
        .filter(|(ys, xs)| ys.len() >= 3 && xs.len() >= 3)
    {
        match build_detected_table(ys, xs, text, SNAP) {
            Ok(table) => tables.push(table),
            Err(error) => rejected.push((grid_bounds(ys, xs), error)),
        }
    }

    // Double rules leave short corner pieces and several strokes per cell edge.
    // Try the wider tolerance only for regions still missing a valid grid.
    if !rejected.is_empty() {
        for (ys, xs) in connected_grids(&rules.painted.0, &rules.painted.1, DOUBLE_RULE_SNAP, 3, 3)
        {
            let bounds = grid_bounds(&ys, &xs);
            if !rejected
                .iter()
                .any(|(region, _)| region.intersects(&bounds))
                || tables
                    .iter()
                    .any(|table| table.bbox.is_some_and(|r| r.intersects(&bounds)))
            {
                continue;
            }
            if let Ok(table) = build_detected_table(&ys, &xs, text, DOUBLE_RULE_SNAP) {
                rejected.retain(|(region, _)| !region.intersects(&bounds));
                tables.push(table);
            }
        }
    }

    let fills = crate::borderless::header_cells(paths);
    let grids = rules.combined_grids.as_deref().unwrap_or(&rules.grids);
    for (ys, xs) in grids.iter().filter(|(ys, _)| ys.len() >= 3) {
        let bounds = grid_bounds(ys, xs);
        if tables
            .iter()
            .any(|table| table.bbox.is_some_and(|r| r.intersects(&bounds)))
        {
            continue;
        }
        // The grid is read in the path coordinate frame. When most
        // words run sideways, its horizontal edges are not row edges.
        let mut upright = 0;
        let mut sideways = 0;
        for word in words
            .iter()
            .filter(|word| bounds.contains_point(&word.bbox.center()))
        {
            let angle = word.rotation_degrees.rem_euclid(180.0);
            if angle <= WORD_ANGLE_TOLERANCE || angle >= 180.0 - WORD_ANGLE_TOLERANCE {
                upright += 1;
            } else {
                sideways += 1;
            }
        }
        if upright <= sideways {
            continue;
        }
        let normal = || build_detected_table(ys, xs, text, SNAP).ok();
        // Without a separate stroke grid, this exact candidate was tried above.
        let table = (rules.combined_grids.is_some() && xs.len() >= 3)
            .then(normal)
            .flatten();
        let table = table
            .or_else(|| close_open_sides(ys, xs, text))
            .or_else(|| {
                if xs.len() == 2 {
                    return build_filled_grid(ys, xs, &fills, words, &read_glyphs);
                }
                if xs.len() < 3 {
                    return None;
                }
                let mut ys = ys.clone();
                extend_rules_from_fills(&mut ys, xs, paths)
                    .then(|| build_detected_table(&ys, xs, text, SNAP).ok())
                    .flatten()
            })
            .or_else(|| build_table_with_text(ys, xs, words, SNAP, true, word_text).ok());
        let Some(table) = table else { continue };
        if tables.iter().any(|other| {
            other
                .bbox
                .zip(table.bbox)
                .is_some_and(|(a, b)| a.intersects(&b))
        }) {
            continue;
        }
        // An empty drawing can have the same geometry as a table. A row
        // below a rowspan anchor can legitimately have just one cell.
        let mut filled = table.rows.iter().map(|row| {
            row.cells
                .iter()
                .filter(|c| !c.text.trim().is_empty())
                .count()
        });
        if filled.clone().filter(|&count| count > 0).take(2).count() < 2
            || !filled.any(|count| count >= 2)
        {
            continue;
        }
        rejected.retain(|(region, _)| !region.intersects(&bounds));
        tables.push(table);
    }

    for table in &mut tables {
        restore_rtl_cells(table, &read_glyphs);
    }
    let occupied: Vec<_> = tables.iter().filter_map(|table| table.bbox).collect();
    tables.extend(crate::borderless::detect(&fills, text, &occupied));
    tables.sort_by(|a, b| {
        let a = a.bbox.expect("detected table has bounds");
        let b = b.bbox.expect("detected table has bounds");
        (b.y + b.height)
            .total_cmp(&(a.y + a.height))
            .then_with(|| a.x.total_cmp(&b.x))
    });
    tables
        .into_iter()
        .map(Ok)
        .chain(
            rejected
                .into_iter()
                .map(|(r, error)| Err(format!("x={:.2}, y={:.2}: {error}", r.x, r.y + r.height))),
        )
        .collect()
}

pub(crate) fn horizontal_frames(rules: &PageRules) -> Vec<(Rect, Vec<f32>)> {
    let mut groups: Vec<Vec<Segment>> = Vec::new();
    for rule in snap_rules(rules.combined.0.clone(), AXIS_TOLERANCE) {
        // A single visible line can have path vertices at every column edge.
        let mut intervals: Vec<(f32, f32)> = Vec::new();
        for (start, end) in rule.intervals {
            if let Some(last) = intervals.last_mut()
                && start <= last.1 + AXIS_TOLERANCE
            {
                last.1 = last.1.max(end);
            } else {
                intervals.push((start, end));
            }
        }
        for (start, end) in intervals {
            let edge = Segment {
                coord: rule.coord,
                start,
                end,
            };
            if let Some(group) = groups.iter_mut().find(|group| {
                (start - group[0].start).abs() <= SNAP && (end - group[0].end).abs() <= SNAP
            }) {
                group.push(edge);
            } else {
                groups.push(vec![edge]);
            }
        }
    }
    groups
        .into_iter()
        .filter_map(|group| {
            let left = group[0].start;
            let right = group[0].end;
            let mut rows = snap_rules(group, SNAP);
            if rows.len() < 3 {
                return None;
            }
            rows.reverse();
            let bounds = Rect::from_points(left, rows.last()?.coord, right, rows[0].coord);
            Some((bounds, rows.into_iter().map(|row| row.coord).collect()))
        })
        .collect()
}

fn list_marker(text: &str) -> Option<char> {
    let mut chars = text.trim_start().chars();
    let marker = chars.next()?;
    if matches!(marker, '•' | '▪' | '◦' | '‣') && chars.next().is_some_and(char::is_whitespace)
    {
        Some(marker)
    } else {
        None
    }
}

pub fn join_continuation(
    previous: &mut Table,
    previous_page: Rect,
    page: Rect,
    rules: &PageRules,
    words: &[Word],
    tables: &[Table],
) -> bool {
    let Some(previous_bounds) = previous.bbox else {
        return false;
    };
    let Some(last_row) = previous.rows.last_mut() else {
        return false;
    };
    if previous_bounds.y > previous_page.y + previous_page.height * PAGE_EDGE_BAND
        || last_row.is_header
        || last_row.cells.len() != previous.col_count
        || !last_row
            .cells
            .iter()
            .any(|c| list_marker(&c.text).is_some())
    {
        return false;
    }

    let mut matches = rules.grids.iter().filter_map(|(ys, xs)| {
        if ys.len() != 2 || xs.len() != previous.col_count + 1 {
            return None;
        }
        let bounds = grid_bounds(ys, xs);
        if bounds.y + bounds.height < page.y + page.height * (1.0 - PAGE_EDGE_BAND)
            || tables
                .iter()
                .any(|t| t.bbox.is_some_and(|r| r.intersects(&bounds)))
        {
            return None;
        }
        let fragment = build_table(ys, xs, words, SNAP).ok()?;
        let row = fragment.rows.into_iter().next()?;
        if row.cells.len() != last_row.cells.len()
            || row.cells.iter().all(|c| c.text.is_empty())
            || row.cells.iter().all(|c| !c.text.is_empty())
        {
            return None;
        }
        last_row
            .cells
            .iter()
            .zip(&row.cells)
            .all(|(old, new)| {
                old.rowspan == 1
                    && old.colspan == 1
                    && new.colspan == 1
                    && old.bbox.zip(new.bbox).is_some_and(|(a, b)| {
                        (a.x - b.x).abs() <= SNAP && (a.x + a.width - b.x - b.width).abs() <= SNAP
                    })
                    && (new.text.is_empty()
                        || list_marker(&new.text)
                            .is_some_and(|m| list_marker(&old.text) == Some(m)))
            })
            .then_some(row)
    });
    let Some(fragment) = matches.next() else {
        return false;
    };
    if matches.next().is_some() {
        return false;
    }
    for (cell, continued) in last_row.cells.iter_mut().zip(fragment.cells) {
        if !continued.text.is_empty() {
            cell.text.push('\n');
            cell.text.push_str(&continued.text);
        }
    }
    true
}

pub fn reconstruct(
    paths: &[PathContent],
    words: &[Word],
    region: [f32; 4],
) -> Result<Table, String> {
    let (horizontal, vertical) = extract_segments(paths, region, MIN_RULE_LENGTH);
    let (ys, xs) = table_rules(horizontal, vertical, SNAP);
    build_table(&ys, &xs, words, SNAP)
}

fn build_table(ys: &[Rule], xs: &[Rule], words: &[Word], snap: f32) -> Result<Table, String> {
    build_table_with_text(ys, xs, words, snap, false, word_text)
}

fn build_table_with_text(
    ys: &[Rule],
    xs: &[Rule],
    words: &[Word],
    snap: f32,
    recover_open_frame: bool,
    read_text: impl Fn(Vec<&Word>) -> String,
) -> Result<Table, String> {
    if ys.len() < 2 || xs.len() < 2 {
        return Err("Could not find outer table borders".into());
    }
    let (rows, cols) = (ys.len() - 1, xs.len() - 1);
    let horizontal = |r: usize, c: usize| ys[r].covers(xs[c].coord, xs[c + 1].coord, snap);
    let vertical = |r: usize, c: usize| xs[c].covers(ys[r].coord, ys[r + 1].coord, snap);
    let open_frame = (0..cols).any(|c| !horizontal(0, c) || !horizontal(rows, c))
        || (0..rows).any(|r| !vertical(r, 0) || !vertical(r, cols));
    if open_frame && !recover_open_frame {
        return Err(format!(
            "Outer table border is open ({rows} rows, {cols} columns)"
        ));
    }
    if recover_open_frame && !open_frame {
        return Err("Outer table border is already closed".into());
    }

    let mut table = Table {
        rows: (0..rows).map(|r| TableRow::new(r == 0)).collect(),
        has_header: true,
        col_count: cols,
        bbox: Some(grid_bounds(ys, xs)),
    };
    let mut cell_words = vec![vec![Vec::new(); cols]; rows];
    let mut split_words = Vec::new();
    let column_at = |x: f32| {
        let after = xs.partition_point(|rule| rule.coord <= x);
        after.checked_sub(1).filter(|&column| column < cols)
    };
    for word in words {
        let point = word.bbox.center();
        let row = ys
            .windows(2)
            .position(|y| y[1].coord < point.y && point.y <= y[0].coord);
        let Some(r) = row else { continue };
        let cuts: Vec<_> = word
            .chars
            .windows(2)
            .enumerate()
            .filter_map(|(i, pair)| {
                let a = pair[0].bbox.x;
                let b = pair[1].bbox.x;
                let (left, right) = (column_at(a)?, column_at(b)?);
                let edge = (left + 1..=right).find(|&c| vertical(r, c))?;
                Some((i + 1, Some(xs[edge].coord)))
            })
            .collect();
        if !cuts.is_empty() && word.chars.iter().map(|ch| ch.char).eq(word.text.chars()) {
            let mut start = 0;
            for (end, edge) in cuts.into_iter().chain([(word.chars.len(), None)]) {
                let mut fragment = &word.chars[start..end];
                if let Some(edge) = edge {
                    let last = &word.chars[end - 1];
                    // A clipped suffix can overlap the start of the next cell.
                    // Use the glyph advance without inter-word spacing.
                    let width = if last.advance_width > 0.0 {
                        last.advance_width
                    } else {
                        last.bbox.width
                    };
                    if last.origin_x + width > edge + AXIS_TOLERANCE {
                        fragment = &fragment[..fragment.len() - 1];
                    }
                }
                if let Some(c) = fragment.iter().find_map(|ch| column_at(ch.bbox.x)) {
                    split_words.push((r, c, Word::from_chars(fragment.to_vec())));
                }
                start = end;
            }
        } else {
            // A one-glyph word can also inherit the advance to the next cell.
            let first = word.chars.first().and_then(|ch| column_at(ch.bbox.x));
            let last = word.chars.last().and_then(|ch| column_at(ch.bbox.x));
            if let Some(c) = first
                .filter(|_| first == last)
                .or_else(|| column_at(point.x))
            {
                cell_words[r][c].push(word);
            }
        }
    }
    for (r, c, word) in &split_words {
        cell_words[*r][*c].push(word);
    }
    let mut visited = vec![vec![false; cols]; rows];
    for row in 0..rows {
        for col in 0..cols {
            if visited[row][col] {
                continue;
            }
            // Join only slots sharing a missing separator; each component must
            // be rectangular before it can be emitted as a single HTML cell.
            let mut pending = vec![(row, col)];
            let mut count = 0;
            let mut last_row = row;
            let mut last_col = col;
            let mut words = Vec::new();
            let mut exposed = false;
            while let Some((r, c)) = pending.pop() {
                if c < col {
                    return Err(format!(
                        "Merged cell crosses its starting column (row {}, column {})",
                        row + 1,
                        col + 1
                    ));
                }
                if std::mem::replace(&mut visited[r][c], true) {
                    continue;
                }
                count += 1;
                last_row = last_row.max(r);
                last_col = last_col.max(c);
                words.append(&mut cell_words[r][c]);
                if recover_open_frame {
                    exposed |= (r == 0 && !horizontal(r, c))
                        || (r + 1 == rows && !horizontal(rows, c))
                        || (c == 0 && !vertical(r, c))
                        || (c + 1 == cols && !vertical(r, cols));
                }
                if r > 0 && !horizontal(r, c) {
                    pending.push((r - 1, c));
                }
                if r + 1 < rows && !horizontal(r + 1, c) {
                    pending.push((r + 1, c));
                }
                if c > 0 && !vertical(r, c) {
                    pending.push((r, c - 1));
                }
                if c + 1 < cols && !vertical(r, c + 1) {
                    pending.push((r, c + 1));
                }
            }
            let (height, width) = (last_row - row + 1, last_col - col + 1);
            if count != height * width {
                return Err(format!(
                    "Nonrectangular merged cell (row {}, column {})",
                    row + 1,
                    col + 1
                ));
            }
            // An open, empty part of the perimeter can be represented as
            // a blank slot. Every cell containing data must be closed.
            if exposed && !words.is_empty() {
                return Err("Text touches the open outer border".into());
            }
            table.rows[row].cells.push(TableCell {
                text: read_text(words),
                spans: Vec::new(),
                colspan: width as u32,
                rowspan: height as u32,
                mcids: Vec::new(),
                bbox: Some(Rect::from_points(
                    xs[col].coord,
                    ys[last_row + 1].coord,
                    xs[last_col + 1].coord,
                    ys[row].coord,
                )),
                is_header: row == 0,
            });
        }
    }
    Ok(table)
}
