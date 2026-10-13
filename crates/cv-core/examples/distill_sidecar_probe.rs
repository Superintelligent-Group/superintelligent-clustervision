//! Isolated extra sidecar heap control. The existing elision roster is allocated before the
//! counting baseline; compare legacy whole-file construction with shared JSONL staging. No timings.
use anyhow::{Context, Result};
use cv_core::distill::Elided;
use serde::Serialize;
use serde_json::Value;
use std::io::Read;
#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
#[derive(Serialize)]
struct OwnedEntry {
    id: String,
    slot: String,
    name: String,
    input: Value,
    content: Value,
    size: usize,
    line_count: usize,
    kind: String,
}
fn main() -> Result<()> {
    std::env::var_os("CLUSTERVISION_HOME").context("use scratch CLUSTERVISION_HOME")?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mib: usize = args[1].parse()?;
    let pattern = "a\"\\\nλ";
    let elided = vec![Elided {
        tool_use_id: "call".into(),
        tool_name: "Read".into(),
        input: serde_json::json!({"path":"/tmp/input","metadata":"x".repeat(8192)}),
        content: pattern.repeat(mib * 1024 * 1024 / pattern.len()),
    }];
    let path = std::path::Path::new(&args[2]);
    #[cfg(feature = "probe-heap")]
    let baseline = cv_testkit::heap::begin();
    match args[0].as_str() {
        "legacy" => {
            let mut body = String::new();
            for entry in &elided {
                let owned = OwnedEntry {
                    id: entry.tool_use_id.clone(),
                    slot: "content".into(),
                    name: entry.tool_name.clone(),
                    input: entry.input.clone(),
                    content: Value::String(entry.content.clone()),
                    size: entry.content.len(),
                    line_count: entry.content.lines().count(),
                    kind: "text".into(),
                };
                let line = serde_json::to_string(&owned)?;
                drop(owned);
                body.push_str(&line);
                body.push('\n');
            }
            std::fs::write(path, body)?;
        }
        "staged" => cv_core::distill::write_sidecar(path, &elided)?,
        _ => anyhow::bail!("distill_sidecar_probe legacy|staged MIB SCRATCH_OUTPUT"),
    }
    #[cfg(feature = "probe-heap")]
    let heap = Some(cv_testkit::heap::end(baseline));
    #[cfg(not(feature = "probe-heap"))]
    let heap: Option<()> = None;
    let mut digest = cv_core::digest::Sha256::new();
    let mut reader = std::fs::File::open(path)?;
    let mut buffer = [0u8; 65536];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    println!(
        "{}",
        serde_json::json!({"source_content_bytes":elided[0].content.len(),"output_bytes":std::fs::metadata(path)?.len(),"sha256":digest.finish_hex(),"requested_rust_heap":heap})
    );
    Ok(())
}
