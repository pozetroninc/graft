//! JNI bridge for the Graft Android test app.
//!
//! Exposes a single JNI function that runs the full VFS round-trip test suite
//! and returns results as a string to Kotlin.

extern crate static_assertions;

use std::ffi::CString;
use std::sync::Arc;

use graft::core::commit_hash::CommitMetadata;
use graft::core::page::Page;
use graft::core::page_count::PageCount;
use graft::core::{CommitHashBuilder, LogId, MerkleInclusionProof, PageIdx};
use graft::local::fjall_storage::FjallStorage;
use graft::remote::{Remote, RemoteConfig};
use graft::rt::runtime::Runtime;
use graft::volume_reader::VolumeRead;
use graft::{lsn, pageidx};
use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::jstring;
use rusqlite::Connection;
use sqlite_plugin::vfs::{RegisterOpts, register_static};

struct TestRuntime {
    runtime: Runtime,
    remote: Arc<Remote>,
    vfs_name: Option<CString>,
}

impl TestRuntime {
    fn new_memory() -> Self {
        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        Self::with_remote(remote)
    }

    fn with_remote(remote: Arc<Remote>) -> Self {
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(tokio_rt.handle().clone(), remote.clone(), storage, None, false);
        std::mem::forget(tokio_rt);
        TestRuntime { runtime, remote, vfs_name: None }
    }

    fn spawn_peer(&self) -> Self {
        Self::with_remote(self.remote.clone())
    }

    fn ensure_vfs(&mut self) -> &str {
        let runtime = &self.runtime;
        let vfs_name = self.vfs_name.get_or_insert_with(|| {
            let mut bytes = [0u8; 16];
            for byte in bytes.iter_mut() {
                *byte = rand::random::<u8>() % 26 + b'a';
            }
            let name = CString::new(bytes.to_vec()).unwrap();
            register_static(
                name.clone(),
                GraftVfs::new(runtime.clone()),
                RegisterOpts { make_default: false },
            )
            .expect("failed to register VFS");
            name
        });
        vfs_name.to_str().unwrap()
    }

    fn open_sqlite(&mut self, dbname: &str, remote: Option<LogId>) -> Connection {
        let vfs = self.ensure_vfs();
        let conn = Connection::open(format!("file:{dbname}?vfs={vfs}")).unwrap();
        if let Some(remote) = remote {
            conn.pragma(None, "graft_clone", remote.serialize(), |_| Ok(())).unwrap();
        }
        conn
    }
}

use graft_sqlite::vfs::GraftVfs;

fn graft_pragma(conn: &Connection, suffix: &str) {
    let pragma = format!("graft_{suffix}");
    conn.pragma_query(None, &pragma, |_| Ok(())).unwrap();
}

fn run_test(name: &str, f: impl FnOnce() -> Result<String, String>) -> (bool, String) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(detail)) => (true, format!("[PASS] {name}: {detail}")),
        Ok(Err(detail)) => (false, format!("[FAIL] {name}: {detail}")),
        Err(e) => (false, format!("[FAIL] {name}: panic: {e:?}")),
    }
}

