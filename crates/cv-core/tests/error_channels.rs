//! Optional side stores distinguish absence from a failed read.
use cv_core::{Harness, SessionRef, SubagentDemand};
use std::fs;

#[test]
fn forest_errors_reach_readers_and_selected_metadata() {
    let world = cv_testkit::World::in_process("error-channels", cv_core::env::Env::enter_isolated);
    let path = world.home.join("parent.jsonl");
    let r = SessionRef::new("parent", Harness::Claude, &path);
    assert!(cv_core::subagent_tree_of(&r).unwrap().is_empty());
    let dir = path.with_extension("").join("subagents");
    fs::create_dir_all(dir.parent().unwrap()).unwrap();
    fs::write(&dir, "not a directory").unwrap();
    assert!(cv_core::subagent_tree_of(&r).is_err());
    fs::remove_file(&dir).unwrap();
    fs::create_dir(&dir).unwrap();
    fs::write(
        dir.join("agent-child.jsonl"),
        r#"{"type":"user","uuid":"u","sessionId":"agent-child","message":{"role":"user","content":"hello"}}
"#,
    )
    .unwrap();
    fs::write(dir.join("agent-child.meta.json"), "{broken").unwrap();
    assert_eq!(
        cv_core::subagent_tree_filtered_of(&r, SubagentDemand::Sessions, None, |_, _| true)
            .unwrap()
            .len(),
        1
    );
    assert!(cv_core::subagent_tree_of(&r).is_err());
    assert!(
        cv_core::subagent_tree_filtered_of(&r, SubagentDemand::Rich, None, |_, _| false)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn subagent_refs_propagate_directory_and_database_failures() {
    let world = cv_testkit::World::in_process("error-channels", cv_core::env::Env::enter_isolated);
    let r = SessionRef::new("parent", Harness::Kimi, &world.home);
    fs::write(world.home.join("subagents"), "not a directory").unwrap();
    assert!(cv_core::harness::kimi::subagent_refs(&r).is_err());
    fs::write(world.home.join("agents"), "not a directory").unwrap();
    assert!(cv_core::harness::kimi_code::subagent_refs(&r).is_err());
    #[cfg(feature = "sqlite")]
    {
        let db = world.home.join("broken.db");
        fs::write(&db, "not a database").unwrap();
        let r = SessionRef::new("parent", Harness::Hermes, &db);
        assert!(cv_core::harness::hermes::subagent_refs(&r).is_err());
        assert!(cv_core::harness::devin::subagent_tree(&db, "parent").is_err());
    }
}
