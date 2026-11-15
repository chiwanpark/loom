package com.chiwanpark.loom.ui

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.net.Uri
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.PortForwardState
import com.chiwanpark.loom.core.HostPort
import com.chiwanpark.loom.userMessage
import kotlinx.coroutines.delay

/** How often the screen reads the ports again. */
private const val POLL_MS = 3_000L

/** A port to forward as typed: `3000`, `host:3000`, or `[::1]:3000`; the host defaults to `localhost`. */
fun parseForwardTarget(text: String): Pair<String, Int>? {
    val trimmed = text.trim()
    trimmed.toIntOrNull()?.let { port -> return if (port in 1..65535) "localhost" to port else null }
    val host = trimmed.substringBeforeLast(':', "").removePrefix("[").removeSuffix("]")
    val port = trimmed.substringAfterLast(':', "").toIntOrNull() ?: return null
    if (host.isEmpty() || host.any { it.isWhitespace() } || port !in 1..65535) return null
    return host to port
}

/**
 * The ports that programs listen on, on the agent's host, and the phone's
 * ports forwarded to them: a forwarded port listens on the phone's loopback
 * address, so its browser and other apps reach the server through the agent.
 */
@Composable
fun PortsScreen(app: LoomApplication, link: AgentLink) {
    val context = LocalContext.current
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    var reload by remember { mutableIntStateOf(0) }
    var ports by remember(link) { mutableStateOf<Loaded<List<HostPort>>>(Loaded.Loading) }
    var typed by rememberSaveable { mutableStateOf("") }
    val connected = link.state == LinkState.Connected

    LaunchedEffect(link, connected, reload) {
        if (!connected) return@LaunchedEffect
        while (true) {
            ports = try {
                Loaded.Ready(link.connection().listPorts())
            } catch (e: kotlinx.coroutines.CancellationException) {
                throw e
            } catch (e: Exception) {
                Loaded.Failed(e.userMessage())
            }
            link.forwards.forEach { it.poll() }
            delay(POLL_MS)
        }
    }

    fun forward(host: String, port: Int) = scope.attempt(snackbar) { link.forwardPort(host, port) }

    fun forwardTyped() {
        val target = parseForwardTarget(typed)
        if (target == null) {
            scope.attempt(snackbar) { throw IllegalArgumentException("Type a port such as 3000, or host:port.") }
            return
        }
        typed = ""
        forward(target.first, target.second)
    }

    fun open(forward: PortForwardState) {
        val local = forward.localPort ?: return
        runCatching {
            context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse("http://localhost:$local")))
        }
    }

    fun copy(forward: PortForwardState) {
        val local = forward.localPort ?: return
        context.getSystemService(ClipboardManager::class.java)
            ?.setPrimaryClip(ClipData.newPlainText("Address", "localhost:$local"))
    }

    Scaffold(
        topBar = {
            LoomTopBar(
                "Ports",
                subtitle = workspaceSubtitle(link, manyHosts = app.directory.hosts.size > 1),
                subtitleIsPath = true,
                onBack = { app.navigator.pop() },
            ) {
                IconButton(onClick = { reload++ }) { LoomIcon(LoomIcons.Refresh, "Reload") }
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            LinkBanner(link.state, onRetry = link::retryNow)
            LazyColumn(Modifier.fillMaxSize()) {
                item(key = "add") {
                    Row(
                        Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        CompactTextField(
                            value = typed,
                            onValueChange = { typed = it },
                            placeholder = "Port or host:port",
                            maxLines = 1,
                            modifier = Modifier.weight(1f),
                            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri, imeAction = ImeAction.Go),
                            keyboardActions = KeyboardActions(onGo = { forwardTyped() }),
                        )
                        FilledTonalButton(onClick = ::forwardTyped, enabled = connected) { Text("Forward") }
                    }
                }
                item(key = "h/forwarded") { PortsSectionTitle("Forwarded") }
                if (link.forwards.isEmpty()) {
                    item(key = "none") { PortsNote("No port of the phone is forwarded to this agent's host yet.") }
                }
                items(link.forwards.toList(), key = { "f/${it.host}:${it.port}" }) { forward ->
                    ForwardRow(
                        forward,
                        onOpen = { open(forward) },
                        onCopy = { copy(forward) },
                        onStop = { link.stopForward(forward) },
                    )
                }
                item(key = "h/listening") { PortsSectionTitle("Listening on the agent's host") }
                when (val loaded = ports) {
                    Loaded.Loading -> item(key = "loading") { PortsNote("Reading the ports…") }
                    is Loaded.Failed -> item(key = "failed") { PortsNote("Could not list the ports: ${loaded.message}") }
                    is Loaded.Ready -> {
                        if (loaded.value.isEmpty()) {
                            item(key = "empty") { PortsNote("No program of the agent's user listens on a TCP port.") }
                        }
                        items(loaded.value, key = { "p/${it.port}" }) { port ->
                            val forwarded = link.forwards.firstOrNull { f ->
                                f.port == port.port.toInt() && (f.host == port.host || f.host == "localhost")
                            }
                            PortRow(port, forwarded, enabled = connected) { forward(port.host, port.port.toInt()) }
                        }
                    }
                }
                item(key = "note") {
                    PortsNote(
                        "A forwarded port listens on the phone's loopback address, so the browser and other apps " +
                            "reach it as localhost. Forwarding goes on while Loom is not on screen, until you stop it " +
                            "here or in its notification.",
                    )
                }
            }
        }
    }
}

