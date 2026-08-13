# High-Performance SQL & CSV to OpenXML Excel (.xlsx) Engine Architecture

## 🎯 Executive Summary

* **Benchmark Dataset:** 1,152,150 Rows × 41 Columns (47,238,150 Cells).
* **Original Python Baseline (`rustpy-xlsxwriter`):** ~36.9 Seconds (~31,000 rows/sec, ~1.8 GB RAM).
* **Native Split CSV Engine (`fast_csv`):** **1.50 Seconds** (**758,960 rows/sec**, **~60 MB RAM**, **70.47 MB `.xlsx`**).
* **Direct Stdin Pipe Engine (`fast_csv_pipe`):** **~1.80 Seconds** (**~550,000 rows/sec**, **ZERO CSV files on disk**).
* **Overall Speedup:** **24.6x Faster Execution**, **30x Reduction in RAM Footprint**.

---

## 🌐 Complete End-to-End System Architecture Pipeline

```
========================================================================================================================
                                    PHASE 1: PYTHON DATABASE STREAM & NORMALIZATION
========================================================================================================================

  [ SQL Server Database / SQLEXPRESS ]
                  │
                  ▼  (ODBC Driver 17 - C-Native Bulk Vector Stream)
       ┌────────────────────────────────────────────────────────┐
       │ arrow_odbc.read_arrow_batches_from_odbc()               │
       │  • Batch Size: 5,000 Rows                              │
       │  • Concurrency: Fetching Active                        │
       │  • Measure Time To First Batch (TTFB)                  │
       └───────────────────────────┬────────────────────────────┘
                                   │
                                   ▼ PyArrow RecordBatch Stream
       ┌────────────────────────────────────────────────────────┐
       │ prepare_stream_for_export() Normalization Layer        │
       │  • Decimal128 / Decimal256 ──► Cast to String (Scale)  │
       │  • DATETIME2 timestamp[ns]  ──► Cast to timestamp[us]  │
       └───────────────────────────┬────────────────────────────┘
                                   │
                                   ▼ Standard Input Pipe IPC (bufsize = 2 MB)
                                   
========================================================================================================================
                                    PHASE 2: RUST MULTITHREADED ENGINE (fast_csv_pipe)
========================================================================================================================

  [ Standard Input (stdin) Pipe Receiver ]
                  │
                  ▼
       ┌────────────────────────────────────────────────────────────────────────────────────────┐
       │ PRODUCER THREAD (BufReader 2MB + StdinLock)                                            │
       │  1. Column Classification: Analyze first 300 records (Numeric f64 vs InlineStr Text)   │
       │  2. Repeated Header Filter: Skip duplicate header rows from concatenated streams      │
       │  3. XML 1.0 Character Sanitization:                                                    │
       │     • Strip illegal ASCII control bytes (0x00..0x1F except \t, \n, \r)                 │
       │     • Escape XML reserved characters (&, <, >, ")                                      │
       │  4. Finite Float Inspection:                                                           │
       │     • Check .is_finite() ──► Write <v>123.45</v>                                       │
       │     • NaN / Infinity      ──► Fallback to inline text <is><t>NaN</t></is>             │
       │  5. XML Chunk Assembly: Pack 25,000 formatted rows into zero-alloc buffer               │
       └───────────────────────────────────────────┬────────────────────────────────────────────┘
                                                   │
                                                   ▼ Bounded Channel (sync_channel 16 Chunks)
                                                   
       ┌────────────────────────────────────────────────────────────────────────────────────────┐
       │ WRITER THREAD (Main Thread: OpenXML Generator & ZIP Streamer)                          │
       │  1. Static Structural Files: Write _rels/.rels and xl/styles.xml                        │
       │  2. Worksheet Streamer: Write xl/worksheets/sheet1.xml                                  │
       │  3. Row Limit Management:                                                              │
       │     • Track current_sheet_rows against 1,048,575 OpenXML row limit                      │
       │     • Auto-close </sheetData></worksheet> and create sheet2.xml on overflow             │
       │  4. Dynamic Metadata Finalization (Post-Stream):                                       │
       │     • Write [Content_Types].xml matching exact active sheets generated                 │
       │     • Write xl/workbook.xml listing only active sheets                                 │
       │     • Write xl/_rels/workbook.xml.rels with sheet relationships                        │
       │  5. ZIP Compression: Stream via Deflate Level 1 (Fastest Compression)                  │
       └───────────────────────────────────────────┬────────────────────────────────────────────┘
                                                   │
                                                   ▼
========================================================================================================================
                                    PHASE 3: OUTPUT ARTIFACT & TELEMETRY
========================================================================================================================

                                  [ final_output_file.xlsx ]
                                              │
                                              ▼
       ┌────────────────────────────────────────────────────────────────────────────────────────┐
       │ TELEMETRY BREAKDOWN LOGGING                                                            │
       │  1. SQL Server TTFB (First Batch Time)                                                 │
       │  2. SQL Fetch & Arrow Ingestion Speed (Rows/sec)                                       │
       │  3. Excel OpenXML Pipe Conversion Speed (Rows/sec)                                     │
       │  4. Step Progress Updates (SQL Fetch vs Rust Pipe time per milestone)                   │
       └────────────────────────────────────────────────────────────────────────────────────────┘
```

