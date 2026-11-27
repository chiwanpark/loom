package com.chiwanpark.loom.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.core.ArchivedSession
import com.chiwanpark.loom.core.SessionSummary
import com.chiwanpark.loom.userMessage

@Composable
fun ResumeDialog(link: AgentLink, onResumed: (SessionSummary) -> Unit, onDismiss: () -> Unit) {
    var sessions by remember { mutableStateOf<List<ArchivedSession>?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var query by remember { mutableStateOf("") }
    var resuming by remember { mutableStateOf<ArchivedSession?>(null) }
    LaunchedEffect(link) {
        try {
            sessions = link.awaitConnection().listArchivedSessions()
        } catch (e: kotlinx.coroutines.CancellationException) {
            throw e
        } catch (e: Exception) {
            error = e.userMessage()
        }
    }
    resuming?.let { session ->
        LaunchedEffect(session) {
            try {
                onResumed(link.connection().resumeSession(session.id, session.codespace))
                onDismiss()
            } catch (e: kotlinx.coroutines.CancellationException) {
                throw e
            } catch (e: Exception) {
                error = e.userMessage()
                resuming = null
            }
        }
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Resume a session") },
        text = {
            Column {
                CompactTextField(
                    value = query,
                    onValueChange = { query = it },
                    placeholder = "Find an archived session",
                    maxLines = 1,
                    modifier = Modifier.fillMaxWidth().padding(bottom = 8.dp),
                )
                error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
                val all = sessions
                when {
                    all == null -> if (error == null) {
                        Box(Modifier.fillMaxWidth().padding(24.dp), Alignment.Center) { CircularProgressIndicator() }
                    }
                    all.isEmpty() -> Text(
                        "This workspace has no archived sessions.",
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    else -> {
                        val words = query.trim().lowercase().split(Regex("\\s+")).filter { it.isNotEmpty() }
                        val shown = all.filter { session ->
                            val text = "${session.title} ${session.detail}".lowercase()
                            words.all { it in text }
                        }
                        LazyColumn(Modifier.heightIn(max = 420.dp)) {
                            items(shown, key = { "${it.codespace}/${it.id}" }) { session ->
                                CompactListItem(
                                    modifier = Modifier.clickable(enabled = resuming == null) { resuming = session },
                                    leadingContent = {
                                        if (resuming == session) {
                                            CircularProgressIndicator(Modifier.size(20.dp), strokeWidth = 2.dp)
                                        } else {
                                            LoomIcon(LoomIcons.ArchiveRestore, null, size = 20.dp)
                                        }
                                    },
                                    headlineContent = {
                                        Text(session.title, maxLines = 2, overflow = TextOverflow.Ellipsis)
                                    },
                                    supportingContent = {
                                        Text(
                                            session.detail,
                                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                                            maxLines = 1,
                                            overflow = TextOverflow.Ellipsis,
                                        )
                                    },
                                )
                            }
                        }
                    }
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
