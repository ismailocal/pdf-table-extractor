mod borderless;
mod horizontal;
mod page_frame;
mod ruled;
mod source_text;
mod tagged;

use pdf_oxide::PdfDocument;
use pdf_oxide::config::ExtractionProfile;
use pdf_oxide::geometry::Rect;
use pdf_oxide::structure::table_extractor::Table;
use serde::Deserialize;
use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt::Write;
use std::fs;
use std::path::Path;
use std::time::Instant;

#[derive(Deserialize)]
struct Benchmark {
    documents: BTreeMap<String, String>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    document: String,
    page: usize,
}

struct PageTables {
    page: usize,
    bounds: Option<Rect>,
    tables: Vec<Table>,
    tagged: Vec<tagged::Fragment>,
}

impl PageTables {
    fn export(&self, output: &Path, document: &str) -> Result<(), Box<dyn Error>> {
        for (index, table) in self.tables.iter().enumerate() {
            let stem = format!("{document}-page-{}-table-{}", self.page, index + 1);
            export_table(output, &stem, table, &[])?;
        }
        Ok(())
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn bbox(rect: Option<Rect>) -> String {
    match rect {
        Some(r) => format!("{:.2} {:.2} {:.2} {:.2}", r.x, r.y, r.width, r.height),
        None => String::new(),
    }
}

fn html_header(title: &str) -> String {
    let title = escape_html(title);
    format!(
        "<!doctype html>\n<html lang=\"en\"><meta charset=\"utf-8\">\n\
         <title>{title}</title>\n<style>\n\
         body {{ font: 16px system-ui, sans-serif; margin: 24px; }}\n\
         table {{ border-collapse: collapse; }}\n\
         td, th {{ border: 1px solid #666; padding: 6px; white-space: pre-wrap; }}\n\
         </style>\n",
    )
}

fn append_table(
    html: &mut String,
    txt: &mut String,
    stem: &str,
    table: &Table,
    row_pages: &[(usize, usize)],
) -> Result<(), Box<dyn Error>> {
    writeln!(
        html,
        "<table data-columns=\"{}\" data-bbox=\"{}\">",
        table.col_count,
        bbox(table.bbox)
    )?;
    writeln!(
        txt,
        "# {stem} | Rows: {} | Columns: {}",
        table.rows.len(),
        table.col_count
    )?;
    // Occupied columns from earlier rowspan anchors are skipped by HTML.
    // Track them here so the TXT can name each cell's actual column.
    let mut occupied_until: Vec<usize> = vec![0; table.col_count];

    for (row_index, row) in table.rows.iter().enumerate() {
        let mut column = 0;
        match row_pages.get(row_index) {
            Some(&(first, last)) if first == last => write!(html, "  <tr data-page=\"{first}\">")?,
            Some(&(first, last)) => write!(
                html,
                "  <tr data-first-page=\"{first}\" data-last-page=\"{last}\">"
            )?,
            None => html.push_str("  <tr>"),
        }
        write!(txt, "{:02}", row_index + 1)?;

        for cell in &row.cells {
            let width = cell.colspan as usize;
            while occupied_until
                .iter()
                .skip(column)
                .take(width)
                .any(|until| *until > row_index)
            {
                column += 1;
            }
            if column + width > occupied_until.len() {
                occupied_until.resize(column + width, 0);
            }
            occupied_until[column..column + width].fill(row_index + cell.rowspan as usize);

            let tag = if cell.is_header { "th" } else { "td" };
            write!(
                html,
                "<{tag} rowspan=\"{}\" colspan=\"{}\" data-bbox=\"{}\">{}</{tag}>",
                cell.rowspan,
                cell.colspan,
                bbox(cell.bbox),
                escape_html(&cell.text)
            )?;

            write!(txt, "\tc{}", column + 1)?;
            if cell.rowspan != 1 || cell.colspan != 1 {
                write!(txt, "[rowspan={} colspan={}]", cell.rowspan, cell.colspan)?;
            }
            write!(txt, "={}", serde_json::to_string(&cell.text)?)?;
            column += width;
        }
        html.push_str("</tr>\n");
        txt.push('\n');
    }
    html.push_str("</table>\n");
    Ok(())
}

fn export_table(
    output: &Path,
    stem: &str,
    table: &Table,
    row_pages: &[(usize, usize)],
) -> Result<(), Box<dyn Error>> {
    let mut html = html_header(stem);
    let mut txt = String::new();
    append_table(&mut html, &mut txt, stem, table, row_pages)?;
    html.push_str("</html>\n");
    fs::write(output.join(format!("{stem}.html")), html)?;
    fs::write(output.join(format!("{stem}.txt")), txt)?;
    Ok(())
}

fn extract_auto_page(
    pdf: &PdfDocument,
    page_index: usize,
    page: usize,
    id: &str,
    pending: &mut Option<PageTables>,
) -> Result<Option<PageTables>, Box<dyn Error>> {
    let page_start = Instant::now();
    let mut paths = pdf.extract_paths(page_index)?;
    let rotation = pdf.get_page_rotation(page_index)?;
    let mut words = if rotation == 180 {
        // Paths stay in PDF user space. The default word API maps half-turn
        // pages to display coordinates; the profile API keeps raw positions.
        pdf.extract_words_with_thresholds(page_index, None, Some(ExtractionProfile::CONSERVATIVE))?
    } else {
        pdf.extract_words(page_index)?
    };
    let raw_chars = OnceCell::new();
    let raw_glyphs = || {
        raw_chars
            .get_or_init(|| pdf.extract_chars(page_index))
            .as_deref()
            .map_err(ToString::to_string)
    };
    source_text::verify_spaced_glyphs(&mut words, raw_glyphs)?;
    let mut bounds = pdf
        .get_page_visible_box(page_index)
        .ok()
        .map(|(x0, y0, x1, y1)| Rect::from_points(x0, y0, x1, y1));
    let upright_chars = page_frame::normalize_quarter_turn(
        pdf,
        page_index,
        rotation,
        &mut paths,
        &mut words,
        &mut bounds,
        raw_glyphs,
    )?;
    let glyphs = || match upright_chars.as_deref() {
        Some(chars) => Ok(chars),
        None => raw_glyphs(),
    };
    let read_glyphs = || {
        glyphs()
            .map_err(|err| eprintln!("{id}/{page}: could not read source characters ({err})"))
            .ok()
    };
    let mut tables = Vec::new();
    let mut rejected = 0;
    let text = borderless::PageText::new(&words);
    let rules = ruled::PageRules::new(&paths);
    for candidate in ruled::detect(&paths, &rules, &text, read_glyphs) {
        match candidate {
            Ok(table) => tables.push(table),
            Err(error) => {
                rejected += 1;
                eprintln!("{id}/{page}: rejected candidate ({error})");
            }
        }
    }
    let tagged = tagged::detect(pdf, page_index, &words).unwrap_or_default();
    for fragment in &tagged {
        if let Some(table) = fragment.page_table() {
            tagged::merge_table(&mut tables, table);
        }
    }
    let occupied = tables.iter().filter_map(|table| table.bbox).collect();
    tables.extend(borderless::detect_sparse(
        &text.lines,
        occupied,
        read_glyphs,
    ));
    for (bounds, lines) in ruled::horizontal_frames(&rules) {
        let Some(fragments) = horizontal::overlapping_fragments(&tables, bounds) else {
            continue;
        };
        let glyphs = glyphs()
            .map_err(|err| format!("{id}/{page}: could not read source characters ({err})"))?;
        if let Some(table) = horizontal::read_table(bounds, &lines, glyphs) {
            horizontal::select_table(&mut tables, table, bounds, fragments);
        }
    }
    let occupied = tables.iter().filter_map(|table| table.bbox).collect();
    tables.extend(borderless::detect_numeric_blocks(&text.lines, occupied));
    tables.sort_by(|a, b| {
        let (a, b) = (a.bbox.expect("table bounds"), b.bbox.expect("table bounds"));
        (b.y + b.height)
            .total_cmp(&(a.y + a.height))
            .then_with(|| a.x.total_cmp(&b.x))
    });
    let mut previous = pending.take();
    if let Some(previous) = &mut previous {
        if previous.page.checked_add(1) == Some(page)
            && let (Some(previous_bounds), Some(bounds), Some(table)) =
                (previous.bounds, bounds, previous.tables.last_mut())
            && ruled::join_continuation(table, previous_bounds, bounds, &rules, &words, &tables)
        {
            println!(
                "{id}/{page}: attached continuation row to page {}, table {}",
                previous.page,
                previous.tables.len()
            );
        }
    }
    println!(
        "{id}/{page}: tables: {}, rejected candidates: {rejected}, {:.3} s",
        tables.len(),
        page_start.elapsed().as_secs_f64()
    );
    *pending = Some(PageTables {
        page,
        bounds,
        tables,
        tagged,
    });
    Ok(previous)
}

fn export_document(pages: &[PageTables], output: &Path, title: &str) -> Result<(), Box<dyn Error>> {
    let mut groups = Vec::<Vec<(usize, &tagged::Fragment)>>::new();
    let mut group_by_reference = HashMap::new();
    for (page, data) in pages.iter().enumerate() {
        for fragment in &data.tagged {
            let group = group_by_reference
                .entry(fragment.reference)
                .or_insert_with(|| {
                    groups.push(Vec::new());
                    groups.len() - 1
                });
            groups[*group].push((page, fragment));
        }
    }
    let mut suppressed = HashSet::new();
    let mut combined = BTreeMap::new();
    let mut next_index: Vec<_> = pages.iter().map(|page| page.tables.len()).collect();
    for group in groups.iter().filter(|group| group.len() > 1) {
        let fragments: Vec<_> = group
            .iter()
            .map(|&(page, fragment)| (pages[page].page, fragment))
            .collect();
        let Some((table, row_pages)) = tagged::join_fragments(&fragments) else {
            continue;
        };
        let start_page = group[0].0;
        let mut replaced = Vec::new();
        let mut conflict = false;
        for &(page, fragment) in group {
            let bounds = fragment.table.bbox.expect("tagged fragment bounds");
            let overlapping: Vec<_> = pages[page]
                .tables
                .iter()
                .enumerate()
                .filter(|(_, candidate)| {
                    candidate.bbox.is_some_and(|rect| rect.intersects(&bounds))
                })
                .collect();
            if overlapping
                .iter()
                .any(|&(index, _)| suppressed.contains(&(page, index)))
                || !tagged::covers_fragment(
                    &fragment.table,
                    &table,
                    &overlapping
                        .iter()
                        .map(|&(_, table)| table)
                        .collect::<Vec<_>>(),
                    page != start_page,
                )
            {
                conflict = true;
                break;
            }
            replaced.extend(overlapping.into_iter().map(|(index, _)| (page, index)));
        }
        if conflict {
            continue;
        }
        let index = replaced
            .first()
            .filter(|&&(page, _)| page == start_page)
            .map(|&(_, index)| index)
            .unwrap_or_else(|| {
                let index = next_index[start_page];
                next_index[start_page] += 1;
                index
            });
        suppressed.extend(replaced);
        combined.insert((start_page, index), (table, row_pages));
        println!(
            "Pages {}-{}: joined tagged table fragments",
            pages[start_page].page,
            pages[group[group.len() - 1].0].page
        );
    }
    let mut html = html_header(title);
    let mut txt = String::new();
    for (page, data) in pages.iter().enumerate() {
        for index in 0..next_index[page] {
            let stem = format!("page-{}-table-{}", data.page, index + 1);
            let (table, row_pages) = if let Some((table, row_pages)) = combined.get(&(page, index))
            {
                (table, row_pages.as_slice())
            } else if suppressed.contains(&(page, index)) {
                continue;
            } else {
                (&data.tables[index], &[][..])
            };
            writeln!(
                html,
                "<h2 id=\"{stem}\">Page {} - Table {}</h2>",
                data.page,
                index + 1
            )?;
            if !txt.is_empty() {
                txt.push('\n');
            }
            append_table(&mut html, &mut txt, &stem, table, row_pages)?;
        }
    }
    if txt.is_empty() {
        html.push_str("<p>No tables found.</p>\n");
        txt.push_str("No tables found.\n");
    }
    html.push_str("</html>\n");
    fs::write(output.join("tables.html"), html)?;
    fs::write(output.join("tables.txt"), txt)?;
    Ok(())
}

fn extract_pdf(path: &Path, output: &Path) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    let pdf = PdfDocument::open(path)?;
    let page_count = pdf.page_count()?;
    let title = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    fs::create_dir_all(output)?;
    let mut pending = None;
    let mut pages = Vec::new();
    for page in 1..=page_count {
        match extract_auto_page(&pdf, page - 1, page, "1", &mut pending) {
            Ok(previous) => pages.extend(previous),
            Err(error) => {
                pages.extend(pending);
                export_document(&pages, output, &title)?;
                return Err(error);
            }
        }
    }
    pages.extend(pending);
    export_document(&pages, output, &title)?;
    println!("Total: {:.3} s", started.elapsed().as_secs_f64());
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().collect();
    if let [_, flag, pdf, output] = args.as_slice()
        && flag == "--pdf"
    {
        return extract_pdf(Path::new(pdf), Path::new(output));
    }
    let [_, benchmark_path, output_dir, mode, copies @ ..] = args.as_slice() else {
        return Err(
            "Usage:\n  pdf-table-extractor --pdf INPUT.pdf OUTPUT_DIR\n  pdf-table-extractor MANIFEST.json OUTPUT_DIR default|auto|REGIONS.json [PAGE_PDF_DIR]"
                .into(),
        );
    };
    let copies = match copies {
        [] => None,
        [path] => Some(Path::new(path)),
        _ => return Err("Specify at most one directory of single-page PDFs".into()),
    };
    let regions: Option<BTreeMap<String, [f32; 4]>> = match mode.as_str() {
        "default" | "auto" => None,
        _ => Some(serde_json::from_slice(&fs::read(mode)?)?),
    };
    let benchmark_path = Path::new(benchmark_path);
    let benchmark: Benchmark = serde_json::from_slice(&fs::read(benchmark_path)?)?;
    let output = Path::new(output_dir);
    fs::create_dir_all(output)?;

    let mut pages_by_document = BTreeMap::<_, BTreeMap<_, Vec<_>>>::new();
    for case in benchmark.cases {
        if case.id.is_empty()
            || !case
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(format!("Invalid table ID: {}", case.id).into());
        }
        pages_by_document
            .entry(case.document)
            .or_default()
            .entry(case.page)
            .or_default()
            .push(case.id);
    }

