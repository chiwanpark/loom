package com.chiwanpark.loom.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.core.TaskEdit
import com.chiwanpark.loom.core.TaskItem
import com.chiwanpark.loom.core.TaskState
import com.chiwanpark.loom.userMessage

/** The agent's notes: the scratchpad its sessions share. The user can edit them too. */
@Composable
fun NotesScreen(app: LoomApplication, link: AgentLink) {
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    var text by remember { mutableStateOf<String?>(null) }
    var saved by remember { mutableStateOf("") }
    var version by remember { mutableStateOf("") }
    var changedOnAgent by remember { mutableStateOf(false) }
    var reload by remember { mutableIntStateOf(0) }
    val connected = link.state == LinkState.Connected

    LaunchedEffect(connected, reload) {
        if (!connected) return@LaunchedEffect
        try {
            val notes = link.connection().notes()
            text = notes.text
            saved = notes.text
            version = notes.version
            changedOnAgent = false
        } catch (e: Exception) {
            if (e is kotlinx.coroutines.CancellationException) throw e
            snackbar.showSnackbar(e.userMessage())
        }
    }
    LaunchedEffect(link) {
        link.notesChanged.collect { changed ->
            if (changed == version) return@collect
            // Unsaved edits stay; the user picks between them and the agent's.
            if (text == saved) reload++ else changedOnAgent = true
        }
    }

    val dirty = text != null && text != saved
    Scaffold(
        topBar = {
            LoomTopBar(
                "Notes",
                subtitle = workspaceSubtitle(link, manyHosts = app.directory.hosts.size > 1),
                subtitleIsPath = true,
                onBack = { app.navigator.pop() },
            ) {
                TextButton(
                    enabled = dirty && connected,
                    onClick = {
                        val toSave = text ?: return@TextButton
                        scope.attempt(snackbar) {
                            version = link.connection().writeNotes(toSave, version)
                            saved = toSave
                            changedOnAgent = false
                        }
                    },
                ) { Text("Save") }
            }
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding).imePadding()) {
            LinkBanner(link.state, onRetry = link::retryNow)
            if (changedOnAgent) {
                Row(
                    Modifier.fillMaxWidth().padding(horizontal = 12.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        "The notes changed on the agent.",
                        modifier = Modifier.weight(1f),
                        color = MaterialTheme.colorScheme.error,
                    )
                    TextButton(onClick = { reload++ }) { Text("Load theirs") }
                }
            }
            val current = text
            if (current == null) {
                Box(Modifier.fillMaxSize(), Alignment.Center) { CircularProgressIndicator() }
            } else {
                CompactTextField(
                    value = current,
                    onValueChange = { text = it },
                    modifier = Modifier.fillMaxSize().padding(6.dp),
                    textStyle = MaterialTheme.typography.bodyMedium.copy(fontFamily = LoomFonts.code),
                    placeholder = "No notes yet. All sessions of this agent share them.",
                )
            }
        }
    }
}

