//! Explicit-root composed token statistics probe: one corpus-wide production fold per process.
//! Pass only a frozen scratch directory of Claude JSONL sources and a scratch catalog home.
use anyhow::{Context, Result};
use cv_core::ir::{Harness, SessionRef};
fn main() -> Result<()> {
    let root = std::env::args().nth(1).context("usage_fleet_probe ROOT")?;
    let root = std::fs::canonicalize(root)?;
    let scratch = std::env::var_os("CLUSTERVISION_HOME").context("scratch CLUSTERVISION_HOME required")?;
    let scratch = std::fs::canonicalize(scratch)?;
    if let Some(real) = dirs::home_dir().map(|p| p.join(".clustervision")) {
        let real = std::fs::canonicalize(&real).unwrap_or(real);
        anyhow::ensure!(!scratch.starts_with(real), "real catalog refused");
    }
    let mut paths = std::fs::read_dir(&root)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|path| path.extension().is_some_and(|ext| ext == "jsonl"));
    paths.sort();
    let refs: Vec<_> = paths
        .into_iter()
        .map(|path| {
            SessionRef::new(
                path.file_stem().unwrap().to_string_lossy().into_owned(),
                Harness::Claude,
                path,
            )
        })
        .collect();
    let start = std::time::Instant::now();
    let result = cv_core::stats::TokenStats::compute(&refs)?;
    println!(
        "{}",
        serde_json::json!({"wall_seconds":start.elapsed().as_secs_f64(),"stats":result.to_json()})
    );
    Ok(())
}
