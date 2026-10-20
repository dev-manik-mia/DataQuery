use super::*;

fn draft(name: &str, sql: &str, folder: Option<&str>) -> ScriptDraft {
    ScriptDraft { name: name.into(), sql: sql.into(), folder: folder.map(Into::into), ..ScriptDraft::default() }
}

fn files(root: &Path) -> Vec<String> {
    let mut skipped = Vec::new();
    scan(root, &mut skipped).into_values().map(|f| f.rel).collect()
}

fn script<'a>(scripts: &'a [SavedScript], name: &str) -> &'a SavedScript {
    scripts.iter().find(|s| s.name == name).unwrap_or_else(|| panic!("no script {name}"))
}

#[test]
fn sanitizes_names_for_every_platform() {
    assert_eq!(sanitize("paid orders"), "paid orders");
    assert_eq!(sanitize("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
    assert_eq!(sanitize(".."), "untitled");
    assert_eq!(sanitize("../../etc/passwd"), "_.._etc_passwd");
    assert_eq!(sanitize(".hidden"), "hidden");
    assert_eq!(sanitize("trailing. . "), "trailing");
    assert_eq!(sanitize("CON"), "CON_");
    assert_eq!(sanitize("com1.report"), "com1_.report");
    assert_eq!(sanitize("console"), "console");
    assert_eq!(sanitize("tab\there"), "tab_here");
    assert_eq!(sanitize("   "), "untitled");
    assert_eq!(sanitize("Überweisungen 日本"), "Überweisungen 日本");
    assert_eq!(sanitize(&"é".repeat(300)).chars().count(), 100);
    assert_eq!(relative_path("Q1 / Q2", Some("Reports/ Monthly /")), "Reports/Monthly/Q1 _ Q2.sql");
    assert_eq!(relative_path("x", Some("../..")), "untitled/untitled/x.sql");
}

#[test]
fn header_round_trip_and_mapping() {
    assert_eq!(parse("select 1"), (Header::default(), "select 1"));
    assert_eq!(render(&Header::default(), "select 1"), "select 1", "no header without metadata");
    let header =
        Header { tags: vec!["billing".into()], pinned: true, connection: Some("prod".into()), ..Header::default() };
    let text = render(&header, "select 1;\n");
    assert_eq!(text, "-- dataquery: {\"tags\":[\"billing\"],\"pinned\":true,\"connection\":\"prod\"}\nselect 1;\n");
    assert_eq!(parse(&text), (header, "select 1;\n"));
    // A comment that only looks like a header stays SQL.
    assert_eq!(parse("-- dataquery: not json\nselect 1").1, "-- dataquery: not json\nselect 1");
    assert_eq!(parse("-- dataquery: {\"pinned\":true}\r\nselect 1").1, "select 1");

    let store = Store::in_memory().unwrap();
    let saved = store
        .save_script(&ScriptDraft {
            name: "a/b: report".into(),
            sql: "select 2".into(),
            folder: Some("Ops:Daily".into()),
            tags: vec!["x".into()],
            ..ScriptDraft::default()
        })
        .unwrap();
    let rel = relative_path(&saved.name, saved.folder.as_deref());
    assert_eq!(rel, "Ops_Daily/a_b_ report.sql");
    let text = script_file(&saved, None, &rel);
    assert!(text.starts_with("-- dataquery: {\"name\":\"a/b: report\",\"folder\":\"Ops:Daily\",\"tags\":[\"x\"]}\n"));
    let back = draft_from_file(&rel, &text, |_| None);
    assert_eq!(
        (back.name.as_str(), back.folder.as_deref(), back.sql.as_str()),
        ("a/b: report", Some("Ops:Daily"), "select 2")
    );
    // A clash suffix keeps the header name; renaming the file wins over a stale header.
    assert_eq!(draft_from_file("Ops_Daily/a_b_ report (2).sql", &text, |_| None).name, "a/b: report");
    assert_eq!(draft_from_file("Ops_Daily/renamed.sql", &text, |_| None).name, "renamed");
    assert_eq!(draft_from_file("elsewhere/a_b_ report.sql", &text, |_| None).folder.as_deref(), Some("elsewhere"));
    let plain = SavedScript { name: "paid".into(), folder: Some("Reports".into()), tags: Vec::new(), ..saved };
    assert_eq!(script_file(&plain, None, "Reports/paid.sql"), "select 2", "clean names need no header");
}

#[test]
fn two_way_sync_with_moves_deletes_and_new_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let store = Store::in_memory().unwrap();
    let paid = store.save_script(&draft("paid", "select 'paid'", Some("Reports"))).unwrap();
    let refunds = store.save_script(&draft("refunds", "select 'refunds'", None)).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/HEAD.sql"), "ignored").unwrap();
    std::fs::write(root.join("notes.txt"), "ignored").unwrap();
    std::fs::write(root.join("from-git.sql"), "-- dataquery: {\"tags\":[\"team\"]}\nselect 'git'").unwrap();

    let report = reconcile(&store, root, 1).unwrap();
    assert_eq!((report.written, report.imported), (2, 1));
    assert_eq!(files(root), ["from-git.sql", "refunds.sql", "Reports/paid.sql"]);
    assert_eq!(std::fs::read_to_string(root.join("Reports/paid.sql")).unwrap(), "select 'paid'");
    assert_eq!(script(&store.scripts().unwrap(), "from-git").tags, ["team"]);
    let again = reconcile(&store, root, 2).unwrap();
    assert_eq!((again.written, again.imported, again.updated, again.changed_ids.len()), (0, 0, 0, 0), "idempotent");

    // Edited and moved on disk (e.g. `git pull`).
    std::fs::write(root.join("refunds.sql"), "select 'refunds v2'").unwrap();
    std::fs::create_dir(root.join("Archive")).unwrap();
    std::fs::rename(root.join("Reports/paid.sql"), root.join("Archive/paid orders.sql")).unwrap();
    let report = reconcile(&store, root, 3).unwrap();
    assert_eq!(report.updated, 2);
    let scripts = store.scripts().unwrap();
    assert_eq!(script(&scripts, "refunds").sql, "select 'refunds v2'");
    let moved = script(&scripts, "paid orders");
    assert_eq!(
        (moved.id.as_str(), moved.folder.as_deref()),
        (paid.id.as_str(), Some("Archive")),
        "a move keeps the script"
    );

    // Changed in the app: the file follows, also through a rename.
    store.update_script_sql(&refunds.id, "select 'refunds v3'", None).unwrap();
    store
        .save_script(&ScriptDraft { id: Some(paid.id.clone()), ..draft("paid", "select 'paid'", Some("Archive")) })
        .unwrap();
    let report = reconcile(&store, root, 4).unwrap();
    assert_eq!(report.written, 2);
    assert_eq!(std::fs::read_to_string(root.join("refunds.sql")).unwrap(), "select 'refunds v3'");
    assert_eq!(files(root), ["Archive/paid.sql", "from-git.sql", "refunds.sql"]);

    // Deleted on either side.
    std::fs::remove_file(root.join("from-git.sql")).unwrap();
    store.delete_script(&refunds.id).unwrap();
    let report = reconcile(&store, root, 5).unwrap();
    assert_eq!((report.removed, report.deleted_files), (1, 1));
    assert_eq!(files(root), ["Archive/paid.sql"]);
    assert_eq!(store.scripts().unwrap().len(), 1);
}

#[test]
fn conflicts_keep_both_versions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::in_memory().unwrap();
    let s = store.save_script(&draft("daily", "select 1", None)).unwrap();
    reconcile(&store, dir.path(), 1).unwrap();
    std::fs::write(dir.path().join("daily.sql"), "select 'disk'").unwrap();
    store.update_script_sql(&s.id, "select 'app'", None).unwrap();
    let report = reconcile(&store, dir.path(), 2).unwrap();
    assert_eq!(report.conflicts, ["daily (conflict)"]);
    assert_eq!(report.changed_ids, [s.id.as_str()]);
    let scripts = store.scripts().unwrap();
    assert_eq!(script(&scripts, "daily").sql, "select 'disk'");
    assert_eq!(script(&scripts, "daily (conflict)").sql, "select 'app'");
    assert_eq!(std::fs::read_to_string(dir.path().join("daily (conflict).sql")).unwrap(), "select 'app'");
}

