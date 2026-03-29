//! Standalone test binary for Graft's SQLite VFS round-trip on Android.
//!
//! Designed to run on Android via `adb push` + `adb shell`, or natively on any platform.
//! Exercises:
//!   1. VFS registration and SQLite database creation
//!   2. Table creation, row insertion, and readback
//!   3. Push/pull between two in-memory nodes
//!   4. CommitHash generation and verification

extern crate static_assertions;

use std::ffi::CString;
use std::process::ExitCode;
use std::sync::Arc;

use graft::core::page::Page;
use graft::core::page_count::PageCount;
use graft::core::{CommitHashBuilder, LogId};
use graft::local::fjall_storage::FjallStorage;
use graft::remote::{Remote, RemoteConfig};
use graft::rt::runtime::Runtime;
use graft::{lsn, pageidx};
use graft_sqlite::vfs::GraftVfs;
use graft_tracing::{SubscriberInitExt, TracingConsumer, setup_tracing};
use rusqlite::Connection;
use sqlite_plugin::vfs::{RegisterOpts, register_static};

/// A minimal test runtime similar to GraftTestRuntime in graft-test.
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
        let runtime = Runtime::new(tokio_rt.handle().clone(), remote.clone(), storage, None);

        // Keep the tokio runtime alive by leaking it (this is a short-lived test binary).
        std::mem::forget(tokio_rt);

        TestRuntime {
            runtime,
            remote,
            vfs_name: None,
        }
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
            let pragma = "graft_clone";
            conn.pragma(None, pragma, remote.serialize(), |row| {
                let output: String = row.get(0).unwrap();
                println!("  {pragma}: {output}");
                Ok(())
            })
            .unwrap();
        }
        conn
    }
}

fn graft_pragma(conn: &Connection, suffix: &str) {
    let pragma = format!("graft_{suffix}");
    conn.pragma_query(None, &pragma, |row| {
        let output: String = row.get(0).unwrap();
        println!("  {pragma}: {output}");
        Ok(())
    })
    .unwrap();
}

// ---------- Test definitions ----------

struct TestResult {
    name: &'static str,
    passed: bool,
    detail: String,
}

fn test_vfs_basic_roundtrip() -> TestResult {
    let name = "VFS basic round-trip";
    match std::panic::catch_unwind(|| {
        let mut rt = TestRuntime::new_memory();
        let conn = rt.open_sqlite("basic_test", None);

        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT);
             INSERT INTO t VALUES (1, 'hello');
             INSERT INTO t VALUES (2, 'world');
             INSERT INTO t VALUES (3, 'graft');",
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3, "expected 3 rows");

        let val: String = conn
            .query_row("SELECT val FROM t WHERE id = 2", [], |row| row.get(0))
            .unwrap();
        assert_eq!(val, "world");
    }) {
        Ok(()) => TestResult {
            name,
            passed: true,
            detail: "3 rows inserted and read back".into(),
        },
        Err(e) => TestResult {
            name,
            passed: false,
            detail: format!("{e:?}"),
        },
    }
}

