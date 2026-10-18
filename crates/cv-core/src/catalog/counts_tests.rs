use super::imp::{count_record_in, turns_of};
use super::*;
use crate::source::{Probe, ReadSet};

fn publish(r: &SessionRef, n: u32, reads: &ReadSet) {
    let source = Probe::default().snapshot(r).with_reads(reads);
    note_affordances_with_source(r, &source, || {
        [(crate::affordance::Kind::Plan, n)].into_iter().collect()
    });
}

fn compare(reader: &Reader, r: &SessionRef, expected: Option<u32>) {
    let source = Probe::default().snapshot(r);
    let demand = reader.counts_with_probe(r, &source, &mut Probe::default());
    let batch = AffordanceTable::load().counts_with_source(r, &source);
    assert_eq!(demand, batch);
    assert_eq!(
        demand.map(|c| c.get(&crate::affordance::Kind::Plan).copied().unwrap_or(0)),
        expected
    );
}

#[test]
fn demand_counts_match_batch_baselines_and_reject_stale_pairs() {
    let w = crate::test_world("counts-demand-baselines");
    let path = w.base.join("sessions.db");
    std::fs::write(&path, "database").unwrap();
    let r = SessionRef::new("selected", crate::Harness::Hermes, path.clone());
    let conn = open_db().unwrap();
    let reader = Reader::open().unwrap();
    compare(&reader, &r, None);
    publish(&r, 1, &ReadSet::default());
    compare(&reader, &r, Some(1));
    for mutation in [
        "UPDATE affordance_counts SET counts='{}'",         // missing view version
        "UPDATE affordance_counts SET counts='{\"_v\":0}'", // old view version
        "UPDATE affordance_counts SET counts='malformed'",
        "UPDATE affordance_counts SET counts=counts||' '", // the published output, not this text
        "UPDATE source_dependencies SET record=substr(record,1,13)||X'01'||substr(record,15) WHERE consumer='counts'",
        "UPDATE source_dependencies SET record=substr(record,1,53)||'{}' WHERE consumer='counts'",
        "UPDATE source_dependencies SET record=substr(record,1,53)||'{\"version\":1}' WHERE consumer='counts'",
        "UPDATE source_dependencies SET record=substr(record,1,53)||REPLACE(CAST(substr(record,54) AS TEXT),'\"reads\":[]','\"reads\":null') WHERE consumer='counts'",
        "DELETE FROM source_dependencies WHERE consumer='counts'",
        "DELETE FROM affordance_counts",
    ] {
        conn.execute_batch(mutation).unwrap();
        compare(&reader, &r, None);
        conn.execute_batch("DELETE FROM affordance_counts; DELETE FROM source_dependencies WHERE consumer='counts';")
            .unwrap();
        publish(&r, 1, &ReadSet::default());
        compare(&reader, &r, Some(1));
    }
    let wal = w.base.join("sessions.db-wal");
    std::fs::write(&wal, "committed change").unwrap();
    compare(&reader, &r, None);
    std::fs::remove_file(&wal).unwrap();
    compare(&reader, &r, Some(1));
    let conditional = w.base.join("conditional-sidecar");
    let mut reads = ReadSet::default();
    reads.record(&conditional); // Captured absence is part of the selected body's provenance.
    conn.execute_batch("DELETE FROM affordance_counts; DELETE FROM source_dependencies WHERE consumer='counts';")
        .unwrap();
    publish(&r, 1, &reads);
    compare(&reader, &r, Some(1));
    std::fs::write(&conditional, "appeared").unwrap();
    compare(&reader, &r, None);
    std::fs::write(&path, "changed primary").unwrap();
    compare(&reader, &r, None);
    // A retained reader does not pin a transaction across reductions/publications.
    publish(&r, 2, &ReadSet::default());
    compare(&reader, &r, Some(2));
    // Uncommitted writes cannot expose one half of a count/provenance publication.
    conn.execute_batch("BEGIN IMMEDIATE; UPDATE affordance_counts SET counts='{}'; DELETE FROM source_dependencies WHERE consumer='counts';").unwrap();
    assert_eq!(
        reader
            .counts_with_probe(&r, &Probe::default().snapshot(&r), &mut Probe::default())
            .unwrap()
            .get(&crate::affordance::Kind::Plan),
        Some(&2)
    );
    conn.execute_batch("COMMIT;").unwrap();
    compare(&reader, &r, None);
}