/** A session's task list: the plan the agent works through. */
@Composable
fun TasksScreen(app: LoomApplication, link: AgentLink, session: String) {
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    var tasks by remember { mutableStateOf<List<TaskItem>?>(null) }
    var adding by remember { mutableStateOf("") }
    var editing by remember { mutableStateOf<TaskItem?>(null) }
    val connected = link.state == LinkState.Connected

    LaunchedEffect(connected) {
        if (!connected) return@LaunchedEffect
        try {
            tasks = link.connection().tasks(session)
        } catch (e: Exception) {
            if (e is kotlinx.coroutines.CancellationException) throw e
            snackbar.showSnackbar(e.userMessage())
        }
    }
    LaunchedEffect(link) {
        link.tasksChanged.collect { (changed, list) -> if (changed == session) tasks = list }
    }
    fun update(vararg edits: TaskEdit) = scope.attempt(snackbar) {
        tasks = link.connection().updateTasks(session, edits.toList())
    }

    Scaffold(
        topBar = { LoomTopBar("Tasks", subtitle = link.conversation(session).title, onBack = { app.navigator.pop() }) },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding).imePadding()) {
            LinkBanner(link.state, onRetry = link::retryNow)
            val list = tasks
            Box(Modifier.weight(1f)) {
                when {
                    list == null -> CircularProgressIndicator(Modifier.align(Alignment.Center))
                    list.isEmpty() -> Text(
                        "No tasks yet.",
                        modifier = Modifier.align(Alignment.Center),
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    else -> LazyColumn(Modifier.fillMaxSize()) {
                        items(list, key = { it.id.toLong() }) { task ->
                            TaskRow(
                                task,
                                onCycle = { update(TaskEdit.Update(task.id, null, nextState(task.state))) },
                                onEdit = { editing = task },
                                onRemove = { update(TaskEdit.Remove(task.id)) },
                            )
                            HorizontalDivider()
                        }
                    }
                }
            }
            Row(
                Modifier.fillMaxWidth().padding(start = 6.dp, top = 4.dp, bottom = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(2.dp),
            ) {
                CompactTextField(
                    value = adding,
                    onValueChange = { adding = it },
                    placeholder = "New task",
                    maxLines = 1,
                    modifier = Modifier.weight(1f),
                )
                IconButton(
                    enabled = connected && adding.isNotBlank(),
                    onClick = {
                        update(TaskEdit.Add(adding.trim(), TaskState.CREATED))
                        adding = ""
                    },
                ) { LoomIcon(LoomIcons.Plus, "Add the task") }
            }
        }
    }

    editing?.let { task ->
        TextInputDialog(
            title = "Edit task #${task.id}",
            initial = task.text,
            label = "Task",
            confirm = "Save",
            onConfirm = { text -> if (text.isNotBlank()) update(TaskEdit.Update(task.id, text.trim(), null)) },
            onDismiss = { editing = null },
        )
    }
}

@Composable
private fun TaskRow(task: TaskItem, onCycle: () -> Unit, onEdit: () -> Unit, onRemove: () -> Unit) {
    var menu by remember { mutableStateOf(false) }
    CompactListItem(
        leadingContent = {
            IconButton(onClick = onCycle) {
                LoomIcon(
                    when (task.state) {
                        TaskState.CREATED -> LoomIcons.Circle
                        TaskState.IN_PROGRESS -> LoomIcons.CircleDot
                        TaskState.DONE -> LoomIcons.CircleCheck
                    },
                    taskStateLabel(task.state),
                    tint = when (task.state) {
                        TaskState.IN_PROGRESS -> MaterialTheme.colorScheme.primary
                        TaskState.DONE -> LoomColors.success
                        TaskState.CREATED -> MaterialTheme.colorScheme.onSurfaceVariant
                    },
                )
            }
        },
        overlineContent = { Text("#${task.id}") },
        headlineContent = {
            Text(
                task.text,
                textDecoration = if (task.state == TaskState.DONE) TextDecoration.LineThrough else null,
            )
        },
        trailingContent = {
            Box {
                IconButton(onClick = { menu = true }) { LoomIcon(LoomIcons.More, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    val close = { menu = false }
                    MenuEntry("Edit", LoomIcons.Pencil, close, onEdit)
                    MenuEntry("Remove", LoomIcons.Trash, close, onRemove)
                }
            }
        },
    )
}

private fun nextState(state: TaskState): TaskState = when (state) {
    TaskState.CREATED -> TaskState.IN_PROGRESS
    TaskState.IN_PROGRESS -> TaskState.DONE
    TaskState.DONE -> TaskState.CREATED
}

private fun taskStateLabel(state: TaskState): String = when (state) {
    TaskState.CREATED -> "To do"
    TaskState.IN_PROGRESS -> "In progress"
    TaskState.DONE -> "Done"
}
