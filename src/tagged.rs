use pdf_oxide::PdfDocument;
use pdf_oxide::geometry::Rect;
use pdf_oxide::layout::Word;
use pdf_oxide::object::{Object, ObjectRef};
use pdf_oxide::structure::McidScope;
use pdf_oxide::structure::table_extractor::{Table, TableCell, TableRow};
use std::collections::{HashMap, HashSet};

use crate::source_text::word_text;

type Dictionary = HashMap<String, Object>;

pub(crate) struct Fragment {
    pub reference: ObjectRef,
    pub table: Table,
    source_rows: Vec<usize>,
    total_rows: usize,
}

impl Fragment {
    pub fn page_table(&self) -> Option<Table> {
        let mut table = self.table.clone();
        validate_grid(&mut table, 2)?;
        Some(table)
    }
}

fn number_tree_value(
    pdf: &PdfDocument,
    node: &Object,
    key: i64,
    visited: &mut HashSet<ObjectRef>,
    depth: usize,
) -> Option<Object> {
    if depth == 64
        || node
            .as_reference()
            .is_some_and(|reference| !visited.insert(reference))
    {
        return None;
    }
    let node = pdf.resolve_object(node).ok()?;
    let node = node.as_dict()?;
    if let Some(limits) = node.get("Limits").and_then(Object::as_array)
        && let [Some(low), Some(high)] = [
            limits.first().and_then(Object::as_integer),
            limits.get(1).and_then(Object::as_integer),
        ]
        && !(low..=high).contains(&key)
    {
        return None;
    }
    if let Some(entries) = node
        .get("Nums")
        .and_then(|value| pdf.resolve_object(value).ok())
        && let Some(entries) = entries.as_array()
    {
        return entries
            .chunks_exact(2)
            .find(|pair| pair[0].as_integer() == Some(key))
            .and_then(|pair| pdf.resolve_object(&pair[1]).ok());
    }
    let kids = pdf.resolve_object(node.get("Kids")?).ok()?;
    kids.as_array()?
        .iter()
        .find_map(|child| number_tree_value(pdf, child, key, visited, depth + 1))
}

fn role<'a>(element: &'a Dictionary, roles: Option<&'a Dictionary>) -> Option<&'a str> {
    let name = element.get("S")?.as_name()?;
    Some(
        roles
            .and_then(|map| map.get(name))
            .and_then(Object::as_name)
            .unwrap_or(name),
    )
}

fn owning_cell(
    pdf: &PdfDocument,
    owner: &Object,
    roles: Option<&Dictionary>,
) -> Option<(ObjectRef, ObjectRef)> {
    let mut reference = owner.as_reference()?;
    let mut cell = None;
    for _ in 0..64 {
        let element = pdf.load_object(reference).ok()?;
        let element = element.as_dict()?;
        match role(element, roles) {
            Some("TD" | "TH") if cell.is_none() => cell = Some(reference),
            Some("Table") => return Some((reference, cell?)),
            _ => {}
        }
        reference = element.get("P")?.as_reference()?;
    }
    None
}

fn children(pdf: &PdfDocument, element: &Dictionary) -> Option<Vec<Object>> {
    match element.get("K") {
        None => Some(Vec::new()),
        Some(k) => match pdf.resolve_object(k).ok()? {
            Object::Array(children) => Some(children),
            // Keep the reference so words from the parent tree can match it.
            _ => Some(vec![k.clone()]),
        },
    }
}

fn attribute_entries(pdf: &PdfDocument, entry: Option<&Object>) -> Option<Vec<(Object, i64)>> {
    let Some(entry) = entry else {
        return Some(Vec::new());
    };
    let items = match pdf.resolve_object(entry).ok()? {
        Object::Array(items) => items,
        item => return Some(vec![(item, 0)]),
    };
    let mut items = items.iter().map(|item| pdf.resolve_object(item)).peekable();
    let mut entries = Vec::new();
    while let Some(item) = items.next() {
        let item = item.ok()?;
        if item.as_integer().is_some() {
            return None;
        }
        let mut revision = 0;
        if let Some(Ok(Object::Integer(n))) = items.peek() {
            revision = *n;
            items.next();
        }
        entries.push((item, revision));
    }
    Some(entries)
}

