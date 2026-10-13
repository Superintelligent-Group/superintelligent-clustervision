//! Isolated distill prefix preparation heap control. Supply a synthetic nine-message source
//! whose first message is tiny and remaining messages hold discarded history. No timings.
use anyhow::{Context, Result};
use cv_core::{Harness, SessionRef};
#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
fn main() -> Result<()> {
    std::env::var_os("CLUSTERVISION_HOME").context("use scratch CLUSTERVISION_HOME")?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    let reference = SessionRef::new("source", Harness::Claude, &args[1]);
    let adapter = cv_core::harness::for_harness(Harness::Claude).unwrap();
    #[cfg(feature = "probe-heap")]
    let baseline = cv_testkit::heap::begin();
    let (mut session, total) = match args[0].as_str() {
        "legacy" => {
            let mut source = adapter.parse(&reference)?;
            source.materialize()?;
            let total = source.messages.len();
            source.messages.truncate(1);
            (source, total)
        }
        "selected" => cv_core::emit::prepare_selected(adapter.as_ref(), &reference, 0, Some(1))?,
        _ => anyhow::bail!("distill_prefix_probe legacy|selected SYNTHETIC_SOURCE"),
    };
    anyhow::ensure!(total == 9, "fixture must have nine messages");
    session.materialize()?;
    let bytes = serde_json::to_vec(&session)?;
    #[cfg(feature = "probe-heap")]
    let heap = Some(cv_testkit::heap::end(baseline));
    #[cfg(not(feature = "probe-heap"))]
    let heap: Option<()> = None;
    let mut digest = cv_core::digest::Sha256::new();
    digest.update(&bytes);
    println!(
        "{}",
        serde_json::json!({"source_messages":total,"selected_messages":session.messages.len(),"output_bytes":bytes.len(),"sha256":digest.finish_hex(),"requested_rust_heap":heap})
    );
    Ok(())
}
