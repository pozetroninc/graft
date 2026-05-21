package com.graft.test

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background
                ) {
                    TestScreen()
                }
            }
        }
    }
}

@Composable
fun TestScreen() {
    var results by remember { mutableStateOf("Running tests...") }
    var allPassed by remember { mutableStateOf<Boolean?>(null) }

    LaunchedEffect(Unit) {
        val output = withContext(Dispatchers.IO) {
            try {
                GraftBridge.runTests()
            } catch (e: Exception) {
                "ERROR: ${e.message}\n\n${e.stackTraceToString()}"
            }
        }
        results = output
        allPassed = output.contains("ALL TESTS PASSED")
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(16.dp)
            .verticalScroll(rememberScrollState())
    ) {
        Text(
            text = "Graft VFS Test",
            style = MaterialTheme.typography.headlineMedium,
            modifier = Modifier.padding(bottom = 16.dp)
        )

        val statusColor = when (allPassed) {
            true -> Color(0xFF4CAF50)
            false -> Color(0xFFF44336)
            null -> Color.Gray
        }

        Text(
            text = when (allPassed) {
                true -> "STATUS: ALL PASSED"
                false -> "STATUS: FAILED"
                null -> "STATUS: RUNNING..."
            },
            color = statusColor,
            style = MaterialTheme.typography.titleMedium,
            modifier = Modifier.padding(bottom = 16.dp)
        )

        Text(
            text = results,
            fontFamily = FontFamily.Monospace,
            fontSize = 12.sp,
            lineHeight = 18.sp
        )
    }
}