fn span_value(
    pdf: &PdfDocument,
    name: &str,
    direct: &[(Object, i64)],
    classes: &[(Object, i64)],
) -> Option<u32> {
    // ISO 32000-1 §14.7.5: a direct attribute takes precedence over
    // a class. Array entries are unordered apart from their revisions.
    for attributes in [direct, classes] {
        let candidates = attributes.iter().filter_map(|(attribute, revision)| {
            let attribute = attribute.as_dict()?;
            (attribute.get("O")?.as_name()? == "Table").then_some((attribute.get(name)?, *revision))
        });
        let Some(latest) = candidates.clone().map(|(_, revision)| revision).max() else {
            continue;
        };
        let mut values =
            candidates
                .filter(|&(_, revision)| revision == latest)
                .map(|(value, _)| {
                    u32::try_from(pdf.resolve_object(value).ok()?.as_integer()?)
                        .ok()
                        .filter(|&value| value > 0)
                });
        let first = values.next()??;
        return values.all(|value| value == Some(first)).then_some(first);
    }
    Some(1)
}

fn spans(
    pdf: &PdfDocument,
    cell: &Dictionary,
    class_map: Option<&Dictionary>,
) -> Option<(u32, u32)> {
    let direct = attribute_entries(pdf, cell.get("A"))?;
    let mut classes = Vec::new();
    for (class, revision) in attribute_entries(pdf, cell.get("C"))? {
        let attributes = class_map?.get(class.as_name()?)?;
        classes.extend(
            attribute_entries(pdf, Some(attributes))?
                .into_iter()
                .map(|(value, _)| (value, revision)),
        );
    }
    Some((
        span_value(pdf, "RowSpan", &direct, &classes)?,
        span_value(pdf, "ColSpan", &direct, &classes)?,
    ))
}

fn page_key(pdf: &PdfDocument, page: &Object) -> Option<i64> {
    pdf.resolve_object(page)
        .ok()?
        .as_dict()?
        .get("StructParents")?
        .as_integer()
}

fn read_table(
    pdf: &PdfDocument,
    reference: ObjectRef,
    current_page: Option<i64>,
    roles: Option<&Dictionary>,
    class_map: Option<&Dictionary>,
    content: &HashMap<ObjectRef, Vec<&Word>>,
) -> Option<Fragment> {
    let source = pdf.load_object(reference).ok()?;
    let source = source.as_dict()?;
    let table_page = source.get("Pg").and_then(|pg| page_key(pdf, pg));
    let mut row_objects = Vec::new();
    for child in children(pdf, source)? {
        let element = pdf.resolve_object(&child).ok()?;
        let element = element.as_dict()?;
        match role(element, roles)? {
            "TR" => row_objects.push((child, false, table_page)),
            kind @ ("THead" | "TBody" | "TFoot") => {
                let group_page = element
                    .get("Pg")
                    .and_then(|pg| page_key(pdf, pg))
                    .or(table_page);
                row_objects.extend(
                    children(pdf, element)?
                        .into_iter()
                        .map(|row| (row, kind == "THead", group_page)),
                );
            }
            "Caption" => {}
            _ => return None,
        }
    }

    let mut table = Table::new();
    let mut seen_cells = HashSet::new();
    let mut word_count = 0;
    let total_rows = row_objects.len();
    let mut source_rows = Vec::new();
    for (index, (row, header, inherited_page)) in row_objects.into_iter().enumerate() {
        let row = pdf.resolve_object(&row).ok()?;
        let row = row.as_dict()?;
        if role(row, roles)? != "TR" {
            return None;
        }
        let row_page = row
            .get("Pg")
            .and_then(|pg| page_key(pdf, pg))
            .or(inherited_page);
        let belongs_to_page = current_page.is_some() && row_page == current_page;
        let children = children(pdf, row)?;
        if !belongs_to_page
            && !children.iter().any(|child| {
                child
                    .as_reference()
                    .is_some_and(|reference| content.contains_key(&reference))
            })
        {
            continue;
        }
        let mut row_cells = TableRow::new(header);
        let mut has_text = false;
        for child in children {
            let cell = pdf.resolve_object(&child).ok()?;
            let cell = cell.as_dict()?;
            let kind = role(cell, roles)?;
            if !matches!(kind, "TD" | "TH") {
                return None;
            }
            if child.as_reference().is_some_and(|r| !seen_cells.insert(r)) {
                return None;
            }
            let words = child
                .as_reference()
                .and_then(|r| content.get(&r))
                .map_or(&[][..], Vec::as_slice);
            word_count += words.len();
            let (rowspan, colspan) = spans(pdf, cell, class_map)?;
            let bbox = words
                .iter()
                .map(|word| word.bbox)
                .reduce(|a, b| a.union(&b));
            has_text |= bbox.is_some();
            let mut cell = TableCell::new(word_text(words.to_vec()), header || kind == "TH");
            cell.rowspan = rowspan;
            cell.colspan = colspan;
            cell.bbox = bbox;
            row_cells.add_cell(cell);
        }
        if has_text || belongs_to_page {
            table.rows.push(row_cells);
            source_rows.push(index);
        }
    }
    if word_count != content.values().map(Vec::len).sum::<usize>() {
        return None;
    }

    table.bbox = table
        .rows
        .iter()
        .flat_map(|row| &row.cells)
        .filter_map(|cell| cell.bbox)
        .reduce(|a, b| a.union(&b));
    table.bbox?;
    Some(Fragment {
        reference,
        table,
        source_rows,
        total_rows,
    })
}