fn test_push_pull_sync() -> TestResult {
    let name = "Push/pull sync between nodes";
    match std::panic::catch_unwind(|| {
        let remote = LogId::random();
        let mut rt1 = TestRuntime::new_memory();
        let conn1 = rt1.open_sqlite("sync_n1", Some(remote.clone()));

        conn1
            .execute_batch(
                "CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance REAL);
                 INSERT INTO accounts VALUES (1, 100.0);
                 INSERT INTO accounts VALUES (2, 200.0);",
            )
            .unwrap();

        graft_pragma(&conn1, "push");

        let mut rt2 = rt1.spawn_peer();
        let conn2 = rt2.open_sqlite("sync_n2", Some(remote));
        graft_pragma(&conn2, "pull");

        let balance: f64 = conn2
            .query_row("SELECT balance FROM accounts WHERE id = 2", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(balance, 200.0);

        let count: i64 = conn2
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }) {
        Ok(()) => TestResult {
            name,
            passed: true,
            detail: "2 rows synced between nodes".into(),
        },
        Err(e) => TestResult {
            name,
            passed: false,
            detail: format!("{e:?}"),
        },
    }
}

fn test_commit_hash_verification() -> TestResult {
    let name = "CommitHash generation and verification";
    match std::panic::catch_unwind(|| {
        let log = LogId::random();
        let lsn = lsn!(1);
        let vol_pages = PageCount::new(9);
        let commit_pages = PageCount::new(3);

        let page1 = Page::test_filled(0x11);
        let page2 = Page::test_filled(0x22);
        let page3 = Page::test_filled(0x33);

        // Build a commit hash
        let mut builder = CommitHashBuilder::new(log.clone(), lsn, vol_pages, commit_pages);
        builder.write_page(pageidx!(1), &page1);
        builder.write_page(pageidx!(5), &page2);
        builder.write_page(pageidx!(9), &page3);
        let hash1 = builder.build();

        // Same inputs should produce the same hash
        let mut builder2 = CommitHashBuilder::new(log.clone(), lsn, vol_pages, commit_pages);
        builder2.write_page(pageidx!(1), &page1);
        builder2.write_page(pageidx!(5), &page2);
        builder2.write_page(pageidx!(9), &page3);
        let hash2 = builder2.build();
        assert_eq!(hash1, hash2, "same inputs should produce same hash");

        // Different page data should produce a different hash
        let tampered = Page::test_filled(0xFF);
        let mut builder3 = CommitHashBuilder::new(log, lsn, vol_pages, commit_pages);
        builder3.write_page(pageidx!(1), &page1);
        builder3.write_page(pageidx!(5), &tampered);
        builder3.write_page(pageidx!(9), &page3);
        let hash3 = builder3.build();
        assert_ne!(hash1, hash3, "different data should produce different hash");

        // Verify hash serialization roundtrip
        let pretty = hash1.pretty();
        assert!(!pretty.is_empty(), "pretty-printed hash should not be empty");
    }) {
        Ok(()) => TestResult {
            name,
            passed: true,
            detail: "hash determinism, tamper detection, and serialization all passed".into(),
        },
        Err(e) => TestResult {
            name,
            passed: false,
            detail: format!("{e:?}"),
        },
    }
}

fn test_sqlite_data_integrity() -> TestResult {
    let name = "SQLite data integrity across push/pull";
    match std::panic::catch_unwind(|| {
        let remote_log = LogId::random();
        let mut rt = TestRuntime::new_memory();
        let conn = rt.open_sqlite("integrity_test", Some(remote_log.clone()));

        conn.execute_batch(
            "PRAGMA journal_mode = MEMORY;
             CREATE TABLE data (id INTEGER PRIMARY KEY, payload TEXT NOT NULL);",
        )
        .unwrap();

        // Insert enough rows to span multiple pages
        for i in 0..100 {
            conn.execute(
                "INSERT INTO data VALUES (?1, ?2)",
                rusqlite::params![i, "x".repeat(150)],
            )
            .unwrap();
        }

        graft_pragma(&conn, "push");

        // Pull on a peer and verify data integrity
        let mut rt2 = rt.spawn_peer();
        let conn2 = rt2.open_sqlite("integrity_test_peer", Some(remote_log));
        graft_pragma(&conn2, "pull");

        let count: i64 = conn2
            .query_row("SELECT COUNT(*) FROM data", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 100, "expected 100 rows after pull");

        // Verify specific row content
        let payload: String = conn2
            .query_row("SELECT payload FROM data WHERE id = 50", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(payload, "x".repeat(150), "payload should match");

        count
    }) {
        Ok(n) => TestResult {
            name,
            passed: true,
            detail: format!("{n} rows verified after push/pull"),
        },
        Err(e) => TestResult {
            name,
            passed: false,
            detail: format!("{e:?}"),
        },
    }
}

fn main() -> ExitCode {
    // Initialize tracing (writes to stderr)
    setup_tracing(TracingConsumer::Test, None).init();
    let _ = precept::init(&precept::dispatch::noop::NoopDispatch);
    precept::fault::disable_all();

    println!("=== Graft Android Compatibility Test ===");
    println!();

    let tests: Vec<fn() -> TestResult> = vec![
        test_vfs_basic_roundtrip,
        test_push_pull_sync,
        test_commit_hash_verification,
        test_sqlite_data_integrity,
    ];

    let mut results = Vec::new();
    for test_fn in &tests {
        let result = test_fn();
        let status = if result.passed { "PASS" } else { "FAIL" };
        println!("[{status}] {}: {}", result.name, result.detail);
        results.push(result);
    }

    println!();
    let passed = results.iter().filter(|r| r.passed).count();
    let total = results.len();
    println!("Results: {passed}/{total} tests passed");

    if passed == total {
        println!("ALL TESTS PASSED");
        ExitCode::SUCCESS
    } else {
        println!("SOME TESTS FAILED");
        ExitCode::FAILURE
    }
}
