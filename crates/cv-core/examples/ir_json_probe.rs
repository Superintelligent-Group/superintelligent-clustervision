//! Isolated full-fidelity Message JSON output probe. Source is read-only; stdout is a small
//! report. Run separate processes for `materialized` and `streamed`, using scratch
//! CLUSTERVISION_HOME and a worktree-specific CARGO_TARGET_DIR.
//! Usage: ir_json_probe streamed|materialized json|text|escaped SOURCE [compact] [count]
use anyhow::{bail, Context, Result};
use cv_core::{
    lazy::{Span, Text},
    Block, Json, Message, Resolver, Role,
};
use std::{
    io::{self, Write},
    path::PathBuf,
    time::Instant,
};
#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
struct Digest {
    hash: Option<cv_core::digest::Sha256>,
    bytes: u64,
}
impl Write for Digest {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(hash) = &mut self.hash {
            hash.update(bytes);
        }
        self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(3..=5).contains(&args.len()) {
        bail!("ir_json_probe streamed|materialized json|text|escaped SOURCE [compact] [count]");
    }
    std::env::var_os("CLUSTERVISION_HOME").context("use scratch CLUSTERVISION_HOME")?;
    let path = std::fs::canonicalize(PathBuf::from(&args[2]))?;
    let len = std::fs::metadata(&path)?.len();
    let mut message = Message::new(Role::Assistant);
    message.content.push(match args[1].as_str() {
        "json" => Block::ToolUse {
            id: "id".into(),
            name: "tool".into(),
            input: Json::source(&path, 0, len)?,
            namespace: None,
        },
        "text" | "escaped" => Block::Text {
            text: Text::Span(Span {
                source: Some(path.clone()),
                offset: 0,
                len,
                escaped: args[1] == "escaped",
            }),
        },
        _ => bail!("unknown payload kind"),
    });
    let resolver = Resolver::new(Some(path));
    let pretty = !args[3..].iter().any(|arg| arg == "compact");
    let mut digest = Digest {
        hash: (!args[3..].iter().any(|arg| arg == "count")).then(cv_core::digest::Sha256::new),
        bytes: 0,
    };
    #[cfg(feature = "probe-heap")]
    let baseline = cv_testkit::heap::begin();
    let started = Instant::now();
    match args[0].as_str() {
        "materialized" => {
            message.materialize(&resolver)?;
            if pretty {
                serde_json::to_writer_pretty(&mut digest, &message)?;
            } else {
                serde_json::to_writer(&mut digest, &message)?;
            }
        }
        "streamed" => {
            if pretty {
                message.write_json_pretty(&mut digest, &resolver)?;
            } else {
                message.write_json(&mut digest, &resolver)?;
            }
        }
        _ => bail!("unknown output method"),
    }
    let elapsed = started.elapsed().as_secs_f64();
    #[cfg(feature = "probe-heap")]
    let heap = cv_testkit::heap::end(baseline);
    #[cfg(not(feature = "probe-heap"))]
    let heap: Option<()> = None;
    println!(
        "{}",
        serde_json::json!({"method":args[0],"kind":args[1],"source_bytes":len,"pretty":pretty,"mmap":cfg!(feature="mmap"),"output_bytes":digest.bytes,"sha256":digest.hash.map(cv_core::digest::Sha256::finish_hex),"seconds":elapsed,"requested_rust_heap":heap})
    );
    Ok(())
}
