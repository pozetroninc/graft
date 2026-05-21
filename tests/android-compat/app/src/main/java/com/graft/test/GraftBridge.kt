package com.graft.test

/**
 * JNI bridge to the Graft Rust library.
 *
 * The native library (libgraft_android_jni.so) exports a single function
 * that runs the full VFS round-trip test suite and returns results as a string.
 */
object GraftBridge {
    init {
        System.loadLibrary("graft_android_jni")
    }

    /**
     * Run the Graft VFS test suite.
     * Returns a multiline string with pass/fail results for each test.
     */
    external fun runTests(): String
}
