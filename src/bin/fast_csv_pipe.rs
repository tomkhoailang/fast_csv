use std::env;
use std::io::{self, BufReader, Write};
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

enum PipeStreamMessage {
    Header(Vec<String>),
    Chunk { rows_count: usize, xml_data: Vec<u8> },
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

    let output_path = if filtered_args.len() >= 3 && filtered_args[1] == "-o" {
        filtered_args[2].clone()
    } else if filtered_args.len() >= 2 && !filtered_args[1].starts_with('-') {
        filtered_args[1].clone()
    } else {
        "output/piped_output.xlsx".to_string()
    };

    let mode_label = if use_store_mode {
        "ZIP_STORED (Uncompressed Stdin Stream)"
    } else {
        "Deflate Level 1 (Fast Compression Stdin Stream)"
    };

    println!("[START] Dedicated Stdin Pipe Engine [{}]", mode_label);
    println!("[INPUT STREAM] Listening on Stdin pipe...");
    println!("[OUTPUT FILE] Writing XLSX to: {}", output_path);

    let (tx, rx) = sync_channel::<PipeStreamMessage>(16);

    // Producer Thread: Locks Stdin inside thread and parses CSV
    let producer_handle = thread::spawn(move || {
        let stdin = io::stdin();
        let stdin_lock = stdin.lock();
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(true)
            .flexible(true)
            .from_reader(BufReader::with_capacity(2 * 1024 * 1024, stdin_lock));

        let headers: Vec<String> = rdr
            .headers()
            .expect("Failed to read headers from stdin")
            .iter()
            .map(|s| s.to_string())
            .collect();

        let header_bytes: Vec<Vec<u8>> = headers.iter().map(|s| s.as_bytes().to_vec()).collect();
        let num_cols = headers.len();
        tx.send(PipeStreamMessage::Header(headers)).unwrap();

        let mut record = ByteRecord::new();
        let chunk_row_limit = 25_000;
        let mut buf = String::with_capacity(chunk_row_limit * 150);
        let mut row_count = 0;
        let mut sample_records = Vec::new();
        let mut col_types = vec![ColType::Text; num_cols];
        let mut classified = false;

        while rdr.read_byte_record(&mut record).unwrap_or(false) {
            // Skip repeated header rows from concatenated CSV streams
            if record.len() == header_bytes.len() && record.iter().zip(&header_bytes).all(|(b, hb)| b == hb.as_slice()) {
                continue;
            }

            if !classified {
                sample_records.push(record.iter().map(|b| b.to_vec()).collect::<Vec<Vec<u8>>>());
                if sample_records.len() >= 300 {
                    col_types = (0..num_cols)
                        .map(|c_idx| {
                            let mut num_count = 0;
                            let mut non_empty = 0;
                            for r in &sample_records {
                                if c_idx < r.len() {
                                    let val = std::str::from_utf8(&r[c_idx]).unwrap_or("").trim();
                                    if !val.is_empty() {
                                        non_empty += 1;
                                        if val.parse::<f64>().is_ok() {
                                            num_count += 1;
                                        }
                                    }
                                }
                            }
                            if non_empty > 0 && num_count == non_empty {
                                ColType::Numeric
                            } else {
                                ColType::Text
                            }
                        })
                        .collect();
                    classified = true;

                    for sample_rec in sample_records.drain(..) {
                        buf.push_str("<row>");
                        for (c_idx, val_bytes) in sample_rec.iter().enumerate() {
                            if val_bytes.is_empty() {
                                buf.push_str("<c/>");
                                continue;
                            }
                            if col_types[c_idx] == ColType::Numeric {
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
                    }
                }
                continue;
            }

            buf.push_str("<row>");
            for (c_idx, val_bytes) in record.iter().enumerate() {
                if val_bytes.is_empty() {
                    buf.push_str("<c/>");
                    continue;
                }
                if col_types[c_idx] == ColType::Numeric {
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

            if row_count >= chunk_row_limit {
                let xml_data = std::mem::replace(&mut buf, String::with_capacity(chunk_row_limit * 150)).into_bytes();
                tx.send(PipeStreamMessage::Chunk {
                    rows_count: row_count,
                    xml_data,
                })
                .unwrap();
                row_count = 0;
            }
        }

        if !classified && !sample_records.is_empty() {
            col_types = (0..num_cols)
                .map(|c_idx| {
                    let mut num_count = 0;
                    let mut non_empty = 0;
                    for r in &sample_records {
                        if c_idx < r.len() {
                            let val = std::str::from_utf8(&r[c_idx]).unwrap_or("").trim();
                            if !val.is_empty() {
                                non_empty += 1;
                                if val.parse::<f64>().is_ok() {
                                    num_count += 1;
                                }
                            }
                        }
                    }
                    if non_empty > 0 && num_count == non_empty {
                        ColType::Numeric
                    } else {
                        ColType::Text
                    }
                })
                .collect();

            for sample_rec in sample_records {
                buf.push_str("<row>");
                for (c_idx, val_bytes) in sample_rec.iter().enumerate() {
                    if val_bytes.is_empty() {
                        buf.push_str("<c/>");
                        continue;
                    }
                    if col_types[c_idx] == ColType::Numeric {
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
            }
        }

        if !buf.is_empty() {
            tx.send(PipeStreamMessage::Chunk {
                rows_count: row_count,
                xml_data: buf.into_bytes(),
            })
            .unwrap();
        }
    });

    // Main Writer Thread
    let first_msg = rx.recv().expect("Failed to receive header from stdin");
    let headers = match first_msg {
        PipeStreamMessage::Header(h) => h,
        _ => panic!("Expected Header message as first stream message"),
    };

    let col_letters: Vec<String> = (0..headers.len()).map(get_col_letter).collect();
    println!("[HEADER CHECK] Extracted {} columns from Stdin Header", headers.len());

    println!("[WRITE] Pipelined Stream Engine writing to: {}...", output_path);
    let t_write_start = Instant::now();

    let file = std::fs::File::create(&output_path).expect("Failed to create output file");
    let mut zip = zip::ZipWriter::new(file);

    let zip_options = SimpleFileOptions::default()
        .compression_method(compression_method)
        .compression_level(level);

    let rels_xml = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
        <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
        <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>\
        </Relationships>";
    zip.start_file("_rels/.rels", zip_options).unwrap();
    zip.write_all(rels_xml.as_bytes()).unwrap();

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
        if let PipeStreamMessage::Chunk { rows_count, xml_data } = msg {
            total_rows_processed += rows_count;

            if current_sheet_rows + rows_count > max_rows_per_sheet {
                zip.write_all(b"</sheetData></worksheet>").unwrap();
                current_sheet += 1;
                current_sheet_rows = 0;

                zip.start_file(format!("xl/worksheets/sheet{}.xml", current_sheet), zip_options).unwrap();
                zip.write_all(hdr_xml.as_bytes()).unwrap();
            }

            zip.write_all(&xml_data).unwrap();
            current_sheet_rows += rows_count;
        }
    }

    zip.write_all(b"</sheetData></worksheet>").unwrap();

    // Dynamically write [Content_Types].xml, xl/workbook.xml, and xl/_rels/workbook.xml.rels for exact number of created sheets
    let mut ct_xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\n<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\n<Default Extension=\"xml\" ContentType=\"application/xml\"/>\n<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\n<Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\n");
    for s in 1..=current_sheet {
        ct_xml.push_str(&format!("<Override PartName=\"/xl/worksheets/sheet{}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>\n", s));
    }
    ct_xml.push_str("</Types>");
    zip.start_file("[Content_Types].xml", zip_options).unwrap();
    zip.write_all(ct_xml.as_bytes()).unwrap();

    let mut wb_xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\n<sheets>\n");
    for s in 1..=current_sheet {
        wb_xml.push_str(&format!("<sheet name=\"Sheet {}\" sheetId=\"{}\" r:id=\"rId{}\"/>\n", s, s, s));
    }
    wb_xml.push_str("</sheets>\n</workbook>");
    zip.start_file("xl/workbook.xml", zip_options).unwrap();
    zip.write_all(wb_xml.as_bytes()).unwrap();

    let mut wb_rels_xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n");
    for s in 1..=current_sheet {
        wb_rels_xml.push_str(&format!("<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{}.xml\"/>\n", s, s));
    }
    wb_rels_xml.push_str(&format!("<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\n", current_sheet + 1));
    wb_rels_xml.push_str("</Relationships>");
    zip.start_file("xl/_rels/workbook.xml.rels", zip_options).unwrap();
    zip.write_all(wb_rels_xml.as_bytes()).unwrap();

    zip.finish().unwrap();
    producer_handle.join().unwrap();

    let t_write = t_write_start.elapsed();
    let t_global = t_global_start.elapsed();

    let file_size_mb = std::fs::metadata(&output_path).map(|m| m.len() as f64 / (1024.0 * 1024.0)).unwrap_or(0.0);
    let rows_sec = total_rows_processed as f64 / t_global.as_secs_f64();

    println!("\n========================================================");
    println!("[SUMMARY] Total Rows Processed : {}", total_rows_processed);
    println!("[SUMMARY] Output Excel File    : {} ({:.2} MB)", output_path, file_size_mb);
    println!("[SUMMARY] Worksheets Generated : {} sheet(s)", current_sheet);
    println!("[TIMING] Pipelined Stream Write: {:.4}s", t_write.as_secs_f64());
    println!("[TOTAL TIME] Completed in      : {:.4}s", t_global.as_secs_f64());
    println!("[THROUGHPUT] Total Speed       : {:.0} rows/sec", rows_sec);
    println!("========================================================\n");
}