#[test]
fn demand_counts_decode_only_selected_rows_as_inventory_grows() {
    let w = crate::test_world("counts-demand-work");
    let path = w.base.join("selected.jsonl");
    std::fs::write(&path, "selected").unwrap();
    let r = SessionRef::new("selected", crate::Harness::Codex, path);
    let conn = open_db().unwrap();
    publish(&r, 1, &ReadSet::default());
    for inventory in [8, 1024] {
        conn.execute(
            "WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<?1)
             INSERT OR REPLACE INTO affordance_counts(harness,id,path,counts)
             SELECT 'codex','irrelevant-'||n,'unused-'||n,'malformed unrelated counts' FROM numbers",
            [inventory],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO source_dependencies(consumer,harness,id,path,record)
             SELECT 'counts',harness,id,path,X'00' FROM affordance_counts WHERE id LIKE 'irrelevant-%'",
            [],
        )
        .unwrap();
        let reader = Reader::open().unwrap();
        compare(&reader, &r, Some(1));
        assert_eq!(reader.counts_work(), (1, 1));
        let missing = SessionRef::new("missing", r.harness, r.path.clone());
        compare(&reader, &missing, None);
        assert_eq!(reader.counts_work(), (2, 1));
    }
}

#[test]
fn previous_source_epoch_cannot_certify_current_counts() {
    let world = crate::test_world("counts-source-epoch");
    let path = world.base.join("source.jsonl");
    std::fs::write(&path, "{}\n").unwrap();
    let r = SessionRef::new("epoch", Harness::Claude, path);
    sync(std::slice::from_ref(&r));
    publish(&r, 1, &ReadSet::default());
    assert!(AffordanceTable::load().counts(&r).is_some());
    let conn = open_db().unwrap();
    let mut record: Vec<u8> = conn
        .query_row(
            "SELECT record FROM source_dependencies WHERE consumer='counts' AND id='epoch'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let current = u32::from_le_bytes(record[..4].try_into().unwrap());
    record[..4].copy_from_slice(&(current - 1).to_le_bytes());
    conn.execute(
        "UPDATE source_dependencies SET record=?1 WHERE consumer='counts' AND id='epoch'",
        [&record],
    )
    .unwrap();
    assert!(
        AffordanceTable::load().counts(&r).is_none(),
        "unchanged payload and source cannot revive a previous-epoch receipt"
    );
}

fn counted(r: &SessionRef, turns: usize) {
    crate::stream::Counted {
        turns,
        counts: [(crate::affordance::Kind::Plan, 1)].into_iter().collect(),
        dependencies: ReadSet::default(),
    }
    .record_with_source(r, &Probe::default().snapshot(r));
}

fn refilled(reader: &Reader, r: &SessionRef) -> usize {
    let mut fresh = r.clone();
    fresh.message_count = 0;
    reader.refill_turns(std::iter::once(&mut fresh));
    fresh.message_count
}

#[test]
fn certified_turns_enrich_old_counts_refill_refresh_and_keep_partial_publication() {
    let world = crate::test_world("certified-turn-refresh");
    let path = world.base.join("state.db");
    std::fs::write(&path, "native source").unwrap();
    let r = SessionRef::new("session", Harness::Hermes, path);
    sync(std::slice::from_ref(&r));
    let reader = Reader::open().unwrap();
    assert_eq!(refilled(&reader, &r), 0);
    assert_eq!(
        reader.turn_snapshots(),
        0,
        "cold unknown count must not inventory dependencies"
    );
    publish(&r, 1, &ReadSet::default());
    assert_eq!(refilled(&reader, &r), 0);
    assert_eq!(
        reader.turn_snapshots(),
        0,
        "old counts without turns need no source probe"
    );
    counted(&r, 7);
    assert_eq!(refilled(&reader, &r), 7);
    // Native discovery can return unknown again without discarding a completed source count.
    let mut refreshed = r.clone();
    refreshed.title = Some("same source metadata".into());
    reader.refill_turns(std::iter::once(&mut refreshed));
    let mut owner = Reader::open().unwrap();
    owner.replace_harness(r.harness, std::slice::from_ref(&refreshed), &[], &Default::default());
    assert_eq!(owner.all_sessions().unwrap()[0].message_count, 7);
    // Neither a stale scalar nor partial affordance publication can impersonate exact turns.
    let conn = open_db().unwrap();
    conn.execute("UPDATE sessions SET message_count=999", []).unwrap();
    publish(&r, 99, &ReadSet::default());
    assert_eq!(refilled(&reader, &r), 7);
    assert_eq!(
        reader
            .counts_with_probe(&r, &Probe::default().snapshot(&r), &mut Probe::default())
            .unwrap()[&crate::affordance::Kind::Plan],
        1
    );
    crate::catalog::Writer::open().unwrap().record_counted(
        &r,
        &Probe::default().snapshot(&r),
        crate::stream::Counted {
            turns: 8,
            counts: Default::default(),
            dependencies: Default::default(),
        },
    );
    assert_eq!(
        refilled(&reader, &r),
        8,
        "serial index writer publishes exact turns too"
    );
}

#[test]
fn exact_turn_extensions_require_valid_payload_receipt_version_and_integer() {
    let world = crate::test_world("exact-turn-cache-controls");
    let path = world.base.join("source.jsonl");
    std::fs::write(&path, "{}\n").unwrap();
    let r = SessionRef::new("row", Harness::Claude, path);
    sync(std::slice::from_ref(&r));
    counted(&r, 7);
    let reader = Reader::open().unwrap();
    let conn = open_db().unwrap();
    let original: String = conn
        .query_row("SELECT counts FROM affordance_counts", [], |row| row.get(0))
        .unwrap();
    conn.execute("UPDATE affordance_counts SET counts=REPLACE(counts,'7','8')", [])
        .unwrap();
    assert_eq!(
        refilled(&reader, &r),
        0,
        "payload tamper cannot borrow its former receipt"
    );
    for value in [
        None,
        Some(serde_json::Value::Null),
        Some(serde_json::json!(-1)),
        Some(serde_json::json!("7")),
        Some(serde_json::json!(true)),
        Some(serde_json::json!([])),
        Some(serde_json::json!(u64::MAX)),
    ] {
        let mut object: serde_json::Value = serde_json::from_str(&original).unwrap();
        match value {
            Some(value) => object["_turns"] = value,
            None => {
                object.as_object_mut().unwrap().remove("_turns");
            }
        }
        let text = object.to_string();
        conn.execute("UPDATE affordance_counts SET counts=?1", [&text]).unwrap();
        crate::source::publish_in(&conn, "counts", &r, &Probe::default().snapshot(&r), text.as_bytes()).unwrap();
        let before = reader.turn_snapshots();
        assert_eq!(refilled(&reader, &r), 0);
        assert_eq!(
            reader.turn_snapshots(),
            before,
            "invalid scalar fails before dependency lookup"
        );
    }
    let mut old: serde_json::Value = serde_json::from_str(&original).unwrap();
    old["_v"] = 0.into();
    let text = old.to_string();
    conn.execute("UPDATE affordance_counts SET counts=?1", [&text]).unwrap();
    crate::source::publish_in(&conn, "counts", &r, &Probe::default().snapshot(&r), text.as_bytes()).unwrap();
    assert_eq!(refilled(&reader, &r), 0);
    assert_eq!(
        turns_of(&serde_json::json!({"_v":crate::affordance::VIEW_VERSION,"_turns":0}).to_string()),
        Some(0)
    );
}

#[test]
fn certified_turns_reject_database_wal_and_replaced_source_changes() {
    let world = crate::test_world("certified-turn-source-changes");
    let path = world.base.join("state.db");
    std::fs::write(&path, "native source").unwrap();
    let r = SessionRef::new("session", Harness::Hermes, path.clone());
    sync(std::slice::from_ref(&r));
    let reader = Reader::open().unwrap();
    counted(&r, 4);
    assert_eq!(refilled(&reader, &r), 4);
    let wal = world.base.join("state.db-wal");
    std::fs::write(&wal, "new frames").unwrap();
    assert_eq!(refilled(&reader, &r), 0);
    counted(&r, 5);
    assert_eq!(refilled(&reader, &r), 5);
    std::fs::remove_file(&wal).unwrap();
    assert_eq!(refilled(&reader, &r), 0);
    counted(&r, 6);
    std::fs::write(&path, "changed database").unwrap();
    assert_eq!(refilled(&reader, &r), 0);
    counted(&r, 7);
    let replacement = world.base.join("replacement");
    std::fs::write(&replacement, "changed database").unwrap();
    std::fs::rename(replacement, path).unwrap();
    assert_eq!(refilled(&reader, &r), 0);
}

#[test]
fn turn_payload_receipt_and_session_refill_commit_or_rollback_together() {
    let world = crate::test_world("turn-publication-atomic");
    let path = world.base.join("state.db");
    std::fs::write(&path, "source").unwrap();
    let r = SessionRef::new("session", Harness::Hermes, path);
    sync(std::slice::from_ref(&r));
    counted(&r, 7);
    let conn = open_db().unwrap();
    let before = count_record_in(&conn, &r).unwrap();
    conn.execute_batch("UPDATE sessions SET message_count=0; CREATE TRIGGER reject_turns BEFORE UPDATE OF message_count ON sessions BEGIN SELECT RAISE(ABORT,'counterexample'); END;").unwrap();
    counted(&r, 9);
    assert_eq!(count_record_in(&conn, &r).unwrap(), before);
    assert_eq!(Reader::open().unwrap().all_sessions().unwrap()[0].message_count, 0);
    conn.execute_batch("DROP TRIGGER reject_turns;").unwrap();
    counted(&r, 9);
    assert_eq!(refilled(&Reader::open().unwrap(), &r), 9);
    assert_eq!(Reader::open().unwrap().all_sessions().unwrap()[0].message_count, 9);
}

#[test]
fn stopped_stream_never_certifies_exact_turns() {
    struct Two;
    impl crate::Adapter for Two {
        fn harness(&self) -> Harness {
            Harness::Hermes
        }
        fn roots(&self) -> Vec<std::path::PathBuf> {
            Vec::new()
        }
        fn discover(&self) -> anyhow::Result<Vec<SessionRef>> {
            Ok(Vec::new())
        }
        fn stream(
            &self,
            r: &SessionRef,
            _: &crate::ParseOptions,
            sink: &mut dyn crate::MessageSink,
        ) -> anyhow::Result<crate::Session> {
            for _ in 0..2 {
                if sink.message(crate::Message::of_kind(
                    crate::Role::User,
                    crate::MessageKind::Prompt,
                    crate::Origin::Human,
                )) == crate::Flow::Stop
                {
                    break;
                }
            }
            Ok(crate::Session::new(&r.id, r.harness))
        }
    }
    let world = crate::test_world("partial-turn-no-certificate");
    let path = world.base.join("state.db");
    std::fs::write(&path, "source").unwrap();
    let r = SessionRef::new("session", Harness::Hermes, path);
    sync(std::slice::from_ref(&r));
    let read = crate::stream::stream_counted(
        &Two,
        &r,
        &crate::ParseOptions::lazy(),
        &mut |_| crate::Flow::Stop,
        crate::stream::Pass::RECORD,
    )
    .unwrap();
    assert!(read.counted.is_none());
    assert_eq!(refilled(&Reader::open().unwrap(), &r), 0);
}

#[test]
fn open_discovery_context_sees_certified_directory_turns_without_cache_key_mutation() {
    let world = crate::test_world("context-directory-exact-turns");
    let directory = world.home.join(".kimi/sessions/project/session");
    std::fs::create_dir_all(&directory).unwrap();
    let primary = directory.join("context.jsonl");
    std::fs::write(&primary, "{}\n").unwrap();
    let r = SessionRef::new("session", Harness::Kimi, directory);
    let context = crate::discover_cache::read::Context::current();
    let _scope = context.enter();
    let scans = std::cell::Cell::new(0);
    let discover = || {
        crate::discover_cache::cached_scan_many(&primary, || {
            scans.set(scans.get() + 1);
            vec![r.clone()]
        })
    };
    assert_eq!(discover()[0].message_count, 0);
    sync(std::slice::from_ref(&r));
    counted(&r, 7);
    assert_eq!(
        discover()[0].message_count,
        7,
        "already-open Context observes publication by logical directory key"
    );
    assert_eq!(
        scans.get(),
        1,
        "certified refill does not reparse the unchanged primary"
    );
    std::fs::write(&primary, "{}\n{}\n").unwrap();
    assert_eq!(discover()[0].message_count, 0);
    assert_eq!(scans.get(), 2);
}

#[test]
fn complete_count_publication_waits_for_writer_and_commits_one_pair() {
    use std::sync::mpsc;
    use std::time::Duration;
    let w = crate::test_world("count-held-writer");
    let path = w.base.join("session.jsonl");
    std::fs::write(&path, "source").unwrap();
    let r = SessionRef::new("s", crate::Harness::Codex, path);
    sync(std::slice::from_ref(&r));
    let source = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
    let mut publishing = open_db().unwrap();
    let mut blocking = open_db().unwrap();
    let lock = blocking
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    lock.execute("UPDATE sessions SET message_count=0", []).unwrap();
    let (started, entered) = mpsc::channel();
    let (finished, result) = mpsc::channel();
    let worker = std::thread::spawn(crate::env::carry(move || {
        started.send(()).unwrap();
        super::imp::note_counts_in(&mut publishing, &r, &source, Some(9), Default::default);
        finished.send(()).unwrap();
    }));
    entered.recv_timeout(Duration::from_secs(10)).unwrap();
    let early = result.recv_timeout(Duration::from_millis(100)).is_ok();
    lock.commit().unwrap();
    if !early {
        result.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    worker.join().unwrap();
    assert!(!early, "publication must wait for the held writer");
    let reader = Reader::open().unwrap();
    let r = reader.all_sessions().unwrap().remove(0);
    assert_eq!(r.message_count, 9);
    assert_eq!(refilled(&reader, &r), 9);
}

#[test]
fn partial_publication_preserves_a_concurrent_certified_complete_pass() {
    use std::sync::mpsc;
    use std::time::Duration;
    let w = crate::test_world("partial-count-race");
    let path = w.base.join("session.jsonl");
    std::fs::write(&path, "source").unwrap();
    let r = SessionRef::new("s", crate::Harness::Codex, path);
    sync(std::slice::from_ref(&r));
    let source = Probe::default().snapshot(&r).with_reads(&ReadSet::default());
    let mut publishing = open_db().unwrap();
    let partial_ref = r.clone();
    let (ready, started) = mpsc::channel();
    let (resume, released) = mpsc::channel();
    let worker = std::thread::spawn(crate::env::carry(move || {
        super::imp::note_counts_in(&mut publishing, &partial_ref, &source, None, || {
            // The old pair has been read, but there is no writer transaction yet.
            ready.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(10)).unwrap();
            [(crate::affordance::Kind::Plan, 99)].into_iter().collect()
        });
    }));
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    counted(&r, 9);
    resume.send(()).unwrap();
    worker.join().unwrap();
    let reader = Reader::open().unwrap();
    assert_eq!(refilled(&reader, &r), 9);
    assert_eq!(
        reader
            .counts_with_probe(&r, &Probe::default().snapshot(&r), &mut Probe::default())
            .unwrap()[&crate::affordance::Kind::Plan],
        1
    );
}
