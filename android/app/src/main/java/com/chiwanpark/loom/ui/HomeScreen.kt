package com.chiwanpark.loom.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.HostAgents
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.core.Loom
import com.chiwanpark.loom.core.SessionStatus
import com.chiwanpark.loom.core.SessionSummary
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.flow.filter

/** The home screen's sections, in the order they are shown, as in the desktop's Agents panel. */
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

/** A row of the home screen: a session, or an agent that has none or cannot be reached. */
private sealed interface Row {
    val link: AgentLink

    data class Session(override val link: AgentLink, val session: SessionSummary) : Row

    data class Agent(override val link: AgentLink) : Row
}

/** Every session of every agent on the paired hosts, grouped by what it is doing. */
@Composable
fun HomeScreen(app: LoomApplication, loom: Loom) {
    val directory = app.directory
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    var menu by remember { mutableStateOf(false) }
    var showArchived by remember { mutableStateOf(false) }
    var choosingAgent by remember { mutableStateOf(false) }
    var choosingDirectory by remember { mutableStateOf(false) }
    var worktreeFor by remember { mutableStateOf<AgentLink?>(null) }
    var renaming by remember { mutableStateOf<Row.Session?>(null) }
    var deleting by remember { mutableStateOf<Row.Session?>(null) }

    // Looks for agents when the screen opens and whenever the app comes back.
    LaunchedEffect(Unit) {
        directory.refresh(loom)
        app.foreground.drop(1).filter { it }.collect { directory.refresh(loom) }
    }

    val links = app.links.all
    links.forEach { link ->
        LaunchedEffect(link) { link.notices.collect { snackbar.showSnackbar(it.message) } }
    }
    val manyHosts = directory.hosts.size > 1
    val connected = links.filter { it.state == LinkState.Connected }
    val groups = groupRows(links, showArchived)

    fun newSession(link: AgentLink) = scope.attempt(snackbar) {
        val session = link.connection().createSession(null)
        app.navigator.push(Screen.Conversation(link.host, link.instance, session.id))
    }

    Scaffold(
        topBar = {
            LoomTopBar("Loom") {
                IconButton(onClick = { directory.refresh(loom) }) { LoomIcon(LoomIcons.Refresh, "Refresh") }
                IconButton(onClick = { menu = true }) { LoomIcon(LoomIcons.More, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    val close = { menu = false }
                    MenuEntry("Pair with a host", LoomIcons.QrCode, close) { app.navigator.push(Screen.Pair()) }
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
            if (connected.isNotEmpty()) {
                // It asks where, as the session may go to a directory without an agent yet.
                FloatingActionButton(onClick = { choosingAgent = true }) { LoomIcon(LoomIcons.Plus, "New session") }
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            if (directory.refreshing) LinearProgressIndicator(Modifier.fillMaxWidth())
            when {
                directory.hosts.isEmpty() -> Message(
                    "Pair with a host to see its agents' sessions here. Run `loom-agent pair` on it, " +
                        "then scan the QR code or paste the link it prints.",
                ) {
                    Button(onClick = { app.navigator.push(Screen.Pair()) }) { Text("Pair with a host") }
                }
                groups.isEmpty() && !directory.refreshing -> Message(
                    hostProblems(directory.hosts.map { it.name }, directory.agents)
                        ?: "No agent on your hosts is connected to the relay. Start one with " +
                        "`loom-agent serve`, with the relay enabled in its agent.yaml.",
                ) {
                    Button(onClick = { directory.refresh(loom) }) { Text("Refresh") }
                }
                else -> LazyColumn(Modifier.fillMaxSize(), contentPadding = PaddingValues(bottom = 88.dp)) {
                    hostProblems(directory.hosts.map { it.name }, directory.agents)?.let { problem ->
                        item(key = "problems") {
                            Text(
                                problem,
                                color = MaterialTheme.colorScheme.error,
                                style = MaterialTheme.typography.bodySmall,
                                modifier = Modifier.padding(horizontal = 12.dp, vertical = 4.dp),
                            )
                        }
                    }
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
                            when (row) {
                                is Row.Session -> SessionRow(
                                    row,
                                    agentName = agentName(row.link, manyHosts),
                                    onOpen = {
                                        app.navigator.push(
                                            Screen.Conversation(row.link.host, row.link.instance, row.session.id),
                                        )
                                    },
                                    onRename = { renaming = row },
                                    onArchive = {
                                        scope.attempt(snackbar) {
                                            row.link.connection().archiveSession(row.session.id, !row.session.archived)
                                        }
                                    },
                                    onDelete = { deleting = row },
                                )
                                is Row.Agent -> AgentRow(
                                    row.link,
                                    agentName(row.link, manyHosts),
                                    onNewSession = { newSession(row.link) },
                                )
                            }
                            HorizontalDivider()
                        }
                    }
                }
            }
        }
    }

    if (choosingAgent) {
        NewSessionChooser(
            connected = connected,
            agentName = { agentName(it, manyHosts) },
            onPick = { link ->
                choosingAgent = false
                newSession(link)
            },
            onWorktree = { link ->
                choosingAgent = false
                worktreeFor = link
            },
            onDirectory = {
                choosingAgent = false
                choosingDirectory = true
            },
            onDismiss = { choosingAgent = false },
        )
    }
    worktreeFor?.let { link ->
        WorktreeSessionDialog(
            onCreate = { branch, base ->
                scope.attempt(snackbar) {
                    val session = link.connection()
                        .createWorktreeSession(branch.ifEmpty { null }, base.ifEmpty { null })
                    app.navigator.push(Screen.Conversation(link.host, link.instance, session.id))
                }
            },
            onDismiss = { worktreeFor = null },
        )
    }
    if (choosingDirectory) {
        DirectorySessionDialog(
            app,
            loom,
            connected = connected,
            onStarted = { link, session ->
                choosingDirectory = false
                app.navigator.push(Screen.Conversation(link.host, link.instance, session))
            },
            onDismiss = { choosingDirectory = false },
        )
    }
    renaming?.let { row ->
        TextInputDialog(
            title = "Rename session",
            initial = row.session.title,
            label = "Title (empty for the first message)",
            confirm = "Rename",
            onConfirm = { title ->
                scope.attempt(snackbar) {
                    row.link.connection().renameSession(row.session.id, title.trim().ifEmpty { null })
                }
            },
            onDismiss = { renaming = null },
        )
    }
    deleting?.let { row ->
        val worktree = row.session.worktree
        ConfirmDialog(
            title = "Delete “${row.session.title}”?",
            text = "The session and its conversation are deleted on the agent for good." +
                (worktree?.let { " Its worktree and the branch ${it.branch} are removed too, also changes that were not merged." } ?: ""),
            confirm = "Delete",
            onConfirm = {
                scope.attempt(snackbar) {
                    val connection = row.link.connection()
                    if (worktree != null) connection.removeWorktree(row.session.id, true)
                    connection.deleteSession(row.session.id)
                }
            },
            onDismiss = { deleting = null },
        )
    }
}

/** The rows by section, newest sessions first; agents without sessions sit with the idle ones. */
private fun groupRows(links: List<AgentLink>, showArchived: Boolean): List<Pair<Group, List<Row>>> {
    val rows = mutableMapOf<Group, MutableList<Row>>()
    for (link in links) {
        if (link.state != LinkState.Connected) {
            rows.getOrPut(Group.Offline) { mutableListOf() }.add(Row.Agent(link))
            continue
        }
        val active = link.sessions.filter { !it.archived }
        if (active.isEmpty()) rows.getOrPut(Group.Idle) { mutableListOf() }.add(Row.Agent(link))
        for (session in link.sessions) {
            val group = if (session.archived) {
                if (!showArchived) continue
                Group.Archived
            } else {
                groupOf(session.status)
            }
            rows.getOrPut(group) { mutableListOf() }.add(Row.Session(link, session))
        }
    }
    return Group.entries.mapNotNull { group ->
        rows[group]?.sortedByDescending { (it as? Row.Session)?.session?.createdAt ?: ULong.MAX_VALUE }
            ?.let { group to it }
    }
}

private fun rowKey(row: Row): String = when (row) {
    is Row.Session -> "s/${row.link.host}/${row.link.instance}/${row.session.id}"
    is Row.Agent -> "a/${row.link.host}/${row.link.instance}"
}

/** The agent's workspace name, with its host's when there are several hosts. */
private fun agentName(link: AgentLink, withHost: Boolean): String {
    val name = shortWorkspace(link.workspace) ?: link.instance.take(8)
    return if (withHost) "$name on ${link.host}" else name
}

/** Why some hosts' agents could not be listed, if any could not. */
private fun hostProblems(hosts: List<String>, agents: Map<String, HostAgents>): String? =
    hosts.mapNotNull { host -> (agents[host] as? HostAgents.Failed)?.let { "$host: ${it.message}" } }
        .joinToString("\n")
        .ifEmpty { null }

@Composable
private fun SessionRow(
    row: Row.Session,
    agentName: String,
    onOpen: () -> Unit,
    onRename: () -> Unit,
    onArchive: () -> Unit,
    onDelete: () -> Unit,
) {
    val session = row.session
    var menu by remember { mutableStateOf(false) }
    CompactListItem(
        modifier = Modifier.clickable(onClick = onOpen),
        leadingContent = { SessionStatusIcon(session.status) },
        headlineContent = { Text(session.title, maxLines = 2, overflow = TextOverflow.Ellipsis) },
        supportingContent = {
            Text(
                session.worktree?.let { "$agentName · ${it.branch}" } ?: agentName,
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

/** An agent with no sessions, or one that cannot be reached. */
@Composable
private fun AgentRow(link: AgentLink, name: String, onNewSession: () -> Unit) {
    val state = link.state
    CompactListItem(
        headlineContent = { Text(name) },
        supportingContent = {
            Text(
                when (state) {
                    LinkState.Connected -> "No sessions yet"
                    LinkState.Connecting -> "Connecting…"
                    is LinkState.Down -> state.message
                },
                color = if (state is LinkState.Down) {
                    MaterialTheme.colorScheme.error
                } else {
                    MaterialTheme.colorScheme.onSurfaceVariant
                },
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
        },
        trailingContent = {
            when (state) {
                LinkState.Connected -> IconButton(onClick = onNewSession) { LoomIcon(LoomIcons.Plus, "New session") }
                LinkState.Connecting -> CircularProgressIndicator(Modifier.size(24.dp), strokeWidth = 2.dp)
                is LinkState.Down -> IconButton(onClick = link::retryNow) { LoomIcon(LoomIcons.Refresh, "Retry") }
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
