//! Isolated selected-splice heap probe. Supply synthetic transcript paths and scratch
//! CLUSTERVISION_HOME. Each source contributes only its last message; no timings are collected.
use anyhow::{Context, Result};
use cv_core::{Harness, SessionRef};
#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
fn main() -> Result<()> {
    std::env::var_os("CLUSTERVISION_HOME").context("use scratch CLUSTERVISION_HOME")?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    let adapter = cv_core::harness::for_harness(Harness::Claude).unwrap();
    #[cfg(feature = "probe-heap")]
    let baseline = cv_testkit::heap::begin();
    let mut source_messages = 0;
    let output = match args[0].as_str() {
        "legacy" => {
            let mut owners = Vec::new();
            for path in &args[1..] {
                let reference = SessionRef::new("source", Harness::Claude, path);
                let mut source = adapter.parse(&reference)?;
                source.materialize()?;
                source_messages += source.messages.len();
                owners.push(source);
            }
            let spans: Vec<_> = owners
                .iter()
                .map(|source| cv_core::loom::Span {
                    source,
                    start: 8,
                    end: None,
                })
                .collect();
            cv_core::loom::splice(&spans, Some("fixed".into()), Harness::Claude)
        }
        "selected" => {
            let mut builder = cv_core::loom::Composition::default();
            for path in &args[1..] {
                let reference = SessionRef::new("source", Harness::Claude, path);
                let (source, total) = cv_core::emit::prepare_selected(adapter.as_ref(), &reference, 8, None)?;
                source_messages += total;
                anyhow::ensure!(total == 9, "fixture must have nine messages");
                builder.push(source, total, 8, None);
            }
            let mut source = builder.finish(Some("fixed".into()), Harness::Claude);
            source.materialize()?;
            source
        }
        _ => anyhow::bail!("splice_probe legacy|selected SYNTHETIC_SOURCE..."),
    };
    let bytes = serde_json::to_vec(&output)?;
    #[cfg(feature = "probe-heap")]
    let heap = Some(cv_testkit::heap::end(baseline));
    #[cfg(not(feature = "probe-heap"))]
    let heap: Option<()> = None;
    let mut digest = cv_core::digest::Sha256::new();
    digest.update(&bytes);
    println!(
        "{}",
        serde_json::json!({"source_messages":source_messages,"messages":output.messages.len(),"bytes":bytes.len(),"sha256":digest.finish_hex(),"requested_rust_heap":heap})
    );
    Ok(())
}