    let started = Instant::now();
    for (id, pages) in pages_by_document {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("Invalid document ID: {id}").into());
        }
        let filename = benchmark
            .documents
            .get(&id)
            .ok_or_else(|| format!("Document not found: {id}"))?;
        let path = benchmark_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(filename);
        let original = if copies.is_none() {
            Some(PdfDocument::open(&path)?)
        } else {
            None
        };
        let mut pending: Option<PageTables> = None;
        for (page, cases) in pages {
            let page_start = Instant::now();
            let physical_index = page.checked_sub(1).ok_or("Page numbers start at 1")?;
            let page_copy;
            let (pdf, page_index) = if let Some(dir) = copies {
                page_copy = PdfDocument::open(dir.join(format!("{id}-page-{page}.pdf")))?;
                (&page_copy, 0)
            } else {
                (
                    original.as_ref().expect("Original PDF is open"),
                    physical_index,
                )
            };
            if mode == "auto" {
                if let Some(previous) = extract_auto_page(pdf, page_index, page, &id, &mut pending)?
                {
                    previous.export(output, &id)?;
                }
            } else if let Some(regions) = &regions {
                let paths = pdf.extract_paths(page_index)?;
                let words = pdf.extract_words(page_index)?;
                let glyphs = OnceCell::new();
                for case in &cases {
                    let region = regions
                        .get(case)
                        .ok_or_else(|| format!("Missing table region: {case}"))?;
                    let mut table = ruled::reconstruct(&paths, &words, *region)
                        .map_err(|error| format!("{case}: {error}"))?;
                    source_text::restore_rtl_cells(&mut table, || {
                        glyphs
                            .get_or_init(|| pdf.extract_chars(page_index))
                            .as_deref()
                            .map_err(|error| {
                                eprintln!("{case}: could not read source characters ({error})")
                            })
                            .ok()
                    });
                    export_table(output, case, &table, &[])?;
                }
                println!(
                    "{id}/{page}: regions: {}, {:.3} s",
                    cases.len(),
                    page_start.elapsed().as_secs_f64()
                );
            } else {
                let tables = pdf.extract_tables(page_index)?;
                for (index, table) in tables.iter().enumerate() {
                    let stem = format!("{id}-page-{page}-table-{}", index + 1);
                    export_table(output, &stem, table, &[])?;
                }
                println!(
                    "{id}/{page}: candidates: {}, {:.3} s",
                    tables.len(),
                    page_start.elapsed().as_secs_f64()
                );
            }
        }
        if let Some(last) = pending {
            last.export(output, &id)?;
        }
    }
    println!("Total: {:.3} s", started.elapsed().as_secs_f64());
    Ok(())
}