fn validate_grid(table: &mut Table, min_rows: usize) -> Option<()> {
    // A span can otherwise move every following value into the wrong column.
    let columns = table
        .rows
        .first()?
        .cells
        .iter()
        .try_fold(0usize, |sum, cell| sum.checked_add(cell.colspan as usize))?;
    if columns < 2 || columns > 4096 || table.rows.len() < min_rows || table.bbox.is_none() {
        return None;
    }
    let mut occupied_until = vec![0; columns];
    for (i, row) in table.rows.iter().enumerate() {
        let mut start = 0;
        for cell in &row.cells {
            while start < columns && occupied_until[start] > i {
                start += 1;
            }
            let end = start + cell.colspan as usize;
            let until = i + cell.rowspan as usize;
            if end > columns
                || until > table.rows.len()
                || occupied_until[start..end].iter().any(|&n| n > i)
            {
                return None;
            }
            occupied_until[start..end].fill(until);
            start = end;
        }
        if occupied_until.iter().any(|&n| n <= i) {
            return None;
        }
    }
    table.col_count = columns;
    table.has_header = table
        .rows
        .iter()
        .any(|row| row.cells.iter().any(|cell| cell.is_header));
    Some(())
}

fn scope_for_word<'a>(regions: &[(&'a McidScope, Rect)], word: &Word) -> Option<&'a McidScope> {
    let first = regions.first()?.0;
    if regions.iter().all(|&(scope, _)| scope == first) {
        return Some(first);
    }
    // Word stores the numeric MCID only. For an ID shared by two streams,
    // locate the word inside its original span before reading the owner.
    let mut matches = regions.iter().filter_map(|&(scope, bounds)| {
        const TOLERANCE: f32 = 0.5;
        Rect::new(
            bounds.x - TOLERANCE,
            bounds.y - TOLERANCE,
            bounds.width + 2.0 * TOLERANCE,
            bounds.height + 2.0 * TOLERANCE,
        )
        .contains_rect(&word.bbox)
        .then_some(scope)
    });
    let first = matches.next()?;
    matches.all(|scope| scope == first).then_some(first)
}

