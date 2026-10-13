//! Explicit scratch Claude transcript -> verified Codex export, including source parsing.
use anyhow::Result;
use cv_core::harness::Parse;
use cv_core::{Harness, SessionRef};
#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let adapter = cv_core::harness::claude::Claude::detect();
    let reference = SessionRef::new("synthetic-source", Harness::Claude, &args[1]);
    #[cfg(feature = "probe-heap")]
    let baseline = cv_testkit::heap::begin();
    let start = std::time::Instant::now();
    let session = if args.get(3).is_some_and(|arg| arg == "lazy") {
        cv_core::emit::parse_for_emit(&adapter, &reference, Harness::Codex)?
    } else {
        adapter.parse(&reference)?
    };
    let (result, report) = cv_core::emit::emit_report(
        &session,
        Harness::Codex,
        std::path::Path::new(&args[2]),
        &cv_core::emit::EmitOptions {
            new_id: Some("00000000-0000-0000-0000-000000000001".into()),
            ..Default::default()
        },
    )?;
    #[cfg(feature = "probe-heap")]
    eprintln!("{}", serde_json::to_string(&cv_testkit::heap::end(baseline))?);
    println!("{} {}", start.elapsed().as_secs_f64(), result.path.display());
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
