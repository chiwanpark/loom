package com.chiwanpark.loom.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.HostAgents
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.core.Loom
import com.chiwanpark.loom.core.LoomException
import com.chiwanpark.loom.userMessage
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

/** How long a new agent gets to join the relay and take the link. */
private const val START_TIMEOUT_MS = 30_000L

/**
 * Where a new session goes: one of the connected agents, or a directory on
 * one of their hosts, which gets an agent of its own if it has none.
 */
@Composable
fun NewSessionChooser(
    connected: List<AgentLink>,
    agentName: (AgentLink) -> String,
    onPick: (AgentLink) -> Unit,
    onDirectory: () -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("New session") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                for (link in connected) {
                    CompactListItem(
                        modifier = Modifier.clickable { onPick(link) },
                        leadingContent = { LoomIcon(LoomIcons.Bot, null, size = 20.dp) },
                        headlineContent = { Text(agentName(link)) },
                        supportingContent = {
                            link.workspace?.let { Text(it, maxLines = 1, overflow = TextOverflow.StartEllipsis) }
                        },
                    )
                }
                HorizontalDivider()
                CompactListItem(
                    modifier = Modifier.clickable(onClick = onDirectory),
                    leadingContent = { LoomIcon(LoomIcons.FolderPlus, null, size = 20.dp) },
                    headlineContent = { Text("In another directory…") },
                    supportingContent = { Text("Starts an agent there if none runs") },
                )
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/** The parent directory of a path, with a slash at its end to type on from. */
private fun parentOf(path: String): String? {
    val trimmed = path.trimEnd('/')
    val parent = trimmed.substringBeforeLast('/', "")
    return if (parent.isEmpty()) null else "$parent/"
}

/**
 * Starts a session in a directory on a host: the host's agent starts an agent
 * there, or finds the one that runs there, and the session opens once the
 * app is linked to it. The directory is typed, with the directories of
 * earlier sessions and those next to the running agents to pick from.
 */
@Composable
fun DirectorySessionDialog(
    app: LoomApplication,
    loom: Loom,
    connected: List<AgentLink>,
    onStarted: (AgentLink, String) -> Unit,
    onDismiss: () -> Unit,
) {
    val hosts = connected.map { it.host }.distinct()
    var host by remember { mutableStateOf(hosts.firstOrNull()) }
    var path by remember { mutableStateOf("") }
    var step by remember { mutableStateOf<String?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    var job by remember { mutableStateOf<Job?>(null) }
    val scope = rememberCoroutineScope()

    val current = host ?: return
    val recent = app.settings.recentDirectories(current)
    val running = (app.directory.agents[current] as? HostAgents.Loaded)?.agents.orEmpty().mapNotNull { it.workspace }
    val nearby = running.mapNotNull(::parentOf).distinct().filter { it !in recent }
    val busy = step != null

    fun start() {
        val via = connected.firstOrNull { it.host == current && it.state == LinkState.Connected } ?: run {
            error = "No agent on $current is connected."
            return
        }
        val directory = path.trim()
        error = null
        job = scope.launch {
            try {
                step = "Starting an agent in $directory…"
                val agent = via.connection().spawnAgent(directory)
                val workspace = agent.workspace ?: directory
                app.settings.rememberDirectory(current, workspace)
                // A new agent joins the relay as it starts; until it has, the app cannot reach it.
                step = "Waiting for the agent to join the relay…"
                val listed = withTimeoutOrNull(START_TIMEOUT_MS) {
                    while (loom.listAgents(current).none { it.instance == agent.instance }) delay(1_000)
                    true
                }
                if (listed == null) throw IllegalStateException("The agent in $workspace did not join the relay.")
                app.directory.refreshHost(loom, current)
                step = "Connecting to the agent…"
                val link = app.links.get(loom, current, agent.instance)
                val linked = withTimeoutOrNull(START_TIMEOUT_MS) {
                    snapshotFlow { link.state }.first { state ->
                        // A link that gave up waits for a retry; it is asked for one.
                        if (state is LinkState.Down) link.retryNow()
                        state == LinkState.Connected
                    }
                }
                if (linked == null) {
                    val why = (link.state as? LinkState.Down)?.message
                    throw IllegalStateException(why ?: "Could not connect to the agent in $workspace.")
                }
                step = "Creating the session…"
                val session = link.connection().createSession(null)
                onStarted(link, session.id)
            } catch (e: kotlinx.coroutines.CancellationException) {
                throw e
            } catch (e: LoomException) {
                error = e.userMessage()
            } catch (e: IllegalStateException) {
                error = e.userMessage()
            } finally {
                step = null
                job = null
            }
        }
    }

    AlertDialog(
        onDismissRequest = { if (!busy) onDismiss() },
        title = { Text("New session in a directory") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                if (hosts.size > 1) {
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        for (name in hosts) {
                            FilterChip(
                                selected = name == current,
                                onClick = { host = name },
                                enabled = !busy,
                                label = { Text(name) },
                            )
                        }
                    }
                }
                OutlinedTextField(
                    value = path,
                    onValueChange = {
                        path = it
                        error = null
                    },
                    label = { Text("Directory on $current") },
                    placeholder = { Text("~/projects/app") },
                    singleLine = true,
                    enabled = !busy,
                    isError = error != null,
                    supportingText = {
                        Text(error ?: "~ is your home directory there. The host's agent configuration decides which directories are allowed.")
                    },
                    keyboardOptions = KeyboardOptions(
                        keyboardType = KeyboardType.Uri,
                        autoCorrectEnabled = false,
                        imeAction = ImeAction.Go,
                    ),
                    keyboardActions = androidx.compose.foundation.text.KeyboardActions(
                        onGo = { if (path.isNotBlank() && !busy) start() },
                    ),
                    modifier = Modifier.fillMaxWidth(),
                )
                if (busy) {
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
                        Text(step.orEmpty(), style = MaterialTheme.typography.bodyMedium)
                    }
                } else {
                    Suggestions("Recent", recent) { path = it }
                    Suggestions("Next to running agents", nearby) { path = it }
                }
            }
        },
        confirmButton = {
            TextButton(onClick = ::start, enabled = path.isNotBlank() && !busy) { Text("Start") }
        },
        dismissButton = {
            TextButton(onClick = {
                job?.cancel()
                onDismiss()
            }) { Text("Cancel") }
        },
    )
}

@Composable
private fun Suggestions(title: String, paths: List<String>, onPick: (String) -> Unit) {
    if (paths.isEmpty()) return
    Column {
        Text(
            title,
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.primary,
            modifier = Modifier.padding(top = 4.dp, bottom = 2.dp),
        )
        for (path in paths) {
            Text(
                path,
                style = MaterialTheme.typography.bodyMedium,
                fontFamily = LoomFonts.code,
                maxLines = 1,
                overflow = TextOverflow.StartEllipsis,
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = 36.dp)
                    .clickable { onPick(path) }
                    .padding(vertical = 8.dp),
            )
        }
    }
}
