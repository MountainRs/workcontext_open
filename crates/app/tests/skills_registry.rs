use std::{fs, path::Path};
use tempfile::tempdir;
use workcontext_app::skills::{find, register, Entry};
use workcontext_store::rusqlite::Connection;

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE work_thread(profile TEXT,id TEXT); INSERT INTO work_thread VALUES('current','scope')").unwrap();
    conn
}
fn entry(root: &Path, name: &str, scope: Option<&str>) -> Entry {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("SKILL.md");
    fs::write(&path, name).unwrap();
    Entry { name: name.into(), purpose: "Lookup".into(), path, scope_id: scope.map(str::to_owned) }
}
#[test]
fn registry_roundtrip_and_missing_original() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let db = db();
    assert!(find(&db, root, None, "", None, 0, 20).unwrap().items.is_empty());
    assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    for (name, profile, scope) in [("other", Some("other"), None), ("shared", None, None), ("private", Some("current"), Some("scope"))] {
        register(&db, root, profile, entry(root, name, scope)).unwrap();
    }
    let first = find(&db, root, Some("current"), "LOOK", None, 0, 1).unwrap();
    assert_eq!(first.next_cursor.as_deref(), Some("1"));
    assert!(first.items[0].global && first.items[0].available);
    fs::remove_file(root.join("private/SKILL.md")).unwrap();
    let matched = find(&db, root, Some("current"), "", Some("scope"), 0, 1).unwrap();
    assert_eq!(matched.items[0].entry.name, "private");
    assert!(!matched.items[0].available);
    assert_eq!(find(&db, root, Some("other"), "", None, 0, 2).unwrap().items[1].entry.name, "other");
    assert_eq!(fs::read(root.join("shared/SKILL.md")).unwrap(), b"shared");
}
#[test]
fn failed_writes_preserve_index() {
    let dir = tempdir().unwrap(); let root = dir.path(); let db = db();
    let index = root.join("skills.json");
    for original in ["{", "{\"version\":9}", "{\"version\":1,\"unexpected\":true}"] {
        fs::write(&index, original).unwrap();
        assert!(register(&db, root, None, entry(root, "new", None)).is_err());
        assert_eq!(fs::read_to_string(&index).unwrap(), original);
    }
    fs::remove_file(&index).unwrap();
    assert!(register(&db, root, Some("other"), entry(root, "new", Some("scope"))).is_err());
    let mut invalid = entry(root, "relative", None); invalid.path = "SKILL.md".into();
    assert!(register(&db, root, None, invalid).is_err());
    assert!(!index.exists());
}
#[test]
fn concurrent_registrations_preserve_both_entries() {
    let dir = tempdir().unwrap(); let root = dir.path();
    std::thread::scope(|threads| {
        for name in ["one", "two"] {
            threads.spawn(move || register(&db(), root, None, entry(root, name, None)).unwrap());
        }
    });
    assert_eq!(find(&db(), root, None, "", None, 0, 4).unwrap().items.len(), 2);
}
