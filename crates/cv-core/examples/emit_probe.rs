//! Synthetic native export probe; writes only to the explicit output directory.
use anyhow::Result;
use cv_core::{Block, Harness, Message, Role, Session};
#[cfg(feature = "probe-heap")]
#[global_allocator]
static ALLOCATOR: cv_testkit::heap::Allocator = cv_testkit::heap::Allocator;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let count: usize = args[1].parse()?;
    let bytes: usize = args[2].parse()?;
    let mut session = Session::new("synthetic-export", Harness::Codex);
    session.created_at = Some("2026-10-12T12:00:00Z".parse()?);
    session.cwd = Some("/synthetic/export".into());
    for _ in 0..count {
        let mut message = Message::new(Role::User);
        message.content.push(Block::Text {
            text: "a\"\\\nλ".repeat(bytes / 6).into(),
        });
        session.messages.push(message);
    }
    #[cfg(feature = "probe-heap")]
    let baseline = cv_testkit::heap::begin();
    let start = std::time::Instant::now();
    let result = if args.get(4).is_some_and(|a| a == "report") {
        cv_core::emit::emit_report(
            &session,
            Harness::Codex,
            std::path::Path::new(&args[3]),
            &cv_core::emit::EmitOptions {
                new_id: Some("00000000-0000-0000-0000-000000000001".into()),
                ..Default::default()
            },
        )?
        .0
    } else {
        cv_core::emit::emit(
            &session,
            Harness::Codex,
            std::path::Path::new(&args[3]),
            &cv_core::emit::EmitOptions {
                new_id: Some("00000000-0000-0000-0000-000000000001".into()),
                ..Default::default()
            },
        )?
    };
    #[cfg(feature = "probe-heap")]
    eprintln!("{}", serde_json::to_string(&cv_testkit::heap::end(baseline))?);
    println!("{} {}", start.elapsed().as_secs_f64(), result.path.display());
    Ok(())
}
