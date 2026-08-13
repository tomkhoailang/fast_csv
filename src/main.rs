use std::collections::HashSet;
use std::env;
use std::io::Write;
use std::time::Instant;

use rayon::prelude::*;
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

fn get_col_letter(mut col_idx: usize) -> String {
    let mut result = Vec::new();
    loop {
        let rem = (col_idx % 26) as u8;
        result.push((b'A' + rem) as char);
        if col_idx < 26 {
            break;
        }
        col_idx = col_idx / 26 - 1;
    }
    result.into_iter().rev().collect()
}

#[inline(always)]
fn escape_xml(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

#[derive(Copy, Clone, PartialEq)]
enum ColType {
    Numeric,
    Text,
}

fn classify_columns(headers: &[String], records: &[Vec<String>]) -> Vec<ColType> {
    let sample_count = records.len().min(500);
    headers
        .iter()
        .enumerate()
        .map(|(c_idx, _)| {
            let mut numeric_count = 0;
            let mut total_non_empty = 0;
            for row in records.iter().take(sample_count) {
                if c_idx < row.len() {
                    let val = row[c_idx].trim();
                    if !val.is_empty() {
                        total_non_empty += 1;
                        if val.parse::<f64>().is_ok() {
                            numeric_count += 1;
                        }
                    }
                }
            }
            if total_non_empty > 0 && numeric_count == total_non_empty {
                ColType::Numeric
            } else {
                ColType::Text
            }
        })
        .collect()
}

fn render_rows_parallel(
    records: &[Vec<String>],
    col_letters: &[String],
    col_types: &[ColType],
    start_row_1based: usize,
) -> Vec<u8> {
    const CHUNK_SIZE: usize = 50_000;

    let chunk_results: Vec<String> = records
        .par_chunks(CHUNK_SIZE)
        .enumerate()
        .map(|(chunk_idx, chunk)| {
            let mut buf = String::with_capacity(chunk.len() * 300);
            let chunk_start_row = start_row_1based + chunk_idx * CHUNK_SIZE;

            for (r_idx, row) in chunk.iter().enumerate() {
                let row_num = chunk_start_row + r_idx;
                buf.push_str("<row r=\"");
                buf.push_str(&row_num.to_string());
                buf.push_str("\">");

                for (c_idx, val) in row.iter().enumerate() {
                    let trim_val = val.trim();
                    if trim_val.is_empty() {
                        continue;
                    }

                    let col_let = &col_letters[c_idx];
                    buf.push_str("<c r=\"");
                    buf.push_str(col_let);
                    buf.push_str(&row_num.to_string());

                    if col_types[c_idx] == ColType::Numeric {
                        buf.push_str("\"><v>");
                        buf.push_str(trim_val);
                        buf.push_str("</v></c>");
                    } else {
                        buf.push_str("\" t=\"inlineStr\"><is><t>");
                        escape_xml(trim_val, &mut buf);
                        buf.push_str("</t></is></c>");
                    }
                }

                buf.push_str("</row>");
            }
            buf
        })
        .collect();

    chunk_results.concat().into_bytes()
}

fn read_and_align_csvs(csv_paths: &[String]) -> (Vec<String>, Vec<Vec<String>>) {
    let mut headers = Vec::new();
    let mut header_set = HashSet::new();

    for path in csv_paths {
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .from_path(path)
            .unwrap_or_else(|e| panic!("Failed to open CSV file {}: {}", path, e));

        let hdr = rdr.headers().expect("Failed to read headers");
        for h in hdr {
            let h_str = h.to_string();
            if !header_set.contains(&h_str) {
                header_set.insert(h_str.clone());
                headers.push(h_str);
            }
        }
    }

    let mut all_rows = Vec::new();

    for path in csv_paths {
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .from_path(path)
            .unwrap_or_else(|e| panic!("Failed to open CSV file {}: {}", path, e));

        let file_headers: Vec<String> = rdr.headers().unwrap().iter().map(|s| s.to_string()).collect();
        let header_map: Vec<usize> = file_headers
            .iter()
            .map(|fh| headers.iter().position(|h| h == fh).unwrap())
            .collect();

        for result in rdr.records() {
            let record = result.expect("Failed to read CSV record");
            let mut aligned_row = vec![String::new(); headers.len()];
            for (idx, val) in record.iter().enumerate() {
                if idx < header_map.len() {
                    let target_idx = header_map[idx];
                    aligned_row[target_idx] = val.to_string();
                }
            }
            all_rows.push(aligned_row);
        }
    }

    (headers, all_rows)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let (csv_files, output_path) = if args.len() >= 3 && args[1] == "-o" {
        (args[3..].to_vec(), args[2].clone())
    } else if args.len() >= 2 {
        (args[1..].to_vec(), "output_fast.xlsx".to_string())
    } else {
        (
            vec![
                "output/sample_20260812_180359_Part_1.csv".to_string(),
                "output/sample_20260812_180359_Part_2.csv".to_string(),
                "output/sample_20260812_180359_Part_3.csv".to_string(),
            ],
            "output/sample_fast_rust_merged.xlsx".to_string(),
        )
    };

    println!("[START] High-Performance Native Rust Converter");
    let t_start = Instant::now();

    println!("[READ] Reading and aligning {} CSV files...", csv_files.len());
    let t_read_start = Instant::now();
    let (headers, rows) = read_and_align_csvs(&csv_files);
    let t_read = t_read_start.elapsed();
    println!("[READ COMPLETE] Read {} rows in {:.4}s", rows.len(), t_read.as_secs_f64());

    println!("[CLASSIFY] Classifying column types once...");
    let col_types = classify_columns(&headers, &rows);

    println!("[WRITE] Writing XLSX workbook to: {}...", output_path);
    let t_write_start = Instant::now();

    let file = std::fs::File::create(&output_path).expect("Failed to create output file");
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    let col_letters: Vec<String> = (0..headers.len()).map(get_col_letter).collect();
    let max_rows_per_sheet = 1_048_575;
    let sheet_chunks: Vec<&[Vec<String>]> = rows.chunks(max_rows_per_sheet).collect();
    let num_sheets = sheet_chunks.len();

    // 1. [Content_Types].xml
    let mut ct_overrides = String::new();
    for i in 1..=num_sheets {
        ct_overrides.push_str(&format!(
            "<Override PartName=\"/xl/worksheets/sheet{}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>",
            i
        ));
    }
    let ct_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
        <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
        <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
        <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
        <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
        <Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\
        {}\
        </Types>",
        ct_overrides
    );
    zip.start_file("[Content_Types].xml", options).unwrap();
    zip.write_all(ct_xml.as_bytes()).unwrap();

    // 2. _rels/.rels
    let rels_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
        <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
        <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>\
        </Relationships>";
    zip.start_file("_rels/.rels", options).unwrap();
    zip.write_all(rels_xml.as_bytes()).unwrap();

    // 3. xl/workbook.xml
    let mut wb_sheets = String::new();
    for i in 1..=num_sheets {
        wb_sheets.push_str(&format!(
            "<sheet name=\"Sheet {}\" sheetId=\"{}\" r:id=\"rId{}\"/>",
            i, i, i
        ));
    }
    let wb_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
        <workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
        <sheets>{}</sheets>\
        </workbook>",
        wb_sheets
    );
    zip.start_file("xl/workbook.xml", options).unwrap();
    zip.write_all(wb_xml.as_bytes()).unwrap();

    // 4. xl/_rels/workbook.xml.rels
    let mut wb_rels_sheets = String::new();
    for i in 1..=num_sheets {
        wb_rels_sheets.push_str(&format!(
            "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{}.xml\"/>",
            i, i
        ));
    }
    let styles_rid = num_sheets + 1;
    let wb_rels_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
        <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
        {}\
        <Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
        </Relationships>",
        wb_rels_sheets, styles_rid
    );
    zip.start_file("xl/_rels/workbook.xml.rels", options).unwrap();
    zip.write_all(wb_rels_xml.as_bytes()).unwrap();

    // 5. xl/styles.xml
    let styles_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
        <styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
        <fonts count=\"1\"><font><sz val=\"11\"/><color theme=\"1\"/><name val=\"Calibri\"/><family val=\"2\"/></font></fonts>\
        <fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills>\
        <borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>\
        <cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
        <cellXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/></cellXfs>\
        </styleSheet>";
    zip.start_file("xl/styles.xml", options).unwrap();
    zip.write_all(styles_xml.as_bytes()).unwrap();

    // 6. xl/worksheets/sheet{i}.xml
    for (s_idx, chunk) in sheet_chunks.iter().enumerate() {
        let sheet_num = s_idx + 1;
        zip.start_file(format!("xl/worksheets/sheet{}.xml", sheet_num), options).unwrap();

        // Header row
        let mut hdr_xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData><row r=\"1\">");
        for (c_idx, name) in headers.iter().enumerate() {
            let col_let = &col_letters[c_idx];
            hdr_xml.push_str(&format!("<c r=\"{}1\" t=\"inlineStr\"><is><t>", col_let));
            escape_xml(name, &mut hdr_xml);
            hdr_xml.push_str("</t></is></c>");
        }
        hdr_xml.push_str("</row>");

        zip.write_all(hdr_xml.as_bytes()).unwrap();

        // Data rows in Rayon parallel
        let body_bytes = render_rows_parallel(chunk, &col_letters, &col_types, 2);
        zip.write_all(&body_bytes).unwrap();

        zip.write_all(b"</sheetData></worksheet>").unwrap();
    }

    zip.finish().unwrap();

    let t_write = t_write_start.elapsed();
    let t_total = t_start.elapsed();
    let total_rows = rows.len();
    let rows_sec = total_rows as f64 / t_total.as_secs_f64();

    println!("\n========================================================");
    println!("[SUMMARY] Total Rows Processed : {}", total_rows);
    println!("[SUMMARY] Output Excel File    : {}", output_path);
    println!("[TIMING] CSV Read & Align Time : {:.4}s", t_read.as_secs_f64());
    println!("[TIMING] Parallel Write Time   : {:.4}s", t_write.as_secs_f64());
    println!("[TOTAL TIME] Completed in      : {:.4}s", t_total.as_secs_f64());
    println!("[THROUGHPUT] Total Speed       : {:.0} rows/sec", rows_sec);
    println!("========================================================\n");
}
