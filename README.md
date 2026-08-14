# 🚀 `fast_csv` — Ultra-Fast CSV & Direct Stdin Stream to XLSX Converter Engine

![Language: Rust](https://img.shields.io/badge/Language-Rust-orange.svg)
![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)
![Performance: 750k rows/s](https://img.shields.io/badge/Performance-758%2C960%20rows%2Fsec-brightgreen.svg)

`fast_csv` is a high-performance C-native / Rust OpenXML engine designed for zero-copy, memory-capped conversion of multi-gigabyte database streams and split CSV files into single or multi-sheet Excel (`.xlsx`) files.

---

## 🌟 Key Features

* **⚡ Blazing Fast Throughput:** Converts **758,960 rows/sec** (1.15M rows × 41 columns in **1.50s**).
* **🌊 Direct Stdin Streaming (`fast_csv_pipe`):** Streams PyArrow/ODBC database queries directly to `.xlsx` via standard input pipe IPC with **ZERO intermediate CSV files on disk**.
* **🧠 Fixed Memory Footprint (~60 MB RAM):** Uses bounded multithreaded channels (`sync_channel(16)`) and fixed 2MB stream buffers. RAM usage is strictly capped regardless of dataset size (1M or 100M rows).
* **📑 Dynamic Multi-Sheet Overflow:** Automatically partitions streams at 1,048,575 rows per sheet (`Sheet 1`, `Sheet 2`, ...) and dynamically generates OpenXML workbook metadata matching active sheets.
* **🛡️ 100% Excel Compatibility:** Filters illegal XML 1.0 control characters (`0x00..0x1F`) and evaluates `.is_finite()` on floats to prevent `<v>NaN</v>` or `<v>Infinity</v>` Excel corruption warnings.
* **🎯 Direct PyArrow Schema Support (`--types`):** Pass exact PyArrow column data types directly (`--types N,T,N,T`) to bypass sample row inspection and stream instantly from Row 1.
* **📊 Telemetry Breakdown:** Real-time progress logging isolating SQL Server TTFB, SQL Arrow fetch speed, and Rust OpenXML pipe conversion speed per step.

---

## 🛠️ Installation & Building

### Prerequisites
* Rust toolchain (`cargo`, `rustc` 1.70+)
* Optional: `mingw64-gcc` for cross-compiling Windows `.exe` targets on Linux.

### Build Executables

```bash
# Build native release binaries
cargo build --release --bin fast_csv_pipe
cargo build --release --bin fast_csv

# Cross-compile Windows .exe target (from Linux)
cargo build --target x86_64-pc-windows-gnu --release --bin fast_csv_pipe
cargo build --target x86_64-pc-windows-gnu --release --bin fast_csv
```

---

## 🚀 Usage

### 1. Direct Stdin Pipe Streaming (`fast_csv_pipe`)

Stream CSV byte streams directly from Python or shell scripts:

```bash
cat data.csv | ./target/release/fast_csv_pipe -o output.xlsx
```

With explicit schema column types (`N` = Numeric, `T` = Text):

```bash
cat data.csv | ./target/release/fast_csv_pipe -o output.xlsx --types N,T,N,T
```

### 2. Multi-Part Split CSV Converter (`fast_csv`)

Merge multiple split CSV files sequentially into a single `.xlsx` file:

```bash
./target/release/fast_csv -o merged_output.xlsx part1.csv part2.csv part3.csv
```

---

## 🐍 Python Integration (`simple_run_query.py`)

```python
from simple_run_query import run_query

# Streams SQL Server query directly into fast_csv_pipe -> output.xlsx
output_file = run_query(
    query="SELECT TOP 1000000 * FROM [FOX].[dbo].[safuture]",
    db_server=r"localhost\SQLEXPRESS",
    excel_name="safuture_report",
)
```

## 🧠 How Backpressure & Capped RAM Works

When streaming database queries from Python to `fast_csv_pipe` via standard input (`stdin`), memory usage is capped at **~60 MB RAM** regardless of the dataset size (whether 1M or 100M+ rows). This is achieved via automatic, OS-level cascading backpressure:

1. **Rust Channel Bounding:** If the single-threaded ZIP writer is slower than the CSV parsing thread, the `sync_channel(16)` fills up.
2. **Rust Reader Pause:** Once the channel is full, the Rust parser pauses at `.send()`, which in turn halts the Rust reader thread from pulling new bytes from `stdin`.
3. **OS Pipe Blocking:** Because the Rust process is no longer reading from the standard input pipe, the operating system's internal pipe buffer fills up.
4. **Python Pause:** Once the OS pipe buffer is full, Python's underlying OS `write()` system call blocks. The Python script is automatically paused inside `writer.write_batch(batch)` and does not fetch new rows from SQL Server until the Rust writer catches up, frees space in the channel, and resumes reading from the pipe.

This cascade of blocking ensures optimal CPU utilization and zero memory spikes without needing any custom synchronization logic.

---

## 📊 Benchmark Comparison

| Engine / Strategy | Execution Time | Processing Speed | Memory Usage | Output Size |
| :--- | :--- | :--- | :--- | :--- |
| **Python Baseline (`rustpy-xlsxwriter`)** | 36.9s | 31,000 rows/s | ~1.8 GB RAM | 170 MB |
| **Native Multi-CSV Engine (`fast_csv`)** | **1.50s** | **758,960 rows/s** | **~60 MB RAM** | **70.47 MB** |
| **Direct Stdin Stream Engine (`fast_csv_pipe`)** | **~1.80s** | **~550,000 rows/s** | **~60 MB RAM** | **70.47 MB** |

---

## 📄 Documentation

For full architectural diagrams, memory cap mathematical proofs, and pipeline design details, see [`ARCHITECTURE.md`](ARCHITECTURE.md).
