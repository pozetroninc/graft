import SwiftUI

struct ContentView: View {
    @State private var results: [TestResult] = []
    @State private var overallStatus: String = "Running..."
    @State private var hasRun = false

    var body: some View {
        NavigationView {
            List {
                Section(header: Text("Overall")) {
                    HStack {
                        Text(overallStatus)
                            .font(.headline)
                        Spacer()
                        if hasRun {
                            let allPassed = results.allSatisfy { $0.passed }
                            Image(systemName: allPassed ? "checkmark.circle.fill" : "xmark.circle.fill")
                                .foregroundColor(allPassed ? .green : .red)
                                .font(.title2)
                        }
                    }
                }

                Section(header: Text("Test Results")) {
                    ForEach(results) { result in
                        VStack(alignment: .leading, spacing: 4) {
                            HStack {
                                Image(systemName: result.passed ? "checkmark.circle.fill" : "xmark.circle.fill")
                                    .foregroundColor(result.passed ? .green : .red)
                                Text(result.name)
                                    .font(.body.bold())
                            }
                            Text(result.detail)
                                .font(.caption)
                                .foregroundColor(.secondary)
                        }
                        .padding(.vertical, 2)
                    }
                }
            }
            .navigationTitle("Graft iOS Test")
        }
        .onAppear {
            guard !hasRun else { return }
            hasRun = true
            // Run tests on a background thread to avoid blocking UI
            DispatchQueue.global(qos: .userInitiated).async {
                let testResults = GraftBridge.runSQLiteTest()
                DispatchQueue.main.async {
                    results = testResults
                    let allPassed = testResults.allSatisfy { $0.passed }
                    let passCount = testResults.filter { $0.passed }.count
                    overallStatus = allPassed
                        ? "ALL PASSED (\(passCount)/\(testResults.count))"
                        : "FAILED (\(passCount)/\(testResults.count) passed)"
                }
            }
        }
    }
}
