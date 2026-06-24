//! Test that proves fcntl-based file locking works across processes on Android.
//!
//! Process 1 opens a fjall database (acquires exclusive lock), then spawns
//! Process 2 which tries to open the same database and should get a Locked error.

use std::env;
use std::process::{Command, ExitCode};

use graft::local::fjall_storage::FjallStorage;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    if args.len() > 1 && args[1] == "--child" {
        // Child process: try to open the same database, should fail with Locked
        let db_path = &args[2];
        eprintln!("[child] Attempting to open locked database at {db_path}");
        match FjallStorage::open(db_path) {
            Ok(_) => {
                eprintln!("[child] ERROR: Opened database without error - lock NOT working!");
                std::process::exit(1);
            }
            Err(e) => {
                let err_str = format!("{e:?}");
                eprintln!("[child] Got expected error: {err_str}");
                if err_str.contains("Locked") || err_str.contains("locked") {
                    eprintln!("[child] PASS: Lock correctly prevented concurrent access");
                    std::process::exit(0);
                } else {
                    eprintln!("[child] FAIL: Got unexpected error (not a lock error)");
                    std::process::exit(2);
                }
            }
        }
    }

    // Parent process
    println!("=== File Lock Cross-Process Test ===");
    println!();

    let tmp_dir = env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let db_path = format!("{tmp_dir}/lock-test-db");

    // Clean up any previous run
    let _ = std::fs::remove_dir_all(&db_path);

    println!("[parent] Opening fjall database at {db_path} (acquires lock)...");
    let _storage = match FjallStorage::open(&db_path) {
        Ok(s) => {
            println!("[parent] Database opened and lock acquired successfully");
            s
        }
        Err(e) => {
            println!("[parent] FAIL: Could not open database: {e:?}");
            return ExitCode::FAILURE;
        }
    };

    // Now spawn child process that tries to open the same database
    let self_exe = env::current_exe().unwrap_or_else(|_| args[0].clone().into());
    println!("[parent] Spawning child process to test lock contention...");

    let output = Command::new(&self_exe)
        .args(["--child", &db_path])
        .output();

    match output {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            print!("{stderr}");

            if out.status.success() {
                println!();
                println!("[parent] PASS: Child confirmed lock works correctly!");
                println!();
                println!("=== Lock Test PASSED ===");
                ExitCode::SUCCESS
            } else {
                let code = out.status.code().unwrap_or(-1);
                println!();
                if code == 1 {
                    println!("[parent] FAIL: Child opened locked DB - lock is NOT working!");
                } else if code == 2 {
                    println!("[parent] FAIL: Child got wrong error type");
                } else {
                    println!("[parent] FAIL: Child exited with code {code}");
                }
                println!();
                println!("=== Lock Test FAILED ===");
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            println!("[parent] FAIL: Could not spawn child process: {e}");
            ExitCode::FAILURE
        }
    }
}
