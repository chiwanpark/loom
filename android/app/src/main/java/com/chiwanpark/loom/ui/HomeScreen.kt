package com.chiwanpark.loom.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.OracleLink
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.core.Codespace
import com.chiwanpark.loom.core.Loom
import com.chiwanpark.loom.core.SessionStatus
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.flow.filter

/** The codespaces of the paired oracles, with what their sessions are doing. */
@Composable
fun HomeScreen(app: LoomApplication, loom: Loom) {
    val directory = app.directory
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    var menu by remember { mutableStateOf(false) }
    var choosingWorkspace by remember { mutableStateOf(false) }
    var pullRequest by remember { mutableStateOf<Pair<OracleLink, Codespace>?>(null) }
    var closing by remember { mutableStateOf<Pair<OracleLink, Codespace>?>(null) }

    // Looks for codespaces when the screen opens and whenever the app comes back.
    LaunchedEffect(Unit) {
        directory.refresh(loom)
        app.foreground.drop(1).filter { it }.collect { directory.refresh(loom) }
    }

    val links = app.links.all
    links.forEach { link ->
        LaunchedEffect(link) { link.notices.collect { snackbar.showSnackbar(it.message) } }
    }
    val oracles = directory.hosts.map { app.oracles.get(loom, it.name) }
    oracles.forEach { oracle ->
        LaunchedEffect(oracle) { oracle.notices.collect { snackbar.showSnackbar(it) } }
    }
    val hostColors = directory.hosts.associate { it.name to hostColor(it.color) }
    val empty = oracles.all { it.lists?.codespaces.isNullOrEmpty() }

    Scaffold(
        topBar = {
            LoomTopBar("Loom") {
                IconButton(onClick = { directory.refresh(loom) }) { LoomIcon(LoomIcons.Refresh, "Refresh") }
                IconButton(onClick = { menu = true }) { LoomIcon(LoomIcons.More, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    val close = { menu = false }
                    MenuEntry("Oracles", LoomIcons.Server, close) { app.navigator.push(Screen.Hosts) }
                    MenuEntry("Pair with an oracle", LoomIcons.QrCode, close) { app.navigator.push(Screen.Pair()) }
                    MenuEntry("Font", LoomIcons.ALargeSmall, close) { app.navigator.push(Screen.Font) }
                }
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
        floatingActionButton = {
            if (oracles.isNotEmpty()) {
                FloatingActionButton(onClick = { choosingWorkspace = true }) { LoomIcon(LoomIcons.Plus, "New codespace") }
            }
        },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            when {
                directory.hosts.isEmpty() -> Message(
                    "Pair with an oracle to see its codespaces here. Make a link " +
                        "on the Devices page of the desktop client (or run `loom-oracle pair` for the first device), then scan the QR code or paste the link.",
                ) {
                    Button(onClick = { app.navigator.push(Screen.Pair()) }) { Text("Pair with an oracle") }
                }
                empty && oracles.any { it.state is LinkState.Down } -> Message(
                    "The client cannot reach the oracles. Open Oracles in the menu to see why.",
                ) {
                    Button(onClick = { app.navigator.push(Screen.Hosts) }) { Text("Oracles") }
                }
                empty -> Message("No codespaces. To start one, tap +.") {
                    Button(onClick = { choosingWorkspace = true }) { Text("New codespace") }
                }
                else -> LazyColumn(Modifier.fillMaxSize(), contentPadding = PaddingValues(bottom = 88.dp)) {
                    codespaceItems(
                        app,
                        loom,
                        oracles,
                        hostColors,
                        scope,
                        snackbar,
                        onPullRequest = { oracle, codespace -> pullRequest = oracle to codespace },
                        onClose = { oracle, codespace -> closing = oracle to codespace },
                    )
                }
            }
        }
    }

    if (choosingWorkspace) {
        NewCodespaceChooser(
            oracles = oracles,
            onWorkspace = { oracle, workspace ->
                choosingWorkspace = false
                app.navigator.push(Screen.NewCodespace(oracle.host, workspace))
            },
            onDismiss = { choosingWorkspace = false },
        )
    }
    CodespaceActions(
        app,
        loom,
        scope,
        snackbar,
        pullRequest = pullRequest,
        onPullRequestDone = { pullRequest = null },
        closing = closing,
        onClosingDone = { closing = null },
    )
}

@Composable
private fun Message(text: String, action: @Composable () -> Unit = {}) {
    Box(Modifier.fillMaxSize().padding(32.dp), Alignment.Center) {
        Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(16.dp)) {
            Text(text, style = MaterialTheme.typography.bodyLarge, color = MaterialTheme.colorScheme.onSurfaceVariant)
            action()
        }
    }
}

fun statusLabel(status: SessionStatus): String = when (status) {
    SessionStatus.IDLE -> "Idle"
    SessionStatus.THINKING -> "Thinking"
    SessionStatus.RESPONDING -> "Responding"
    SessionStatus.RUNNING_TOOL -> "Running a tool"
    SessionStatus.WAITING_APPROVAL -> "Waiting for approval"
    SessionStatus.COMPLETED -> "Completed"
}

/** What a session is doing, as an icon; an idle one has a quiet dot. */
@Composable
fun SessionStatusIcon(status: SessionStatus) {
    val description = statusLabel(status)
    when (status) {
        SessionStatus.IDLE -> LoomIcon(
            LoomIcons.Circle,
            description,
            size = 20.dp,
            tint = MaterialTheme.colorScheme.outlineVariant,
        )
        SessionStatus.THINKING -> LoomIcon(
            LoomIcons.Brain,
            description,
            size = 20.dp,
            tint = MaterialTheme.colorScheme.primary,
        )
        SessionStatus.RESPONDING -> LoomIcon(
            LoomIcons.Pencil,
            description,
            size = 20.dp,
            tint = MaterialTheme.colorScheme.primary,
        )
        SessionStatus.RUNNING_TOOL -> SpinningIcon(description, size = 20.dp, tint = MaterialTheme.colorScheme.primary)
        SessionStatus.WAITING_APPROVAL -> LoomIcon(
            LoomIcons.TriangleAlert,
            description,
            size = 20.dp,
            tint = LoomColors.warning,
        )
        SessionStatus.COMPLETED -> LoomIcon(
            LoomIcons.CircleCheck,
            description,
            size = 20.dp,
            tint = LoomColors.success,
        )
    }
}
