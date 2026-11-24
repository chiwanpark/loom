package com.chiwanpark.loom.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.HorizontalDivider
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.OracleLink
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.core.AgentConnection
import com.chiwanpark.loom.core.Codespace
import com.chiwanpark.loom.core.CodespaceSession
import com.chiwanpark.loom.core.Loom
import com.chiwanpark.loom.core.LoomException
import com.chiwanpark.loom.core.SessionStatus
import com.chiwanpark.loom.core.SessionSummary
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.flow.filter

/** The home screen's sections, in the order they are shown, as in the desktop's Sessions panel. */
private enum class Group(val title: String) {
    NeedsApproval("Needs approval"),
    Working("Working"),
    Completed("Completed"),
    Idle("Idle"),
    Offline("Offline"),
    Archived("Archived"),
}

private fun groupOf(status: SessionStatus): Group = when (status) {
    SessionStatus.WAITING_APPROVAL -> Group.NeedsApproval
    SessionStatus.THINKING, SessionStatus.RESPONDING, SessionStatus.RUNNING_TOOL -> Group.Working
    SessionStatus.COMPLETED -> Group.Completed
    SessionStatus.IDLE -> Group.Idle
}

/** A row of the home screen: a session of a codespace. */
private data class Row(val oracle: OracleLink, val item: CodespaceSession)

private val Row.summary: SessionSummary
    get() = item.session

