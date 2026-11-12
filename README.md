# PDF Table Extractor

Extract tables from text-based PDFs into HTML and plain text. This Rust CLI reconstructs each table's rows, columns, and cell boundaries from embedded text, drawn lines, and PDF structure tags.

## Table structures it handles

- **Merged cells and grouped headers.** Preserves cells that span multiple rows, columns, or both using `rowspan` and `colspan`.
- **Empty and multiline cells.** Keeps empty cells in their original columns and preserves line breaks inside a cell.
- **Ruled and borderless tables.** Uses drawn borders where available and text alignment for supported borderless layouts.
- **Tables across pages.** Can join fragments linked by PDF table tags and attach supported continuation rows to the preceding table.

The extractor uses the PDF's text layer. Image-only scans need OCR first.

## Quick start with Docker

From the project directory, build the image and run it on a PDF.

```sh
docker build -t pdf-table-extractor:local .
./pdf-table-extractor /path/to/document.pdf
```

## Try a public example

- **[Merged rows and columns](https://github.com/camelot-dev/camelot/blob/92777b852e00933a5c4b9ba642601784604356cd/tests/files/column_span_2.pdf)** has a cell that spans three rows and five columns.
- **[Grouped headers and empty cells](https://github.com/camelot-dev/camelot/blob/92777b852e00933a5c4b9ba642601784604356cd/tests/files/missing_values.pdf)** has a two-level header, empty cells in the last two columns, and a note spanning the full table width.

Download the merged-cell example from Camelot and run the extractor.

```sh
example_dir=$(mktemp -d)
curl -fL https://raw.githubusercontent.com/camelot-dev/camelot/92777b852e00933a5c4b9ba642601784604356cd/tests/files/column_span_2.pdf \
  -o "$example_dir/merged-cells.pdf"
./pdf-table-extractor "$example_dir/merged-cells.pdf"
```

## Output

The Docker wrapper creates a directory next to the PDF. Both files contain all detected tables in page order.

```text
merged-cells.pdf
merged-cells/
├── tables.html
└── tables.txt
```

`tables.html` shows the reconstructed grid in a browser. `tables.txt` records each cell's column and span. In the merged-cell example, the cell starting in column 3 is recorded as `c3[rowspan=3 colspan=5]="All the available individuals"`.

## Run with Cargo

```sh
cargo run --release --locked -- --pdf /path/to/document.pdf ./output
cargo test --locked
```
