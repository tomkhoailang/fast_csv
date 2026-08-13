use std::collections::HashSet;
use std::env;
use std::io::Write;
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::Instant;

use csv::ByteRecord;
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
fn escape_xml_bytes(s: &[u8], out: &mut String) {
    let s_str = std::str::from_utf8(s).unwrap_or("");
    for c in s_str.chars() {
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
    let sample_limit = 300;
    let mut samples: Vec<Vec<Vec<u8>>> = Vec::new();

    for path in csv_paths {
        if let Ok(mut rdr) = csv::ReaderBuilder::new().has_headers(true).from_path(path) {
            let mut record = ByteRecord::new();
            let mut count = 0;
            while rdr.read_byte_record(&mut record).unwrap_or(false) && count < sample_limit {
                samples.push(record.iter().map(|b| b.to_vec()).collect());
                count += 1;
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
                    let val_bytes = &row[c_idx];
                    let s = std::str::from_utf8(val_bytes).unwrap_or("").trim();
                    if !s.is_empty() {
                        total_non_empty += 1;
                        if s.parse::<f64>().is_ok() {
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

struct FileXmlChunkMessage {
    rows_count: usize,
    xml_data: Vec<u8>,
}

fn main() {
    let t_global_start = Instant::now();
    let args: Vec<String> = env::args().collect();

    let use_store_mode = args.iter().any(|a| a == "--store");
    let (compression_method, level) = if use_store_mode {
        (CompressionMethod::Stored, None)
    } else {
        (CompressionMethod::Deflated, Some(1))
    };

    let filtered_args: Vec<String> = args.into_iter().filter(|a| a != "--store").collect();

    let (csv_files, output_path) = if filtered_args.len() >= 3 && filtered_args[1] == "-o" {
        (filtered_args[3..].to_vec(), filtered_args[2].clone())
    } else if filtered_args.len() >= 2 {
        (filtered_args[1..].to_vec(), "output_fast.xlsx".to_string())
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

    let mode_label = if use_store_mode {
        "ZIP_STORED (Uncompressed)"
    } else {
        "Deflate Level 1 (Fast Compression)"
    };

    println!("[START] Sequential Order Pipelined CSV-to-XML Engine [{}]", mode_label);

    // 1. Header Alignment
    let t_hdr_start = Instant::now();
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
    let t_hdr = t_hdr_start.elapsed();

    let (tx, rx) = sync_channel::<FileXmlChunkMessage>(16);

    let csv_files_clone = csv_files.clone();
    let headers_clone = headers.clone();

    // Spawns sequential producer thread preserving strict file order
    let producer_handle = thread::spawn(move || {
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

            let mut record = ByteRecord::new();
            let chunk_row_limit = 25_000;
            let mut buf = String::with_capacity(chunk_row_limit * 150);
            let mut row_count = 0;

            while rdr.read_byte_record(&mut record).unwrap_or(false) {
                buf.push_str("<row>");

                let mut aligned_bytes = vec![b"".as_slice(); headers_clone.len()];
                for (orig_idx, val_bytes) in record.iter().enumerate() {
                    if orig_idx < header_map.len() {
                        let target_idx = header_map[orig_idx];
                        aligned_bytes[target_idx] = val_bytes;
                    }
                }

                for (target_idx, val_bytes) in aligned_bytes.into_iter().enumerate() {
                    if val_bytes.is_empty() {
                        buf.push_str("<c/>");
                        continue;
                    }

                    if col_types[target_idx] == ColType::Numeric {
                        buf.push_str("<c><v>");
                        buf.push_str(std::str::from_utf8(val_bytes).unwrap_or(""));
                        buf.push_str("</v></c>");
                    } else {
                        buf.push_str("<c t=\"inlineStr\"><is><t>");
                        escape_xml_bytes(val_bytes, &mut buf);
                        buf.push_str("</t></is></c>");
                    }
                }

                buf.push_str("</row>");
                row_count += 1;

                if row_count == chunk_row_limit {
                    let xml_data = std::mem::replace(&mut buf, String::with_capacity(chunk_row_limit * 150)).into_bytes();
                    tx.send(FileXmlChunkMessage {
                        rows_count: row_count,
                        xml_data,
                    })
                    .unwrap();
                    row_count = 0;
                }
            }

            if !buf.is_empty() {
                tx.send(FileXmlChunkMessage {
                    rows_count: row_count,
                    xml_data: buf.into_bytes(),
                })
                .unwrap();
            }
        }
    });

    println!("[WRITE] Pipelined Stream Engine writing to: {}...", output_path);
    let t_write_start = Instant::now();

    let file = std::fs::File::create(&output_path).expect("Failed to create output file");
    let mut zip = zip::ZipWriter::new(file);

    let zip_options = SimpleFileOptions::default()
        .compression_method(compression_method)
        .compression_level(level);

    // Static Metadata Files
    let ct_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
        <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
        <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
        <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
        <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
        <Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\
        <Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>\
        <Override PartName=\"/xl/worksheets/sheet2.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>\
        </Types>";
    zip.start_file("[Content_Types].xml", zip_options).unwrap();
    zip.write_all(ct_xml.as_bytes()).unwrap();

    let rels_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
        <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
        <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>\
        </Relationships>";
    zip.start_file("_rels/.rels", zip_options).unwrap();
    zip.write_all(rels_xml.as_bytes()).unwrap();

    let wb_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
        <workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
        <sheets>\
        <sheet name=\"Sheet 1\" sheetId=\"1\" r:id=\"rId1\"/>\
        <sheet name=\"Sheet 2\" sheetId=\"2\" r:id=\"rId2\"/>\
        </sheets>\
        </workbook>";
    zip.start_file("xl/workbook.xml", zip_options).unwrap();
    zip.write_all(wb_xml.as_bytes()).unwrap();

    let wb_rels_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
        <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
        <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/>\
        <Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet2.xml\"/>\
        <Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
        </Relationships>";
    zip.start_file("xl/_rels/workbook.xml.rels", zip_options).unwrap();
    zip.write_all(wb_rels_xml.as_bytes()).unwrap();

    let styles_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
        <styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
        <fonts count=\"1\"><font><sz val=\"11\"/><color theme=\"1\"/><name val=\"Calibri\"/><family val=\"2\"/></font></fonts>\
        <fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills>\
        <borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>\
        <cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
        <cellXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/></cellXfs>\
        </styleSheet>";
    zip.start_file("xl/styles.xml", zip_options).unwrap();
    zip.write_all(styles_xml.as_bytes()).unwrap();

    zip.start_file("xl/worksheets/sheet1.xml", zip_options).unwrap();
    let mut hdr_xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData><row>");
    for (c_idx, name) in headers.iter().enumerate() {
        let col_let = &col_letters[c_idx];
        hdr_xml.push_str(&format!("<c r=\"{}1\" t=\"inlineStr\"><is><t>", col_let));
        escape_xml_bytes(name.as_bytes(), &mut hdr_xml);
        hdr_xml.push_str("</t></is></c>");
    }
    hdr_xml.push_str("</row>");
    zip.write_all(hdr_xml.as_bytes()).unwrap();

    let max_rows_per_sheet = 1_048_575;
    let mut current_sheet = 1;
    let mut current_sheet_rows = 0;
    let mut total_rows_processed = 0;

    while let Ok(msg) = rx.recv() {
        total_rows_processed += msg.rows_count;

        if current_sheet_rows + msg.rows_count > max_rows_per_sheet {
            zip.write_all(b"</sheetData></worksheet>").unwrap();
            current_sheet += 1;
            current_sheet_rows = 0;

            zip.start_file(format!("xl/worksheets/sheet{}.xml", current_sheet), zip_options).unwrap();
            zip.write_all(hdr_xml.as_bytes()).unwrap();
        }

        zip.write_all(&msg.xml_data).unwrap();
        current_sheet_rows += msg.rows_count;
    }

    zip.write_all(b"</sheetData></worksheet>").unwrap();
    zip.finish().unwrap();
    producer_handle.join().unwrap();

    let t_write = t_write_start.elapsed();
    let t_global = t_global_start.elapsed();

    let file_size_mb = std::fs::metadata(&output_path).map(|m| m.len() as f64 / (1024.0 * 1024.0)).unwrap_or(0.0);
    let rows_sec = total_rows_processed as f64 / t_global.as_secs_f64();

    println!("\n========================================================");
    println!("[SUMMARY] Total Rows Processed : {}", total_rows_processed);
    println!("[SUMMARY] Output Excel File    : {} ({:.2} MB)", output_path, file_size_mb);
    println!("[TIMING] Header & Classify     : {:.4}s", t_hdr.as_secs_f64());
    println!("[TIMING] Pipelined Stream Write: {:.4}s", t_write.as_secs_f64());
    println!("[TOTAL TIME] Completed in      : {:.4}s", t_global.as_secs_f64());
    println!("[THROUGHPUT] Total Speed       : {:.0} rows/sec", rows_sec);
    println!("========================================================\n");
}
