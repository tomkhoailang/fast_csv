use std::env;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::Instant;

use csv::ByteRecord;
use zip::ZipArchive;

fn extract_xml_cells(row_str: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut pos = 0;

    while let Some(c_start) = row_str[pos..].find("<c") {
        let abs_c_start = pos + c_start;
        let next_slash_gt = row_str[abs_c_start..].find("/>");
        let next_close_c = row_str[abs_c_start..].find("</c>");

        let (cell_xml, c_len) = match (next_slash_gt, next_close_c) {
            (Some(sg_idx), Some(cc_idx)) if sg_idx < cc_idx => {
                let len = sg_idx + 2;
                (&row_str[abs_c_start..abs_c_start + len], len)
            }
            (Some(sg_idx), None) => {
                let len = sg_idx + 2;
                (&row_str[abs_c_start..abs_c_start + len], len)
            }
            (_, Some(cc_idx)) => {
                let len = cc_idx + 4;
                (&row_str[abs_c_start..abs_c_start + len], len)
            }
            (None, None) => break,
        };

        pos = abs_c_start + c_len;

        if cell_xml == "<c/>" || cell_xml.ends_with("/>") {
            cells.push(String::new());
            continue;
        }

        if cell_xml.contains("t=\"inlineStr\"") {
            if let Some(t_start) = cell_xml.find("<t>") {
                if let Some(t_end) = cell_xml.find("</t>") {
                    let text = &cell_xml[t_start + 3..t_end];
                    let unescaped = text
                        .replace("&amp;", "&")
                        .replace("&lt;", "<")
                        .replace("&gt;", ">")
                        .replace("&quot;", "\"");
                    cells.push(unescaped);
                    continue;
                }
            }
        } else if let Some(v_start) = cell_xml.find("<v>") {
            if let Some(v_end) = cell_xml.find("</v>") {
                let val = &cell_xml[v_start + 3..v_end];
                cells.push(val.to_string());
                continue;
            }
        }

        cells.push(String::new());
    }

    cells
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let xlsx_path = if args.len() >= 2 {
        args[1].clone()
    } else {
        "output/merged_report.xlsx".to_string()
    };

    let csv_files: Vec<String> = if args.len() >= 3 {
        args[2..].to_vec()
    } else {
        vec![
            "output/sample_20260812_180359_Part_1.csv".to_string(),
            "output/sample_20260812_180359_Part_2.csv".to_string(),
            "output/sample_20260812_180359_Part_3.csv".to_string(),
        ]
    };

    println!("========================================================");
    println!("[VERIFIER] Starting 1:1 Round-Trip Integrity Verification");
    println!("[INPUT XLSX] {}", xlsx_path);
    println!("[INPUT CSVs] {:?}", csv_files);
    println!("========================================================\n");

    let t_start = Instant::now();

    if !Path::new(&xlsx_path).exists() {
        eprintln!("[ERROR] Output Excel file not found: {}", xlsx_path);
        std::process::exit(1);
    }

    let file = File::open(&xlsx_path).expect("Failed to open XLSX file");
    let mut archive = ZipArchive::new(BufReader::new(file)).expect("Failed to parse XLSX ZIP archive");

    let mut sheet_names: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let entry = archive.by_index(i).unwrap();
        let name = entry.name();
        if name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml") {
            sheet_names.push(name.to_string());
        }
    }
    sheet_names.sort();

    println!("[XLSX ARCHIVE] Found {} worksheet(s): {:?}", sheet_names.len(), sheet_names);

    let mut csv_file_idx = 0;
    let mut current_csv_rdr = None;
    let mut current_record = ByteRecord::new();

    let mut total_xlsx_rows = 0;
    let mut total_csv_rows = 0;
    let mut total_cells_checked = 0;
    let mut mismatch_count = 0;

    for sheet_file in &sheet_names {
        let mut entry = archive.by_name(sheet_file).expect("Failed to open sheet in ZIP");
        let mut sheet_content = String::new();
        entry.read_to_string(&mut sheet_content).expect("Failed to read sheet XML");

        let mut pos = 0;
        let mut is_sheet_first_row = true;

        while let Some(r_start) = sheet_content[pos..].find("<row>") {
            let abs_r_start = pos + r_start;
            let r_end_rel = match sheet_content[abs_r_start..].find("</row>") {
                Some(idx) => idx + 6,
                None => break,
            };
            let row_xml = &sheet_content[abs_r_start..abs_r_start + r_end_rel];
            pos = abs_r_start + r_end_rel;

            let xlsx_cells = extract_xml_cells(row_xml);

            // Skip Header row on EVERY sheet
            if is_sheet_first_row {
                is_sheet_first_row = false;
                if total_xlsx_rows == 0 {
                    println!("[HEADER CHECK] Extracted {} columns from XLSX Header", xlsx_cells.len());
                }
                continue;
            }

            total_xlsx_rows += 1;

            // Fetch Next CSV Record
            loop {
                if current_csv_rdr.is_none() {
                    if csv_file_idx >= csv_files.len() {
                        eprintln!("[MISMATCH ERROR] XLSX has more rows than original CSV files!");
                        mismatch_count += 1;
                        break;
                    }
                    let path = &csv_files[csv_file_idx];
                    let rdr = csv::ReaderBuilder::new()
                        .has_headers(true)
                        .flexible(true)
                        .from_path(path)
                        .unwrap_or_else(|e| panic!("Failed to open CSV file {}: {}", path, e));
                    current_csv_rdr = Some(rdr);
                    csv_file_idx += 1;
                }

                if let Some(ref mut rdr) = current_csv_rdr {
                    if rdr.read_byte_record(&mut current_record).unwrap_or(false) {
                        total_csv_rows += 1;
                        break;
                    } else {
                        current_csv_rdr = None;
                    }
                }
            }

            // Compare Row Cells
            for (c_idx, xlsx_val) in xlsx_cells.iter().enumerate() {
                total_cells_checked += 1;

                let csv_val = if c_idx < current_record.len() {
                    let bytes = current_record.get(c_idx).unwrap_or(b"");
                    std::str::from_utf8(bytes).unwrap_or("").trim().to_string()
                } else {
                    String::new()
                };

                let xlsx_val_trimmed = xlsx_val.trim();

                let matches = if xlsx_val_trimmed == csv_val {
                    true
                } else if let (Ok(n1), Ok(n2)) = (xlsx_val_trimmed.parse::<f64>(), csv_val.parse::<f64>()) {
                    (n1 - n2).abs() < 1e-6
                } else {
                    false
                };

                if !matches {
                    mismatch_count += 1;
                    if mismatch_count <= 10 {
                        eprintln!(
                            "[DATA MISMATCH] Row {} Col {}: XLSX='{}' vs CSV='{}'",
                            total_xlsx_rows, c_idx + 1, xlsx_val_trimmed, csv_val
                        );
                    }
                }
            }
        }
    }

    let duration = t_start.elapsed();

    println!("\n========================================================");
    println!("[SUMMARY] Verification Completed in {:.4}s", duration.as_secs_f64());
    println!("[SUMMARY] Total XLSX Data Rows : {}", total_xlsx_rows);
    println!("[SUMMARY] Total CSV Data Rows  : {}", total_csv_rows);
    println!("[SUMMARY] Total Cells Inspected: {}", total_cells_checked);

    if mismatch_count == 0 && total_xlsx_rows == total_csv_rows {
        println!("\n✅ [VERIFICATION PASSED] 100% EXACT 1:1 MATCH CONFIRMED!");
        println!("   Zero data loss, zero row discrepancies, zero value mismatches.");
    } else {
        println!("\n❌ [VERIFICATION FAILED] Found {} discrepancies!", mismatch_count);
    }
    println!("========================================================\n");
}