/** Every session of the codespaces of the paired oracles, grouped by what it is doing. */
@Composable
fun HomeScreen(app: LoomApplication, loom: Loom) {
    val directory = app.directory
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    var menu by remember { mutableStateOf(false) }
    var showArchived by remember { mutableStateOf(false) }
    var renaming by remember { mutableStateOf<Row?>(null) }
    var deleting by remember { mutableStateOf<Row?>(null) }
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
    val manyHosts = directory.hosts.size > 1
    val groups = groupRows(oracles, showArchived)
    val hostColors = directory.hosts.associate { it.name to hostColor(it.color) }

    suspend fun connectionOf(row: Row): AgentConnection {
        val instance = row.item.instance ?: throw LoomException.Offline("The codespace has no agent.")
        return app.links.get(loom, row.oracle.host, instance).awaitConnection()
    }

    fun open(row: Row) {
        row.item.instance?.let { instance ->
            app.navigator.push(Screen.Conversation(row.oracle.host, instance, row.item.session.id))
        }
    }

    Scaffold(
        topBar = {
            LoomTopBar("Loom") {
                IconButton(onClick = { directory.refresh(loom) }) { LoomIcon(LoomIcons.Refresh, "Refresh") }
                IconButton(onClick = { menu = true }) { LoomIcon(LoomIcons.More, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    val close = { menu = false }
                    MenuEntry("Pair with an oracle", LoomIcons.QrCode, close) { app.navigator.push(Screen.Pair()) }
                    MenuEntry("Hosts", LoomIcons.Server, close) { app.navigator.push(Screen.Hosts) }
                    MenuEntry(
                        if (showArchived) "Hide archived sessions" else "Show archived sessions",
                        if (showArchived) LoomIcons.EyeOff else LoomIcons.Eye,
                        close,
                    ) { showArchived = !showArchived }
                    MenuEntry("Font", LoomIcons.ALargeSmall, close) { app.navigator.push(Screen.Font) }
                }
            }
        },
        floatingActionButton = {
            if (oracles.isNotEmpty()) {
                FloatingActionButton(onClick = { choosingWorkspace = true }) { LoomIcon(LoomIcons.Plus, "New") }
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            when {
                directory.hosts.isEmpty() -> Message(
                    "Pair with an oracle to see the sessions of its codespaces here. Run " +
                        "`loom-oracle-cli pair-device` on it, then scan the QR code or paste the link it prints.",
                ) {
                    Button(onClick = { app.navigator.push(Screen.Pair()) }) { Text("Pair with an oracle") }
                }
                else -> LazyColumn(Modifier.fillMaxSize(), contentPadding = PaddingValues(bottom = 88.dp)) {
                    groups.forEach { (group, rows) ->
                        item(key = "group-$group") {
                            Text(
                                "${group.title} (${rows.size})",
                                style = MaterialTheme.typography.titleSmall,
                                color = MaterialTheme.colorScheme.primary,
                                modifier = Modifier.padding(start = 12.dp, end = 12.dp, top = 10.dp, bottom = 2.dp),
                            )
                        }
                        items(rows, key = { row -> rowKey(row) }) { row ->
                            SessionRow(
                                row.item.session,
                                agentName = codespaceLabel(row, manyHosts, hostColors[row.oracle.host]),
                                onOpen = { open(row) },
                                onRename = { renaming = row },
                                onArchive = {
                                    scope.attempt(snackbar) {
                                        connectionOf(row).archiveSession(row.item.session.id, !row.item.session.archived)
                                    }
                                },
                                onDelete = { deleting = row },
                            )
                            HorizontalDivider()
                        }
                    }
                    workspaceItems(
                        app,
                        loom,
                        oracles,
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
    renaming?.let { row ->
        val session = row.summary
        TextInputDialog(
            title = "Rename session",
            initial = session.title,
            label = "Title (empty for the first message)",
            confirm = "Rename",
            onConfirm = { title ->
                scope.attempt(snackbar) {
                    connectionOf(row).renameSession(session.id, title.trim().ifEmpty { null })
                }
            },
            onDismiss = { renaming = null },
        )
    }
    deleting?.let { row ->
        val session = row.summary
        ConfirmDialog(
            title = "Delete “${session.title}”?",
            text = "The session and its conversation are deleted on the agent for good.",
            confirm = "Delete",
            onConfirm = {
                scope.attempt(snackbar) {
                    connectionOf(row).deleteSession(session.id)
                }
            },
            onDismiss = { deleting = null },
        )
    }
}

/** The rows by section, newest sessions first. */
private fun groupRows(oracles: List<OracleLink>, showArchived: Boolean): List<Pair<Group, List<Row>>> {
    val rows = mutableMapOf<Group, MutableList<Row>>()
    for (oracle in oracles) {
        for (item in oracle.lists?.sessions.orEmpty()) {
            val group = if (item.session.archived) {
                if (!showArchived) continue
                Group.Archived
            } else {
                groupOf(item.session.status)
            }
            rows.getOrPut(group) { mutableListOf() }.add(Row(oracle, item))
        }
    }
    return Group.entries.mapNotNull { group ->
        rows[group]?.sortedByDescending { it.summary.createdAt }
            ?.let { group to it }
    }
}

private fun rowKey(row: Row): String = "r/${row.oracle.host}/${row.item.codespace}/${row.item.session.id}"

private fun codespaceLabel(row: Row, withHost: Boolean, hostColor: Color?): AnnotatedString {
    val name = "${row.item.workspace} · ${row.item.branch}"
    if (!withHost) return AnnotatedString(name)
    val text = "$name on ${row.oracle.host}"
    if (hostColor == null) return AnnotatedString(text)
    return buildAnnotatedString {
        append(text)
        addStyle(SpanStyle(color = hostColor), text.length - row.oracle.host.length, text.length)
    }
}

@Composable
private fun SessionRow(
    session: SessionSummary,
    agentName: AnnotatedString,
    onOpen: () -> Unit,
    onRename: () -> Unit,
    onArchive: () -> Unit,
    onDelete: () -> Unit,
) {
    var menu by remember { mutableStateOf(false) }
    CompactListItem(
        modifier = Modifier.clickable(onClick = onOpen),
        leadingContent = { SessionStatusIcon(session.status) },
        headlineContent = {
            Text(
                session.title,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
        },
        supportingContent = {
            Text(
                agentName,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        },
        trailingContent = {
            Box {
                IconButton(onClick = { menu = true }) { LoomIcon(LoomIcons.More, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    val close = { menu = false }
                    MenuEntry("Rename", LoomIcons.Pencil, close, onRename)
                    MenuEntry(
                        if (session.archived) "Unarchive" else "Archive",
                        if (session.archived) LoomIcons.ArchiveRestore else LoomIcons.Archive,
                        close,
                        onArchive,
                    )
                    MenuEntry("Delete", LoomIcons.Trash, close, onDelete)
                }
            }
        },
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