pub(crate) fn detect(
    pdf: &PdfDocument,
    page_index: usize,
    words: &[Word],
) -> Option<Vec<Fragment>> {
    if !words.iter().any(|word| word.mcid.is_some()) {
        return None;
    }
    let page = page_key(pdf, &pdf.get_page(page_index).ok()?);
    let catalog = pdf.catalog().ok()?;
    let root = pdf
        .resolve_object(catalog.as_dict()?.get("StructTreeRoot")?)
        .ok()?;
    let root = root.as_dict()?;
    let roles = root
        .get("RoleMap")
        .and_then(|map| pdf.resolve_object(map).ok());
    let roles = roles.as_ref().and_then(Object::as_dict);
    let class_map = root
        .get("ClassMap")
        .and_then(|map| pdf.resolve_object(map).ok());
    let class_map = class_map.as_ref().and_then(Object::as_dict);
    let parent_tree = root.get("ParentTree")?;
    let spans = pdf.extract_spans(page_index).ok()?;
    let mut regions = HashMap::<u32, Vec<(&McidScope, Rect)>>::new();
    for span in &spans {
        if let (Some(mcid), Some(scope)) = (span.mcid, span.mcid_scope.as_ref()) {
            regions.entry(mcid).or_default().push((scope, span.bbox));
        }
    }
    let mut parent_arrays = HashMap::new();
    let mut tables = Vec::new();
    let mut owners = HashMap::new();
    let mut content = HashMap::<ObjectRef, HashMap<ObjectRef, Vec<&Word>>>::new();
    for word in words {
        let Some(mcid) = word.mcid else { continue };
        let Some(regions) = regions.get(&mcid) else {
            continue;
        };
        let scope = scope_for_word(regions, word)?;
        let owner = owners.entry((scope, mcid)).or_insert_with(|| {
            let parents = parent_arrays.entry(scope).or_insert_with(|| {
                let key = match *scope {
                    McidScope::Page(n) if n as usize == page_index => page,
                    McidScope::Page(_) => None,
                    McidScope::Form(reference) | McidScope::Pattern(reference) => {
                        page_key(pdf, &Object::Reference(reference))
                    }
                }?;
                number_tree_value(pdf, parent_tree, key, &mut HashSet::new(), 0)
            });
            parents
                .as_ref()?
                .as_array()?
                .get(mcid as usize)
                .and_then(|owner| owning_cell(pdf, owner, roles))
        });
        if let Some((table, cell)) = *owner {
            let cells = content.entry(table).or_insert_with(|| {
                tables.push(table);
                HashMap::new()
            });
            cells.entry(cell).or_default().push(word);
        }
    }
    Some(
        tables
            .into_iter()
            .filter_map(|table| read_table(pdf, table, page, roles, class_map, &content[&table]))
            .collect(),
    )
}

fn same_header(a: &TableRow, b: &TableRow) -> bool {
    !b.cells.is_empty()
        && a.cells.len() == b.cells.len()
        && a.cells.iter().zip(&b.cells).all(|(a, b)| {
            a.is_header
                && b.is_header
                && a.text == b.text
                && a.rowspan == b.rowspan
                && a.colspan == b.colspan
        })
}

fn remove_repeated_headers(table: &mut Table, row_pages: &mut Vec<(usize, usize)>) {
    let count = table
        .rows
        .iter()
        .take_while(|row| same_header(row, row))
        .count();
    if count == 0
        || table.rows[..count].iter().enumerate().any(|(i, row)| {
            row.cells
                .iter()
                .any(|cell| i + cell.rowspan as usize > count)
        })
    {
        return;
    }
    let mut keep = vec![true; table.rows.len()];
    let mut i = count;
    while i + count < table.rows.len() {
        let page = row_pages[i].0;
        if page > row_pages[i - 1].1
            && row_pages[i..i + count]
                .iter()
                .all(|&(a, b)| a == page && b == page)
            && table.rows[..count]
                .iter()
                .zip(&table.rows[i..i + count])
                .all(|(a, b)| same_header(a, b))
        {
            keep[i..i + count].fill(false);
            i += count;
        } else {
            i += 1;
        }
    }
    let mut decisions = keep.iter();
    table
        .rows
        .retain(|_| *decisions.next().expect("row retention"));
    let mut decisions = keep.iter();
    row_pages.retain(|_| *decisions.next().expect("row provenance retention"));
}

pub(crate) fn join_fragments(
    fragments: &[(usize, &Fragment)],
) -> Option<(Table, Vec<(usize, usize)>)> {
    let (_, first) = fragments.first()?;
    let mut table = Table::new();
    table.bbox = first.table.bbox;
    let mut row_pages = Vec::new();
    for &(page, fragment) in fragments {
        if fragment.total_rows != first.total_rows {
            return None;
        }
        for (&index, row) in fragment.source_rows.iter().zip(&fragment.table.rows) {
            if index == table.rows.len() {
                table.rows.push(row.clone());
                row_pages.push((page, page));
            } else if table
                .rows
                .get(index)
                .is_some_and(|previous| same_header(previous, row))
            {
                // A repeated header with the same source row is displayed
                // again on the new page but is already in the logical table.
                continue;
            } else if index.checked_add(1) == Some(table.rows.len())
                && let Some(previous) = table.rows.last_mut()
                && previous.cells.len() == row.cells.len()
                && previous.cells.iter().zip(&row.cells).all(|(a, b)| {
                    a.rowspan == b.rowspan && a.colspan == b.colspan && a.is_header == b.is_header
                })
            {
                // The PDF references the same logical row on both pages.
                // A split cell has no single page-local bounding box.
                for (a, b) in previous.cells.iter_mut().zip(&row.cells) {
                    if !b.text.is_empty() {
                        if !a.text.is_empty() {
                            a.text.push('\n');
                        }
                        a.text.push_str(&b.text);
                    }
                    a.bbox = None;
                }
                row_pages.last_mut()?.1 = page;
            } else {
                return None;
            }
        }
    }
    if table.rows.len() != first.total_rows {
        return None;
    }
    validate_grid(&mut table, 2)?;
    remove_repeated_headers(&mut table, &mut row_pages);
    Some((table, row_pages))
}