fn run_all_tests() -> String {
    let mut lines = vec!["=== Graft Android VFS Test ===".to_string(), String::new()];
    let mut passed = 0u32;
    let mut total = 0u32;

    // Test 1: Basic VFS round-trip
    {
        let (ok, line) = run_test("VFS basic round-trip", || {
            let mut rt = TestRuntime::new_memory();
            let conn = rt.open_sqlite("basic", None);
            conn.execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT);
                 INSERT INTO t VALUES (1, 'hello');
                 INSERT INTO t VALUES (2, 'world');
                 INSERT INTO t VALUES (3, 'graft');",
            ).map_err(|e| e.to_string())?;

            let count: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            if count != 3 { return Err(format!("expected 3, got {count}")); }

            let val: String = conn.query_row("SELECT val FROM t WHERE id=2", [], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            if val != "world" { return Err(format!("expected 'world', got '{val}'")); }

            Ok("3 rows inserted and read back".into())
        });
        if ok { passed += 1; }
        total += 1;
        lines.push(line);
    }

    // Test 2: Push/pull sync
    {
        let (ok, line) = run_test("Push/pull sync", || {
            let remote = LogId::random();
            let mut rt1 = TestRuntime::new_memory();
            let conn1 = rt1.open_sqlite("sync1", Some(remote.clone()));
            conn1.execute_batch(
                "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance REAL);
                 INSERT INTO accounts VALUES (1, 100.0);
                 INSERT INTO accounts VALUES (2, 200.0);",
            ).map_err(|e| e.to_string())?;
            graft_pragma(&conn1, "push");

            let mut rt2 = rt1.spawn_peer();
            let conn2 = rt2.open_sqlite("sync2", Some(remote));
            graft_pragma(&conn2, "pull");

            let bal: f64 = conn2.query_row("SELECT balance FROM accounts WHERE id=2", [], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            if bal != 200.0 { return Err(format!("expected 200.0, got {bal}")); }

            Ok("data synced between two nodes".into())
        });
        if ok { passed += 1; }
        total += 1;
        lines.push(line);
    }

    // Test 3: Merkle proof verification
    {
        let (ok, line) = run_test("Merkle proof verification", || {
            let remote = LogId::random();
            let lsn = lsn!(1);
            let vol_pages = PageCount::new(9);
            let commit_pages = PageCount::new(3);

            let page1 = Page::test_filled(0x11);
            let page2 = Page::test_filled(0x22);
            let page3 = Page::test_filled(0x33);

            let mut builder = CommitHashBuilder::new(remote.clone(), lsn, vol_pages, commit_pages);
            builder.write_page(pageidx!(1), &page1);
            builder.write_page(pageidx!(5), &page2);
            builder.write_page(pageidx!(9), &page3);
            let (hash, tree) = builder.build_with_tree();
            let meta = CommitMetadata::new(remote, lsn, vol_pages, commit_pages);

            let proof = tree.proof(&[pageidx!(5)]).map_err(|e| e.to_string())?;
            if !proof.verify(&hash, &meta, &[(pageidx!(5), &page2)]) {
                return Err("clean proof failed".into());
            }

            let tampered = Page::test_filled(0xFF);
            if proof.verify(&hash, &meta, &[(pageidx!(5), &tampered)]) {
                return Err("tampered proof should fail".into());
            }

            let bytes = proof.to_bytes();
            let restored = MerkleInclusionProof::from_bytes(&bytes)
                .map_err(|e| e.to_string())?;
            if !restored.verify(&hash, &meta, &[(pageidx!(5), &page2)]) {
                return Err("deserialized proof failed".into());
            }

            Ok("proof verify + tamper detect + serde roundtrip".into())
        });
        if ok { passed += 1; }
        total += 1;
        lines.push(line);
    }

    // Test 4: SQLite corruption detection via Merkle
    {
        let (ok, line) = run_test("SQLite corruption detection", || {
            let remote_log = LogId::random();
            let mut rt = TestRuntime::new_memory();
            let conn = rt.open_sqlite("corrupt", Some(remote_log));

            conn.execute_batch(
                "PRAGMA journal_mode = MEMORY;
                 CREATE TABLE data (id INTEGER PRIMARY KEY, payload TEXT);",
            ).map_err(|e| e.to_string())?;

            for i in 0..100 {
                conn.execute("INSERT INTO data VALUES (?1, ?2)",
                    rusqlite::params![i, "x".repeat(150)])
                    .map_err(|e| e.to_string())?;
            }
            graft_pragma(&conn, "push");

            let tag = rt.runtime.tag_get("corrupt").unwrap().unwrap();
            let vol_info = rt.runtime.volume_get(&tag).unwrap();
            let snapshot = rt.runtime.volume_snapshot(&tag).unwrap();
            let reader = rt.runtime.volume_reader(tag).unwrap();
            let page_count = snapshot.page_count;

            let mut pages: Vec<(PageIdx, Page)> = Vec::new();
            for i in 1..=page_count.to_u32() {
                let pidx = PageIdx::try_new(i).unwrap();
                let page = reader.read_page(pidx).map_err(|e| e.to_string())?;
                if page != Page::EMPTY { pages.push((pidx, page)); }
            }
            let n = pages.len();

            let mut builder = CommitHashBuilder::new(
                vol_info.remote.clone(), lsn!(1), page_count, PageCount::new(n as u32));
            for (pidx, page) in &pages { builder.write_page(*pidx, page); }
            let (hash, tree) = builder.build_with_tree();
            let meta = CommitMetadata::new(vol_info.remote, lsn!(1), page_count, PageCount::new(n as u32));

            let (fpidx, fpage) = &pages[0];
            let proof = tree.proof(&[*fpidx]).map_err(|e| e.to_string())?;
            if !proof.verify(&hash, &meta, &[(*fpidx, fpage)]) {
                return Err("clean proof failed".into());
            }

            let mut bad = fpage.as_ref().to_vec();
            bad[100] ^= 0xFF;
            let bad_page = Page::try_from(bytes::Bytes::from(bad)).map_err(|e| e.to_string())?;
            if proof.verify(&hash, &meta, &[(*fpidx, &bad_page)]) {
                return Err("corrupt proof should fail".into());
            }

            Ok(format!("{n} pages verified, corruption detected"))
        });
        if ok { passed += 1; }
        total += 1;
        lines.push(line);
    }

    lines.push(String::new());
    lines.push(format!("Results: {passed}/{total} tests passed"));
    if passed == total {
        lines.push("ALL TESTS PASSED".into());
    } else {
        lines.push("SOME TESTS FAILED".into());
    }

    lines.join("\n")
}

/// JNI entry point called from Kotlin: `GraftBridge.runTests()`.
/// Returns a string with the test results.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_graft_test_GraftBridge_runTests(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    // Initialize precept (no-op dispatcher for test)
    let _ = precept::init(&precept::dispatch::noop::NoopDispatch);
    precept::fault::disable_all();

    let result = run_all_tests();

    env.new_string(&result)
        .expect("failed to create Java string")
        .into_raw()
}