@Composable
private fun PortsSectionTitle(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 12.dp, bottom = 4.dp),
    )
}

@Composable
private fun PortsNote(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 6.dp),
    )
}

@Composable
private fun ForwardRow(forward: PortForwardState, onOpen: () -> Unit, onCopy: () -> Unit, onStop: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().padding(start = 16.dp, end = 4.dp, top = 2.dp, bottom = 2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(
                forward.localPort?.let { "localhost:$it" } ?: "Not listening",
                style = MaterialTheme.typography.bodyLarge,
                fontWeight = FontWeight.SemiBold,
            )
            val detail = buildList {
                add("to ${forward.host}:${forward.port}")
                when (forward.connections) {
                    0 -> {}
                    1 -> add("1 connection")
                    else -> add("${forward.connections} connections")
                }
            }.joinToString(" · ")
            Text(
                forward.error ?: detail,
                style = MaterialTheme.typography.bodySmall,
                color = if (forward.error != null) {
                    MaterialTheme.colorScheme.error
                } else {
                    MaterialTheme.colorScheme.onSurfaceVariant
                },
            )
        }
        IconButton(onClick = onOpen, enabled = forward.localPort != null) {
            LoomIcon(LoomIcons.ExternalLink, "Open in the browser")
        }
        IconButton(onClick = onCopy, enabled = forward.localPort != null) {
            LoomIcon(LoomIcons.Copy, "Copy the address")
        }
        IconButton(onClick = onStop) { LoomIcon(LoomIcons.X, "Stop forwarding") }
    }
}

@Composable
private fun PortRow(port: HostPort, forwarded: PortForwardState?, enabled: Boolean, onForward: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Column(Modifier.weight(1f)) {
            Text(port.port.toString(), style = MaterialTheme.typography.bodyLarge, fontWeight = FontWeight.SemiBold)
            val process = when {
                port.process != null && port.pid != null -> "${port.process} (${port.pid})"
                port.process != null -> port.process
                port.pid != null -> "process ${port.pid}"
                else -> null
            }
            Text(
                listOfNotNull(process, port.address).joinToString(" · "),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        if (forwarded?.localPort != null) {
            Text(
                "on localhost:${forwarded.localPort}",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        } else {
            FilledTonalButton(onClick = onForward, enabled = enabled && forwarded == null) { Text("Forward") }
        }
    }
}
