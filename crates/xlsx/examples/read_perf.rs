//! Reads a large generated workbook and times it (release build):
//! `cargo run --release -p gridcraft-xlsx --example read_perf -- [rows] [cols]`
//!
//! The default is a 100,000 × 10 sheet (one million cells): one text column (shared strings)
//! plus numeric columns.

#![allow(clippy::disallowed_methods)] // a native-only benchmark: wasm never runs it

use std::sync::Arc;
use std::time::Instant;

use gridcraft_core::{CellRef, Value};
use gridcraft_model::{Cell, Sheet, Workbook};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let rows: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let cols: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);

    let mut sheet = Sheet::new("Data");
    for r in 0..rows {
        for c in 0..cols {
            let value = if c == 0 { Value::from(format!("Item {r}")) } else { Value::Number((r as f64) * 100.0 + c as f64 + 0.5) };
            sheet.cells.set(CellRef::new(r, c), Cell::value(value));
        }
    }
    let mut wb = Workbook::new();
    wb.sheets.clear();
    wb.sheets.push(Arc::new(sheet));

    let t = Instant::now();
    let bytes = gridcraft_xlsx::write_xlsx(&wb)?;
    println!("build+write: {:?} ({} bytes, {} cells)", t.elapsed(), bytes.len(), u64::from(rows) * u64::from(cols));

    // Warm-up read (shared strings caches, page cache), then the timed one.
    let (warm, _) = gridcraft_xlsx::read_xlsx(&bytes)?;
    drop(warm);
    let t = Instant::now();
    let (back, report) = gridcraft_xlsx::read_xlsx(&bytes)?;
    let elapsed = t.elapsed();
    let cells = back.sheets.first().map(|s| s.cells.len()).unwrap_or(0);
    println!("read_xlsx: {elapsed:?} ({cells} cells, {} warnings)", report.warnings.len());

    // Spot-check the corners so a "fast but wrong" reader can't pass.
    let s = &back.sheets[0];
    let first_ok = matches!(s.cells.get(CellRef::new(0, 0)).map(|c| &c.value), Some(Value::Text(t)) if t.as_ref() == "Item 0");
    let want = (rows as f64 - 1.0) * 100.0 + cols as f64 - 1.0 + 0.5;
    let last_ok = matches!(s.cells.get(CellRef::new(rows - 1, cols - 1)).map(|c| &c.value), Some(Value::Number(n)) if *n == want);
    println!("A1 ok: {first_ok}, last ok: {last_ok}");
    if cells != (rows as usize) * (cols as usize) || !first_ok || !last_ok {
        return Err("cell content mismatch".into());
    }
    Ok(())
}