pub(crate) fn merge_table(tables: &mut Vec<Table>, tagged: Table) {
    let Some(bounds) = tagged.bbox else { return };
    let mut overlapping = tables
        .iter()
        .enumerate()
        .filter(|(_, table)| table.bbox.is_some_and(|bbox| bbox.intersects(&bounds)));
    let Some((index, current)) = overlapping.next() else {
        tables.push(tagged);
        return;
    };
    if overlapping.next().is_some()
        || !current.bbox.is_some_and(|bbox| bounds.contains_rect(&bbox))
        || current.col_count > tagged.col_count
        || current.rows.len() > tagged.rows.len()
        || current
            .rows
            .iter()
            .flat_map(|row| &row.cells)
            .any(|cell| cell.bbox.is_some())
    {
        return;
    }
    // An alignment-only table can end just before a row covered by a span.
    // Use the source structure when it accounts for every value already
    // detected in that region and supplies additional rows or spans.
    let additional_structure = tagged.rows.len() > current.rows.len()
        || tagged.col_count > current.col_count
        || tagged
            .rows
            .iter()
            .flat_map(|row| &row.cells)
            .any(|cell| cell.rowspan > 1 || cell.colspan > 1);
    if !additional_structure {
        return;
    }
    if covers_rows(&tagged, &current.rows) {
        tables[index] = tagged;
    }
}

pub(crate) fn covers_fragment(
    source: &Table,
    logical: &Table,
    existing: &[&Table],
    continuation: bool,
) -> bool {
    if covers_rows(source, existing.iter().flat_map(|table| &table.rows)) {
        return true;
    }
    let [existing] = existing else { return false };
    if !continuation {
        return false;
    }
    // A repeated header can be painted as an Artifact. Its text appears
    // in the physical table without belonging to a second logical row.
    let count = logical
        .rows
        .iter()
        .take_while(|row| same_header(row, row))
        .count();
    count > 0
        && existing.rows.len() > count
        && logical.rows[..count]
            .iter()
            .zip(&existing.rows[..count])
            .all(|(a, b)| same_header(a, b))
        && covers_rows(source, &existing.rows[count..])
}

fn covers_rows<'a>(source: &Table, existing: impl IntoIterator<Item = &'a TableRow>) -> bool {
    let mut available = HashMap::<&str, usize>::new();
    for token in source
        .rows
        .iter()
        .flat_map(|row| &row.cells)
        .flat_map(|cell| cell.text.split_whitespace())
    {
        *available.entry(token).or_default() += 1;
    }
    existing
        .into_iter()
        .flat_map(|row| &row.cells)
        .flat_map(|cell| cell.text.split_whitespace())
        .all(|token| {
            let Some(count) = available.get_mut(token).filter(|count| **count > 0) else {
                return false;
            };
            *count -= 1;
            true
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_values_beneath_a_display_only_repeated_header() {
        let header = TableRow {
            cells: ["Name", "Count"]
                .map(|text| TableCell::new(text.into(), true))
                .to_vec(),
            is_header: true,
        };
        let record = TableRow {
            cells: ["Item", "2"]
                .map(|text| TableCell::new(text.into(), false))
                .to_vec(),
            is_header: false,
        };
        let mut source = Table::new();
        source.rows.push(record.clone());
        let mut logical = Table::new();
        logical.rows.extend([header, record]);
        let mut displayed = logical.clone();
        assert!(covers_fragment(&source, &logical, &[&displayed], true));
        assert!(!covers_fragment(&source, &logical, &[&displayed], false));
        displayed.rows[1].cells[1].text = "3".into();
        assert!(!covers_fragment(&source, &logical, &[&displayed], true));
    }
}