#[test]
fn name_clashes_and_unsafe_entries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let store = Store::in_memory().unwrap();
    store.save_script(&draft("Report", "select 1", None)).unwrap();
    store.save_script(&draft("report", "select 2", None)).unwrap();
    store.save_script(&draft("a/b", "select 3", None)).unwrap();
    std::fs::write(root.join("big.sql"), vec![b'x'; 2 * 1024 * 1024]).unwrap();
    std::fs::write(root.join("latin1.sql"), [0xff, 0xfe, 0x00]).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.sql"), "select 'outside'").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), root.join("linked")).unwrap();

    let report = reconcile(&store, root, 1).unwrap();
    let names = files(root);
    assert!(names.contains(&"a_b.sql".to_owned()));
    let lower: HashSet<String> = names.iter().map(|n| n.to_lowercase()).collect();
    assert_eq!(lower.len(), names.len(), "no two files differ only in case: {names:?}");
    assert!(lower.contains("report.sql") && lower.contains("report (2).sql"));
    assert!(report.skipped.iter().any(|s| s.starts_with("big.sql")));
    assert!(report.skipped.iter().any(|s| s.starts_with("latin1.sql")));
    assert!(store.scripts().unwrap().iter().all(|s| !s.sql.contains("outside")), "symlinks are not followed");
    assert_eq!(std::fs::read_to_string(root.join("a_b.sql")).unwrap(), "-- dataquery: {\"name\":\"a/b\"}\nselect 3");
}

#[test]
fn first_run_adopts_identical_files() {
    let dir = tempfile::tempdir().unwrap();
    let a = Store::in_memory().unwrap();
    a.save_script(&draft("paid", "select 1", Some("Reports"))).unwrap();
    reconcile(&a, dir.path(), 1).unwrap();
    let b = Store::in_memory().unwrap();
    b.save_script(&draft("paid", "select 1", Some("Reports"))).unwrap();
    let report = reconcile(&b, dir.path(), 2).unwrap();
    assert_eq!((report.written, report.imported, b.scripts().unwrap().len()), (0, 0, 1));
    assert_eq!(files(dir.path()), ["Reports/paid.sql"]);
}
