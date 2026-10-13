//! Scratch-only verified native port (Claude by default; optional final target argument).
use anyhow::{Context, Result};
use cv_core::{
    emit::{self, EmitOptions, ThinkingMode},
    Harness, SessionRef,
};
#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let source = Harness::parse(&args[1]).context("unknown source harness")?;
    let adapter = cv_core::harness::for_harness(source).context("source adapter")?;
    let reference = SessionRef::new("synthetic-source", source, &args[2]);
    let mode = ThinkingMode::parse(&args[4]).context("unknown thinking mode")?;
    let target = match args.get(5) {
        Some(value) => Harness::parse(value).context("unknown target harness")?,
        None => Harness::Claude,
    };
    #[cfg(feature = "probe-heap")]
    let baseline = cv_testkit::heap::begin();
    let session = emit::parse_for_emit(adapter.as_ref(), &reference, target)?;
    let (result, report) = emit::emit_report(
        &session,
        target,
        std::path::Path::new(&args[3]),
        &EmitOptions {
            new_id: Some("00000000-0000-0000-0000-000000000001".into()),
            thinking: mode,
            ..Default::default()
        },
    )?;
    #[cfg(feature = "probe-heap")]
    let heap = cv_testkit::heap::end(baseline);
    println!(
        "{}",
        serde_json::json!({"path":result.path,"messages":session.messages.len(),"fidelity":report})
    );
    #[cfg(feature = "probe-heap")]
    eprintln!("{}", serde_json::to_string(&heap)?);
    Ok(())
}
