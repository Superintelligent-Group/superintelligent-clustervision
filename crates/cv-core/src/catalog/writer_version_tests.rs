use super::*;
use crate::source::{Baseline, Probe, ReadSet};
use rusqlite::{params, Connection};

fn session(w: &std::path::Path, id: &str) -> SessionRef {
    let path = w.join(format!("{id}.jsonl"));
    std::fs::write(&path, format!("{{\"id\":\"{id}\"}}\n")).unwrap();
    SessionRef::new(id, Harness::Claude, path)
}

/// The baseline of a complete read of `r`'s primary, as the reader that examines it captures.
fn read_of(r: &SessionRef) -> Baseline {
    let mut reads = ReadSet::default();
    reads.record(&r.path);
    Probe::default().snapshot(r).with_reads(&reads)
}

fn note(r: &SessionRef, version: Option<&str>) {
    note_writer_version(r, &read_of(r), version);
}

fn version(r: &SessionRef) -> Option<Option<String>> {
    WriterVersions::load().version(r).map(|v| v.map(str::to_owned))
}

fn column(r: &SessionRef) -> Option<String> {
    open_db()
        .unwrap()
        .query_row(
            "SELECT writer_version FROM sessions WHERE harness=?1 AND id=?2 AND path=?3",
            params![r.harness.as_str(), r.id, r.path.to_string_lossy()],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn a_version_survives_metadata_replacement_and_dies_with_its_source() {
    let w = crate::test_world("writer-version-retention");
    let r = session(&w.base, "kept");
    let other = session(&w.base, "other");
    sync(&[r.clone(), other.clone()]);
    assert_eq!(version(&r), None, "a fresh row was never examined");
    note(&r, Some("2.1.0"));
    assert_eq!(version(&r), Some(Some("2.1.0".into())));

    // Additive upsert, then a full per-harness rewrite with changed metadata and a new sibling.
    let mut retitled = r.clone();
    retitled.title = Some("new title".into());
    retitled.message_count = 9;
    sync(std::slice::from_ref(&retitled));
    assert_eq!(version(&r), Some(Some("2.1.0".into())));
    let extra = session(&w.base, "extra");
    replace_harness(
        Harness::Claude,
        &[extra, retitled, other.clone()],
        &[],
        &Default::default(),
    );
    assert_eq!(version(&r), Some(Some("2.1.0".into())));
    assert_eq!(version(&other), None);

    // A session that vanishes and comes back is a row nobody examined.
    replace_harness(Harness::Claude, std::slice::from_ref(&other), &[], &Default::default());
    replace_harness(Harness::Claude, &[r.clone(), other], &[], &Default::default());
    assert_eq!(column(&r), None);
    assert_eq!(version(&r), None);

    // The same row, retained, stops describing a source that changed.
    note(&r, Some("2.1.0"));
    std::fs::write(&r.path, "{\"id\":\"kept\"}\n{\"more\":1}\n").unwrap();
    assert_eq!(column(&r).as_deref(), Some("2.1.0"));
    assert_eq!(version(&r), None, "the changed source invalidates the version");
    assert!(WriterVersions::load().stale(&r));
    note(&r, Some("2.2.0"));
    assert_eq!(version(&r), Some(Some("2.2.0".into())));
}

#[test]
fn examined_without_a_version_settles_but_unexamined_and_incomplete_do_not() {
    let w = crate::test_world("writer-version-settled");
    let r = session(&w.base, "bare");
    let unlisted = session(&w.base, "unlisted");
    let incomplete = session(&w.base, "incomplete");
    sync(&[r.clone(), incomplete.clone()]);
    note(&r, None);
    assert_eq!(version(&r), Some(None));
    assert_eq!(
        column(&r).as_deref(),
        Some(""),
        "absent is stored, NULL means unexamined"
    );

    note(&unlisted, Some("1.0.0"));
    assert_eq!(
        version(&unlisted),
        None,
        "a session the catalog does not list records nothing"
    );
    note_writer_version(&incomplete, &Probe::default().snapshot(&incomplete), Some("1.0.0"));
    assert_eq!(
        version(&incomplete),
        None,
        "a baseline without its read set publishes nothing"
    );

    let stale = WriterVersions::load();
    assert!(!stale.stale(&r) && stale.stale(&incomplete) && stale.stale(&unlisted));
}

#[test]
fn a_current_catalog_with_unexamined_rows_backfills_without_another_bump() {
    let w = crate::test_world("writer-version-backfill");
    let r = session(&w.base, "old");
    let path = db_path().unwrap();
    {
        // A catalog as 0.17 left it: no timestamp or writer columns, interpretation 4.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions(harness TEXT NOT NULL, id TEXT NOT NULL, path TEXT NOT NULL, cwd TEXT,
                 title TEXT, created_at INTEGER, updated_at INTEGER, message_count INTEGER,
                 PRIMARY KEY(harness,id,path));
             CREATE TABLE catalog_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO catalog_meta VALUES('discovery_version','4');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions(harness,id,path,message_count) VALUES('claude','old',?1,3)",
            [r.path.to_string_lossy()],
        )
        .unwrap();
    }
    let stored = |conn: &Connection| -> String {
        conn.query_row(
            "SELECT value FROM catalog_meta WHERE key='discovery_version'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    let conn = open_db().unwrap();
    assert_eq!(stored(&conn), crate::discover_cache::INTERPRETATION_VERSION.to_string());
    assert_eq!(column(&r), None, "the migrated row exists and is unexamined");
    assert!(WriterVersions::load().stale(&r));

    // H's backfill runs on a catalog already at the current version; nothing bumps again.
    let mut writer = Writer::open().unwrap();
    writer.record_writer_version(&r, &read_of(&r), Some("0.99.0"));
    drop(writer);
    drop(conn);
    let conn = open_db().unwrap();
    assert_eq!(stored(&conn), crate::discover_cache::INTERPRETATION_VERSION.to_string());
    assert_eq!(version(&r), Some(Some("0.99.0".into())));
}

#[cfg(unix)]
#[test]
fn a_read_only_catalog_serves_versions_and_drops_writes() {
    use std::os::unix::fs::PermissionsExt;
    let w = crate::test_world("writer-version-read-only");
    let r = session(&w.base, "ro");
    sync(std::slice::from_ref(&r));
    note(&r, Some("1.2.3"));
    let set_mode = |mode: u32| {
        for entry in std::fs::read_dir(&w.cv_home).unwrap() {
            let path = entry.unwrap().path();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o666)).unwrap();
        }
        std::fs::set_permissions(&w.cv_home, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    set_mode(0o555);
    let served = std::panic::catch_unwind(|| {
        let r = SessionRef::new("ro", Harness::Claude, w.base.join("ro.jsonl"));
        let served = version(&r);
        note(&r, Some("9.9.9"));
        (served, version(&r))
    });
    set_mode(0o755);
    let (before, after) = served.unwrap();
    assert_eq!(before, Some(Some("1.2.3".into())));
    assert_eq!(after, before, "the write was dropped");
}
