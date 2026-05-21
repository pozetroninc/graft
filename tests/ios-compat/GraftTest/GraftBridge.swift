import Foundation
import SQLite3

/// Bridge to the Rust graft-ext static library.
/// Calls graft_static_init() which registers the "graft" VFS with SQLite.
enum GraftBridge {

    /// Register the Graft VFS with SQLite.
    /// Returns nil on success, or an error description on failure.
    static func registerVFS() -> String? {
        // Set environment variables for graft config before init.
        // RemoteConfig defaults to "memory" via GRAFT_REMOTE__TYPE=memory
        // data_dir needs to be a writable path on iOS
        let tmpDir = NSTemporaryDirectory().appending("graft-test")
        setenv("GRAFT_REMOTE__TYPE", "memory", 1)
        setenv("GRAFT_DATA_DIR", tmpDir, 1)

        // Create the data directory if needed
        try? FileManager.default.createDirectory(
            atPath: tmpDir,
            withIntermediateDirectories: true
        )

        let rc = graft_static_init()
        if rc != 0 {
            return "graft_static_init() returned error code \(rc)"
        }
        return nil
    }

    /// Open a SQLite database using the Graft VFS, run basic operations,
    /// and return the test results.
    static func runSQLiteTest() -> [TestResult] {
        var results: [TestResult] = []

        // Step 1: Register the VFS
        if let err = registerVFS() {
            results.append(TestResult(name: "Register VFS", passed: false, detail: err))
            return results
        }
        results.append(TestResult(name: "Register VFS", passed: true, detail: "graft_static_init() returned 0"))

        // Step 2: Open a database with the graft VFS
        var db: OpaquePointer?
        let dbName = "file:test.db?vfs=graft"
        let openFlags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_URI
        let rc = sqlite3_open_v2(dbName, &db, openFlags, nil)
        if rc != SQLITE_OK {
            let errMsg = db.flatMap { String(cString: sqlite3_errmsg($0)) } ?? "unknown"
            results.append(TestResult(name: "Open DB", passed: false, detail: "rc=\(rc): \(errMsg)"))
            return results
        }
        results.append(TestResult(name: "Open DB", passed: true, detail: "Database opened with graft VFS"))

        defer { sqlite3_close(db) }

        // Step 3: Create a table
        let createSQL = "CREATE TABLE IF NOT EXISTS test_data (id INTEGER PRIMARY KEY, name TEXT, value REAL)"
        if let err = execSQL(db: db!, sql: createSQL) {
            results.append(TestResult(name: "CREATE TABLE", passed: false, detail: err))
            return results
        }
        results.append(TestResult(name: "CREATE TABLE", passed: true, detail: "Table created"))

        // Step 4: Clear any existing data (idempotent re-runs) and insert rows
        _ = execSQL(db: db!, sql: "DELETE FROM test_data")
        let insertStatements = [
            "INSERT INTO test_data (id, name, value) VALUES (1, 'alpha', 1.1)",
            "INSERT INTO test_data (id, name, value) VALUES (2, 'beta', 2.2)",
            "INSERT INTO test_data (id, name, value) VALUES (3, 'gamma', 3.3)",
        ]
        for (i, sql) in insertStatements.enumerated() {
            if let err = execSQL(db: db!, sql: sql) {
                results.append(TestResult(name: "INSERT row \(i+1)", passed: false, detail: err))
                return results
            }
        }
        results.append(TestResult(name: "INSERT 3 rows", passed: true, detail: "Rows inserted"))

        // Step 5: Read back and verify
        let selectSQL = "SELECT id, name, value FROM test_data ORDER BY id"
        var stmt: OpaquePointer?
        let prepRC = sqlite3_prepare_v2(db, selectSQL, -1, &stmt, nil)
        if prepRC != SQLITE_OK {
            let errMsg = String(cString: sqlite3_errmsg(db))
            results.append(TestResult(name: "SELECT prepare", passed: false, detail: "rc=\(prepRC): \(errMsg)"))
            return results
        }
        defer { sqlite3_finalize(stmt) }

        let expected: [(Int32, String, Double)] = [
            (1, "alpha", 1.1),
            (2, "beta", 2.2),
            (3, "gamma", 3.3),
        ]

        var rowIndex = 0
        var selectPassed = true
        var selectDetail = ""
        while sqlite3_step(stmt) == SQLITE_ROW {
            let id = sqlite3_column_int(stmt, 0)
            let namePtr = sqlite3_column_text(stmt, 1)
            let name = namePtr.map { String(cString: $0) } ?? ""
            let value = sqlite3_column_double(stmt, 2)

            if rowIndex < expected.count {
                let (eID, eName, eValue) = expected[rowIndex]
                if id != eID || name != eName || abs(value - eValue) > 0.001 {
                    selectPassed = false
                    selectDetail = "Row \(rowIndex): got (\(id), \(name), \(value)), expected (\(eID), \(eName), \(eValue))"
                }
            }
            rowIndex += 1
        }

        if rowIndex != expected.count {
            selectPassed = false
            selectDetail = "Expected \(expected.count) rows, got \(rowIndex)"
        }

        if selectPassed {
            selectDetail = "All \(rowIndex) rows match expected values"
        }
        results.append(TestResult(name: "SELECT verify", passed: selectPassed, detail: selectDetail))

        // Step 6: Verify row count via COUNT(*)
        var countStmt: OpaquePointer?
        let countSQL = "SELECT COUNT(*) FROM test_data"
        if sqlite3_prepare_v2(db, countSQL, -1, &countStmt, nil) == SQLITE_OK {
            if sqlite3_step(countStmt) == SQLITE_ROW {
                let count = sqlite3_column_int(countStmt, 0)
                let countPassed = count == 3
                results.append(TestResult(
                    name: "COUNT(*) verify",
                    passed: countPassed,
                    detail: countPassed ? "COUNT(*) = 3" : "COUNT(*) = \(count), expected 3"
                ))
            }
            sqlite3_finalize(countStmt)
        }

        return results
    }

    private static func execSQL(db: OpaquePointer, sql: String) -> String? {
        var errMsg: UnsafeMutablePointer<CChar>?
        let rc = sqlite3_exec(db, sql, nil, nil, &errMsg)
        if rc != SQLITE_OK {
            let msg = errMsg.map { String(cString: $0) } ?? "unknown error"
            sqlite3_free(errMsg)
            return "rc=\(rc): \(msg)"
        }
        return nil
    }
}

struct TestResult: Identifiable {
    let id = UUID()
    let name: String
    let passed: Bool
    let detail: String
}
