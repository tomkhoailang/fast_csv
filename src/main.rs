use std::collections::HashSet;
use std::env;
use std::io::Write;
use std::sync::mpsc::sync_channel;
use std::thread;
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

#[derive(Copy, Clone, PartialEq, Debug)]
enum ColType {
    Numeric,
    Text,
}

fn classify_columns_from_sample(csv_paths: &[String], headers: &[String]) -> Vec<ColType> {
    let sample_limit = 200;
    let mut samples: Vec<Vec<String>> = Vec::new();

    for path in csv_paths {
        if let Ok(mut rdr) = csv::ReaderBuilder::new().has_headers(true).from_path(path) {
            for result in rdr.records().take(sample_limit) {
                if let Ok(rec) = result {
                    samples.push(rec.iter().map(|s| s.to_string()).collect());
                }
            }
        }
    }

    headers
        .iter()
        .enumerate()
        .map(|(c_idx, _)| {
            let mut numeric_count = 0;
            let mut total_non_empty = 0;
            for row in &samples {
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

fn render_chunk_parallel(
    records: &[Vec<String>],
    col_letters: &[String],
    col_types: &[ColType],
    start_row_1based: usize,
) -> Vec<u8> {
    const SUB_CHUNK: usize = 10_000;

    let sub_results: Vec<String> = records
        .par_chunks(SUB_CHUNK)
        .enumerate()
        .map(|(sub_idx, sub)| {
            let mut buf = String::with_capacity(sub.len() * 250);
            let sub_start_row = start_row_1based + sub_idx * SUB_CHUNK;

            for (r_idx, row) in sub.iter().enumerate() {
                let row_num = sub_start_row + r_idx;
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

    sub_results.concat().into_bytes()
}

struct RowChunkMessage {
    sheet_idx: usize,
    chunk_start_row: usize,
    records: Vec<Vec<String>>,
    is_last_chunk_of_sheet: bool,
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
            "output/merged_report.xlsx".to_string(),
        )
    };

    println!("[START] Pipelined Stream Engine (Zero RAM Bottleneck)");
    let t_start = Instant::now();

    // 1. First Pass: Collect Aligned Headers Across Files
    let mut headers = Vec::new();
    let mut header_set = HashSet::new();
    let mut file_headers_list = Vec::new();

    for path in &csv_files {
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .from_path(path)
            .unwrap_or_else(|e| panic!("Failed to open CSV file {}: {}", path, e));

        let hdr: Vec<String> = rdr
            .headers()
            .expect("Failed to read headers")
            .iter()
            .map(|s| s.to_string())
            .collect();

        for h in &hdr {
            if !header_set.contains(h) {
                header_set.insert(h.clone());
                headers.push(h.clone());
            }
        }
        file_headers_list.push(hdr);
    }

    let col_types = classify_columns_from_sample(&csv_files, &headers);
    let col_letters: Vec<String> = (0..headers.len()).map(get_col_letter).collect();

    // Bounded sync channel (max 4 buffered chunks in memory = ~160k rows max RAM footprint)
    let (tx, rx) = sync_channel::<RowChunkMessage>(4);

    let csv_files_clone = csv_files.clone();
    let headers_clone = headers.clone();

    // Producer Thread: Streams CSV rows concurrently into bounded channel
    thread::spawn(move || {
        let max_rows_per_sheet = 1_048_575; // Excel sheet row limit
        let chunk_size = 40_000;

        let mut current_sheet_idx = 1;
        let mut current_sheet_rows = 0;
        let mut current_chunk = Vec::with_capacity(chunk_size);
        let mut chunk_start_row = 2; // Row 1 is header

        for (f_idx, path) in csv_files_clone.iter().enumerate() {
            let mut rdr = csv::ReaderBuilder::new()
                .has_headers(true)
                .flexible(true)
                .from_path(path)
                .unwrap_or_else(|e| panic!("Failed to open CSV file {}: {}", path, e));

            let file_headers = &file_headers_list[f_idx];
            let header_map: Vec<usize> = file_headers
                .iter()
                .map(|fh| headers_clone.iter().position(|h| h == fh).unwrap())
                .collect();

            for result in rdr.records() {
                let record = result.expect("Failed to read CSV record");
                let mut aligned_row = vec![String::new(); headers_clone.len()];
                for (idx, val) in record.iter().enumerate() {
                    if idx < header_map.len() {
                        let target_idx = header_map[idx];
                        aligned_row[target_idx] = val.to_string();
                    }
                }

                current_chunk.push(aligned_row);
                current_sheet_rows += 1;

                if current_chunk.len() == chunk_size || current_sheet_rows == max_rows_per_sheet {
                    let is_sheet_end = current_sheet_rows == max_rows_per_sheet;
                    let msg = RowChunkMessage {
                        sheet_idx: current_sheet_idx,
                        chunk_start_row,
                        records: current_chunk,
                        is_last_chunk_of_sheet: is_sheet_end,
                    };
                    chunk_start_row += msg.records.len();
                    tx.send(msg).unwrap();

                    current_chunk = Vec::with_capacity(chunk_size);

                    if is_sheet_end {
                        current_sheet_idx += 1;
                        current_sheet_rows = 0;
                        chunk_start_row = 2;
                    }
                }
            }
        }

        if !current_chunk.is_empty() {
            let msg = RowChunkMessage {
                sheet_idx: current_sheet_idx,
                chunk_start_row,
                records: current_chunk,
                is_last_chunk_of_sheet: true,
            };
            tx.send(msg).unwrap();
        }
    });

    // Consumer (Main Thread): Receives chunks and streams XML directly to ZIP
    println!("[WRITE] Streaming Pipelined XLSX to: {}...", output_path);
    let file = std::fs::File::create(&output_path).expect("Failed to create output file");
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    let mut sheets_created = HashSet::new();
    let mut total_rows_processed = 0;

    while let Ok(msg) = rx.recv() {
        let sheet_num = msg.sheet_idx;
        total_rows_processed += msg.records.len();

        if !sheets_created.contains(&sheet_num) {
            sheets_created.insert(sheet_num);

            // Write minimal workbook metadata on first sheet creation
            if sheet_num == 1 {
                let ct_xml = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
                    <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
                    <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
                    <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
                    <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
                    <Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\
                    <Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>\
                    <Override PartName=\"/xl/worksheets/sheet2.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>\
                    </Types>"
                );
                zip.start_file("[Content_Types].xml", options).unwrap();
                zip.write_all(ct_xml.as_bytes()).unwrap();

                let rels_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
                    <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
                    <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>\
                    </Relationships>";
                zip.start_file("_rels/.rels", options).unwrap();
                zip.write_all(rels_xml.as_bytes()).unwrap();

                let wb_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
                    <workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
                    <sheets>\
                    <sheet name=\"Sheet 1\" sheetId=\"1\" r:id=\"rId1\"/>\
                    <sheet name=\"Sheet 2\" sheetId=\"2\" r:id=\"rId2\"/>\
                    </sheets>\
                    </workbook>";
                zip.start_file("xl/workbook.xml", options).unwrap();
                zip.write_all(wb_xml.as_bytes()).unwrap();

                let wb_rels_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
                    <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
                    <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/>\
                    <Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet2.xml\"/>\
                    <Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
                    </Relationships>";
                zip.start_file("xl/_rels/workbook.xml.rels", options).unwrap();
                zip.write_all(wb_rels_xml.as_bytes()).unwrap();

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
            }

            // Start new worksheet file
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
        }

        // Parallel Rayon render for the popped chunk
        let xml_bytes = render_chunk_parallel(&msg.records, &col_letters, &col_types, msg.chunk_start_row);
        zip.write_all(&xml_bytes).unwrap();

        if msg.is_last_chunk_of_sheet {
            zip.write_all(b"</sheetData></worksheet>").unwrap();
        }
    }

    zip.finish().unwrap();

    let t_total = t_start.elapsed();
    let rows_sec = total_rows_processed as f64 / t_total.as_secs_f64();

    println!("\n========================================================");
    println!("[SUMMARY] Total Rows Processed : {}", total_rows_processed);
    println!("[SUMMARY] Output Excel File    : {}", output_path);
    println!("[STREAMING] Overlapped Read & Write Pipeline Complete");
    println!("[TOTAL TIME] Completed in      : {:.4}s", t_total.as_secs_f64());
    println!("[THROUGHPUT] Total Speed       : {:.0} rows/sec", rows_sec);
    println!("========================================================\n");
}
