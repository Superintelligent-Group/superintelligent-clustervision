//! Isolated prune probe: source must be a scratch fixture, never a live harness store.
use anyhow::Result;
use cv_core::prune::{prune_session, PruneOptions};
#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mut options = PruneOptions {
        new_id: Some("22222222-2222-4222-8222-222222222222".into()),
        keep_last: 2,
        ..Default::default()
    };
    for mode in args.iter().skip(2) {
        match mode.as_str() {
            "revive" => options.revive = true,
            "window" => options.window = Some(4000),
            "range" => options.keep_range = Some((3, Some(12))),
            "drop" => options.drop = true,
            "thinking" => options.thinking = true,
            "declassify" => {
                options.declassify = true;
                options.declassify_tokens = vec!["alpha".into(), "beta".into()];
            }
            "dry" => options.dry_run = true,
            "resources" => options.copy_resources = true,
            _ => anyhow::bail!("unknown probe mode {mode}"),
        }
    }
    #[cfg(feature = "probe-heap")]
    let baseline = cv_testkit::heap::begin();
    let start = std::time::Instant::now();
    let result = prune_session(std::path::Path::new(&args[1]), &options)?;
    let elapsed = start.elapsed().as_secs_f64();
    #[cfg(feature = "probe-heap")]
    let heap = Some(cv_testkit::heap::end(baseline));
    #[cfg(not(feature = "probe-heap"))]
    let heap: Option<serde_json::Value> = None;
    println!(
        "{}",
        serde_json::json!({"result": result, "usage_rewritten":result.usage_rewritten,"revive_tokens":result.revive_tokens,"revive_old_tokens":result.revive_old_tokens,"seconds":elapsed,"heap":heap})
    );
    Ok(())
}