---

## ⚡ Key Architectural Modules & Data Flow

### 1. Database Connection & Query Extraction
* **SQL Parsing:** Uses regex matching `(?:FROM|JOIN)\s+\[?([a-zA-Z0-9_]+)\]?\s*\.\s*\[?[a-zA-Z0-9_]+\]?\s*\.\s*\[?[a-zA-Z0-9_]+\]?` to automatically extract the target database name (e.g., `SOLReporting` from `SOLReporting.Suca.LogisticReport`).
* **Connection String:** Connects via C-Native ODBC Driver 17 with 32 KB packet size.

### 2. Standard Input Pipe IPC (`subprocess.Popen`)
* **Zero Disk Writes:** PyArrow writes CSV batches to `proc.stdin` via `subprocess.Popen([binary, "-o", output_file], stdin=subprocess.PIPE, bufsize=2*1024*1024)`.
* **Zero Memory Copy:** Data flows over the operating system pipe buffer without writing intermediate disk files.

### 3. Rust Producer Thread (`BufReader` + Column Inference)
* **Stdin Locking:** Locks `stdin` inside the spawned thread closure (`std::io::stdin().lock()`) to ensure thread-safe ingestion.
* **Auto Type Classification:** Inspects the first 300 rows to differentiate numeric float columns from text columns.
* **Finite Float Inspection:** Evaluates numeric values using `.is_finite()`. Valid floats write to `<c><v>...</v></c>`, while `NaN` / `Infinity` gracefully fall back to inline string tags `<is><t>NaN</t></is>` without corrupting Excel.
* **XML 1.0 Sanitization:** Strips illegal control characters (`0x00..0x1F` except `\t`, `\n`, `\r`) to guarantee 100% Excel compatibility and prevent "Repaired Records" warnings.

### 4. Rust Writer Thread & Dynamic Metadata
* **Compact OpenXML Payload:** Uses `inlineStr` (`<c t="inlineStr">`) and strips unnecessary cell coordinates (`r="A1"`) per ISO/IEC 29500 standards, reducing uncompressed XML size by 68%.
* **Dynamic Worksheet Rotation:** Automatically partitions streams at 1,048,575 rows per sheet (`sheet1.xml`, `sheet2.xml`, ...).
* **Dynamic Metadata Generation:** At stream completion, writes `[Content_Types].xml`, `xl/workbook.xml`, and `xl/_rels/workbook.xml.rels` dynamically based on the exact number of active worksheets created.

### 5. Isolated Telemetry & Timing Breakdown
* Measures and logs:
  - **SQL Server TTFB:** Query execution & first batch delivery latency.
  - **SQL Fetch Time:** Time spent pulling Arrow batches from SQL Server over the network.
  - **Rust Pipe Conversion Time:** Time spent converting Arrow batches into OpenXML and compressing the `.xlsx` file.

---

## 📈 Performance Evolution & Optimization Milestones

| Iteration | Strategy Applied | Total Runtime | Throughput | Output File Size | RAM Usage |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Python Baseline** | `rustpy-xlsxwriter` Constant Memory | 36.9s | 31,000 rows/s | 170 MB | ~1.8 GB |
| **Iteration 1** | Native Rust Single-Threaded CSV Read + Parallel XML Write | 12.6s | 91,000 rows/s | 1.7 GB (Uncompressed) | ~1.8 GB |
| **Iteration 2** | Pipelined Bounded Producer-Consumer Architecture | 5.45s | 211,000 rows/s | 1.7 GB | ~120 MB |
| **Iteration 3** | Zero-Alloc `ByteRecord` + Stack Integer Formatting | 5.22s | 220,000 rows/s | 1.7 GB | ~120 MB |
| **Iteration 4** | Compact ISO OpenXML Payload Stripping | 4.11s | 280,000 rows/s | 70.47 MB | ~120 MB |
| **Iteration 5** | **Multi-Threaded Parallel CSV Engine (`fast_csv`)** | **1.50s** | **758,960 rows/s** | **70.47 MB** | **~60 MB** |
| **Final Pipe** | **Direct Stdin Stream Engine (`fast_csv_pipe`)** | **~1.80s** | **~550,000 rows/s** | **70.47 MB** | **~60 MB** |
