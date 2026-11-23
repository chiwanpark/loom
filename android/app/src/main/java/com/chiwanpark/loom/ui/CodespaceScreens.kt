package com.chiwanpark.loom.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.OracleLink
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.codespaceTitle
import com.chiwanpark.loom.core.AgentConnection
import com.chiwanpark.loom.core.BaseBranch
import com.chiwanpark.loom.core.Codespace
import com.chiwanpark.loom.core.CodespaceStatus
import com.chiwanpark.loom.core.Loom
import com.chiwanpark.loom.core.LoomException
import com.chiwanpark.loom.core.NewCodespaceForm
import com.chiwanpark.loom.core.NewCodespaceRequest
import com.chiwanpark.loom.core.OracleModels
import com.chiwanpark.loom.core.OracleWorkspace
import com.chiwanpark.loom.core.RepositoryState
import com.chiwanpark.loom.core.closePlan
import com.chiwanpark.loom.userMessage
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch

private fun starting(status: CodespaceStatus) =
    status == CodespaceStatus.PROVISIONING || status == CodespaceStatus.STARTING

private suspend fun codespaceConnection(app: LoomApplication, loom: Loom, oracle: OracleLink, codespace: Codespace): AgentConnection {
    val instance = codespace.instance?.takeIf { codespace.status == CodespaceStatus.READY }
        ?: throw LoomException.Offline("The codespace is ${codespace.state}. Its agent is not ready.")
    return app.links.get(loom, oracle.host, instance).awaitConnection()
}

fun openCodespace(
    app: LoomApplication,
    loom: Loom,
    oracle: OracleLink,
    codespace: Codespace,
    scope: CoroutineScope,
    snackbar: SnackbarHostState,
    fresh: Boolean = false,
    replace: Boolean = false,
) = scope.attempt(snackbar) {
    val instance = codespace.instance ?: throw LoomException.Offline("The codespace has no agent yet.")
    val newest = oracle.lists?.sessions.orEmpty()
        .filter { it.codespace == codespace.id && !it.session.archived }
        .maxByOrNull { it.session.createdAt }
    val session = if (newest != null && !fresh) {
        newest.session.id
    } else {
        codespaceConnection(app, loom, oracle, codespace).createSession(null).id
    }
    val screen = Screen.Conversation(oracle.host, instance, session)
    if (replace) {
        app.navigator.replace(screen)
    } else {
        app.navigator.push(screen)
    }
}

private val WorkspacePadding = Modifier.padding(start = 12.dp, end = 12.dp, top = 10.dp, bottom = 2.dp)

@Composable
fun NewCodespaceChooser(
    oracles: List<OracleLink>,
    withAgents: Boolean,
    onWorkspace: (OracleLink, String) -> Unit,
    onAgent: () -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("New codespace") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                var any = false
                for (oracle in oracles) {
                    for (workspace in oracle.lists?.workspaces.orEmpty()) {
                        any = true
                        CompactListItem(
                            modifier = Modifier.clickable { onWorkspace(oracle, workspace.name) },
                            leadingContent = { LoomIcon(LoomIcons.FolderGit, null, size = 20.dp) },
                            headlineContent = { Text(workspace.name) },
                            supportingContent = {
                                Text(
                                    "${oracle.host} · " + workspace.repositories.joinToString(", ") { it.directory },
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                            },
                        )
                    }
                }
                if (!any) Text("The oracles have no workspaces. Add one with loom-oracle-cli workspace add.")
                if (withAgents) {
                    HorizontalDivider()
                    CompactListItem(
                        modifier = Modifier.clickable(onClick = onAgent),
                        leadingContent = { LoomIcon(LoomIcons.Bot, null, size = 20.dp) },
                        headlineContent = { Text("New session on an agent…") },
                    )
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

fun LazyListScope.workspaceItems(
    app: LoomApplication,
    loom: Loom,
    oracles: List<OracleLink>,
    scope: CoroutineScope,
    snackbar: SnackbarHostState,
    onPullRequest: (OracleLink, Codespace) -> Unit,
    onClose: (OracleLink, Codespace) -> Unit,
) {
    item(key = "workspaces") {
        Text(
            "Workspaces",
            style = MaterialTheme.typography.titleSmall,
            color = MaterialTheme.colorScheme.primary,
            modifier = WorkspacePadding,
        )
    }
    for (oracle in oracles) {
        item(key = "oracle/${oracle.host}") {
            val state = oracle.state
            CompactListItem(
                leadingContent = { LoomIcon(LoomIcons.Cloud, null, size = 20.dp) },
                headlineContent = { Text(oracle.host) },
                supportingContent = {
                    Text(
                        when (state) {
                            LinkState.Connected -> "Oracle"
                            LinkState.Connecting -> "Connecting…"
                            is LinkState.Down -> state.message
                        },
                        color = if (state is LinkState.Down) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                    )
                },
                trailingContent = {
                    when (state) {
                        LinkState.Connecting -> CircularProgressIndicator(Modifier.size(24.dp), strokeWidth = 2.dp)
                        is LinkState.Down -> IconButton(onClick = oracle::retryNow) { LoomIcon(LoomIcons.Refresh, "Retry") }
                        LinkState.Connected -> {}
                    }
                },
            )
        }
        val lists = oracle.lists ?: continue
        if (oracle.state == LinkState.Connected && lists.workspaces.isEmpty()) {
            item(key = "oracle/${oracle.host}/empty") {
                Text(
                    "No workspaces. Add one with loom-oracle-cli workspace add.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(start = 44.dp, end = 12.dp, bottom = 6.dp),
                )
            }
        }
        for (workspace in lists.workspaces) {
            item(key = "workspace/${oracle.host}/${workspace.name}") {
                WorkspaceRow(workspace) { app.navigator.push(Screen.NewCodespace(oracle.host, workspace.name)) }
            }
            for (codespace in lists.codespaces.filter { it.workspace == workspace.name }) {
                item(key = "codespace/${oracle.host}/${codespace.id}") {
                    CodespaceRow(
                        codespace,
                        onOpen = {
                            if (codespace.status == CodespaceStatus.READY) {
                                openCodespace(app, loom, oracle, codespace, scope, snackbar)
                            } else {
                                app.navigator.push(Screen.CodespaceProgress(oracle.host, codespace.id))
                            }
                        },
                        onNewSession = { openCodespace(app, loom, oracle, codespace, scope, snackbar, fresh = true) },
                        onProgress = { app.navigator.push(Screen.CodespaceProgress(oracle.host, codespace.id)) },
                        onPullRequest = { onPullRequest(oracle, codespace) },
                        onClose = { onClose(oracle, codespace) },
                    )
                }
            }
        }
    }
}

@Composable
private fun WorkspaceRow(workspace: OracleWorkspace, onNew: () -> Unit) {
    CompactListItem(
        modifier = Modifier.padding(start = 16.dp),
        leadingContent = { LoomIcon(LoomIcons.FolderGit, null, size = 20.dp) },
        headlineContent = { Text(workspace.name) },
        supportingContent = {
            Text(
                workspace.repositories.joinToString(", ") { it.directory },
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        },
        trailingContent = { IconButton(onClick = onNew) { LoomIcon(LoomIcons.Plus, "New codespace") } },
    )
}

@Composable
private fun CodespaceRow(
    codespace: Codespace,
    onOpen: () -> Unit,
    onNewSession: () -> Unit,
    onProgress: () -> Unit,
    onPullRequest: () -> Unit,
    onClose: () -> Unit,
) {
    var menu by remember { mutableStateOf(false) }
    val ready = codespace.status == CodespaceStatus.READY
    CompactListItem(
        modifier = Modifier.padding(start = 32.dp).clickable(onClick = onOpen),
        leadingContent = {
            when {
                starting(codespace.status) -> SpinningIcon(codespace.state, size = 20.dp, tint = MaterialTheme.colorScheme.primary)
                codespace.status == CodespaceStatus.FAILED ->
                    LoomIcon(LoomIcons.CircleX, codespace.state, size = 20.dp, tint = MaterialTheme.colorScheme.error)
                ready -> LoomIcon(LoomIcons.Container, codespace.state, size = 20.dp, tint = LoomColors.success)
                else -> LoomIcon(LoomIcons.Container, codespace.state, size = 20.dp, tint = MaterialTheme.colorScheme.outline)
            }
        },
        headlineContent = { Text(codespace.branch, maxLines = 1, overflow = TextOverflow.Ellipsis) },
        supportingContent = {
            val sessions = codespace.sessions.toInt()
            Text(
                "${codespace.preset ?: "no preset"} · ${codespace.state} · $sessions session${if (sessions == 1) "" else "s"}",
                color = if (codespace.status == CodespaceStatus.FAILED) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
            )
        },
        trailingContent = {
            Box {
                IconButton(onClick = { menu = true }) { LoomIcon(LoomIcons.More, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    val close = { menu = false }
                    if (ready) {
                        MenuEntry("New session", LoomIcons.Plus, close, onNewSession)
                        MenuEntry("Open pull request", LoomIcons.GitPullRequest, close, onPullRequest)
                    }
                    if (!ready) MenuEntry("Start progress", LoomIcons.ScrollText, close, onProgress)
                    MenuEntry("Close codespace", LoomIcons.Trash, close, onClose)
                }
            }
        },
    )
}

@Composable
fun CodespaceActions(
    app: LoomApplication,
    loom: Loom,
    scope: CoroutineScope,
    snackbar: SnackbarHostState,
    pullRequest: Pair<OracleLink, Codespace>?,
    onPullRequestDone: () -> Unit,
    closing: Pair<OracleLink, Codespace>?,
    onClosingDone: () -> Unit,
) {
    val uri = LocalUriHandler.current
    pullRequest?.let { (oracle, codespace) ->
        var repositories by remember(codespace.id) { mutableStateOf<List<RepositoryState>?>(null) }
        var connection by remember(codespace.id) { mutableStateOf<AgentConnection?>(null) }
        LaunchedEffect(codespace.id) {
            try {
                val conn = codespaceConnection(app, loom, oracle, codespace)
                connection = conn
                repositories = conn.repositories()
            } catch (e: LoomException) {
                snackbar.showSnackbar(e.userMessage())
                onPullRequestDone()
            }
        }
        val suggested = oracle.lists?.sessions.orEmpty()
            .filter { it.codespace == codespace.id }
            .maxByOrNull { it.session.createdAt }
            ?.session?.title.orEmpty()
        PullRequestDialog(
            repositories = repositories,
            initialTitle = suggested,
            onOpen = { title, body, draft ->
                val conn = connection ?: return@PullRequestDialog
                onPullRequestDone()
                scope.attempt(snackbar) {
                    val outcomes = conn.openPullRequests(title, body, draft)
                    snackbar.showSnackbar(outcomes.joinToString("\n") { "${it.directory}: ${it.text}" })
                    outcomes.firstOrNull { it.url != null && !it.failed }?.url?.let { runCatching { uri.openUri(it) } }
                }
            },
            onDismiss = onPullRequestDone,
        )
    }
    val (closeOracle, closeCodespace) = closing ?: (null to null)
    if (closeOracle != null && closeCodespace != null && closeCodespace.status != CodespaceStatus.READY) {
        val oracle = closeOracle
        val codespace = closeCodespace
            ConfirmDialog(
                title = "Close codespace?",
                text = "The codespace ${codespaceTitle(codespace)} is ${codespace.state}, so the app cannot ask its agent " +
                    "for unsaved work. The oracle removes the container and its data.",
                confirm = "Close",
                onConfirm = {
                    scope.attempt(snackbar) {
                        oracle.connection().closeCodespace(codespace.id)
                    }
                },
                onDismiss = onClosingDone,
            )
    } else if (closeOracle != null && closeCodespace != null) {
        val oracle = closeOracle
        val codespace = closeCodespace
        var repositories by remember(codespace.id) { mutableStateOf<List<RepositoryState>?>(null) }
        var connection by remember(codespace.id) { mutableStateOf<AgentConnection?>(null) }
        LaunchedEffect(codespace.id) {
            try {
                val conn = codespaceConnection(app, loom, oracle, codespace)
                connection = conn
                repositories = conn.repositories()
            } catch (e: LoomException) {
                snackbar.showSnackbar("The app cannot read the repositories of the codespace: ${e.userMessage()}")
                onClosingDone()
            }
        }
        CloseCodespaceDialog(
            title = codespaceTitle(codespace),
            repositories = repositories,
            onClose = { push, message ->
                val conn = connection ?: return@CloseCodespaceDialog
                onClosingDone()
                scope.attempt(snackbar) {
                    if (push.isNotEmpty()) {
                        val after = conn.pushRepositories(push, message)
                        val failed = after.filter { it.directory in push && (it.error != null || it.needsChoice) }
                        if (failed.isNotEmpty()) {
                            snackbar.showSnackbar(
                                "The push failed, so the codespace stays open. " +
                                    failed.joinToString("; ") { "${it.directory}: ${it.detail}" },
                            )
                            return@attempt
                        }
                    }
                    oracle.connection().closeCodespace(codespace.id)
                }
            },
            onDismiss = onClosingDone,
        )
    }
}

@Composable
private fun PullRequestDialog(
    repositories: List<RepositoryState>?,
    initialTitle: String,
    onOpen: (String, String, Boolean) -> Unit,
    onDismiss: () -> Unit,
) {
    var title by remember { mutableStateOf(initialTitle) }
    var body by remember { mutableStateOf("") }
    var draft by remember { mutableStateOf(false) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Open pull request") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                if (repositories == null) {
                    LinearProgressIndicator(Modifier.fillMaxWidth())
                } else {
                    for (repository in repositories) {
                        val base = repository.baseBranch ?: "the default branch"
                        Text(
                            repository.pullRequest?.let { "${repository.directory}: ${repository.branch} → $base · $it" }
                                ?: "${repository.directory}: ${repository.branch} → $base · ${repository.ahead} commits",
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                }
                OutlinedTextField(value = title, onValueChange = { title = it }, label = { Text("Title") }, singleLine = true)
                OutlinedTextField(value = body, onValueChange = { body = it }, label = { Text("Body") }, minLines = 3)
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Checkbox(checked = draft, onCheckedChange = { draft = it })
                    Text("Draft")
                }
            }
        },
        confirmButton = {
            TextButton(
                enabled = repositories != null && title.isNotBlank(),
                onClick = { onOpen(title.trim(), body, draft) },
            ) { Text("Open") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

private enum class Choice { Push, Discard }

@Composable
private fun CloseCodespaceDialog(
    title: String,
    repositories: List<RepositoryState>?,
    onClose: (List<String>, String?) -> Unit,
    onDismiss: () -> Unit,
) {
    val choices = remember { mutableStateMapOf<String, Choice>() }
    var message by remember { mutableStateOf("") }
    var error by remember { mutableStateOf<String?>(null) }
    val withWork = repositories.orEmpty().filter { it.needsChoice }
    val needsMessage = withWork.any { it.changedFiles > 0u && choices[it.directory] == Choice.Push }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Close codespace") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    if (repositories != null && withWork.isEmpty()) {
                        "Close the codespace $title. No repository has unsaved work. The oracle removes the container and its data."
                    } else {
                        "Close the codespace $title. The oracle removes the container and its data. " +
                            "For each repository with unsaved work, push the branch or discard the work."
                    },
                )
                if (repositories == null) LinearProgressIndicator(Modifier.fillMaxWidth())
                for (repository in withWork) {
                    Column {
                        Text(repository.directory, style = MaterialTheme.typography.bodyLarge)
                        Text(
                            "${repository.branch} · ${repository.detail}",
                            style = MaterialTheme.typography.bodySmall,
                            color = if (repository.error != null) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            FilterChip(
                                selected = choices[repository.directory] == Choice.Push,
                                enabled = repository.error == null,
                                onClick = { choices[repository.directory] = Choice.Push },
                                label = { Text("Push") },
                            )
                            FilterChip(
                                selected = choices[repository.directory] == Choice.Discard,
                                onClick = { choices[repository.directory] = Choice.Discard },
                                label = { Text("Discard") },
                            )
                        }
                    }
                }
                if (needsMessage) {
                    OutlinedTextField(
                        value = message,
                        onValueChange = { message = it },
                        label = { Text("Commit message") },
                        singleLine = true,
                    )
                }
                error?.let { Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall) }
            }
        },
        confirmButton = {
            TextButton(
                enabled = repositories != null,
                onClick = {
                    val push = choices.filterValues { it == Choice.Push }.keys.toList()
                    val discard = choices.filterValues { it == Choice.Discard }.keys.toList()
                    val text = message.trim().ifEmpty { null }
                    try {
                        val planned = closePlan(repositories.orEmpty(), push, discard, text)
                        onClose(planned, text)
                    } catch (e: LoomException) {
                        error = e.userMessage()
                    }
                },
            ) { Text("Close codespace", color = MaterialTheme.colorScheme.error) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
fun NewCodespaceScreen(app: LoomApplication, oracle: OracleLink, workspace: String) {
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    var form by remember { mutableStateOf<NewCodespaceForm?>(null) }
    var preset by remember { mutableStateOf<String?>(null) }
    var cpus by remember { mutableStateOf("") }
    var memory by remember { mutableStateOf("") }
    val branches = remember { mutableStateMapOf<String, String>() }
    var message by remember { mutableStateOf("") }
    var model by remember { mutableStateOf<String?>(null) }
    var models by remember { mutableStateOf<OracleModels?>(null) }
    val warnings = remember { mutableStateListOf<String>() }
    var creating by remember { mutableStateOf(false) }
    var presetMenu by remember { mutableStateOf(false) }
    var modelMenu by remember { mutableStateOf(false) }
    val connected = oracle.state == LinkState.Connected

    LaunchedEffect(connected) {
        if (!connected || form != null) return@LaunchedEffect
        try {
            val loaded = oracle.connection().newCodespaceForm(workspace)
            form = loaded
            preset = loaded.preset
            cpus = loaded.cpus.toString()
            memory = loaded.memory
        } catch (e: LoomException) {
            snackbar.showSnackbar(e.userMessage())
        }
    }
    LaunchedEffect(form, preset) {
        if (form == null) return@LaunchedEffect
        warnings.clear()
        models = null
        model = null
        try {
            warnings.addAll(oracle.connection().codespaceWarnings(workspace, preset))
            val chosen = preset ?: return@LaunchedEffect
            val loaded = oracle.connection().models(chosen)
            models = loaded
            model = loaded.defaultModel
        } catch (e: LoomException) {
            snackbar.showSnackbar(e.userMessage())
        }
    }

    fun create() {
        val loaded = form ?: return
        val count = cpus.trim().toUIntOrNull()
        if (count == null || count == 0u) {
            scope.launch { snackbar.showSnackbar("“$cpus” is not a number of CPUs.") }
            return
        }
        creating = true
        scope.launch {
            try {
                val created = oracle.connection().createCodespace(
                    NewCodespaceRequest(
                        workspace = loaded.workspace,
                        preset = preset,
                        cpus = count,
                        memory = memory,
                        baseBranches = loaded.repositories.map { BaseBranch(it.directory, branches[it.directory].orEmpty()) },
                        firstMessage = message.trim().ifEmpty { null },
                        model = model,
                    ),
                )
                for (warning in created.warnings) snackbar.showSnackbar("Warning: $warning.")
                app.navigator.replace(Screen.CodespaceProgress(oracle.host, created.codespace.id))
            } catch (e: LoomException) {
                snackbar.showSnackbar(e.userMessage())
            } finally {
                creating = false
            }
        }
    }

    Scaffold(
        topBar = { LoomTopBar("New codespace", subtitle = "$workspace on ${oracle.host}", onBack = { app.navigator.pop() }) },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        val loaded = form
        if (loaded == null) {
            Box(Modifier.fillMaxSize().padding(padding), Alignment.Center) {
                if (oracle.state is LinkState.Down) {
                    Text((oracle.state as LinkState.Down).message, color = MaterialTheme.colorScheme.error)
                } else {
                    CircularProgressIndicator()
                }
            }
            return@Scaffold
        }
        Column(
            Modifier.fillMaxSize().padding(padding).imePadding().verticalScroll(rememberScrollState()).padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(
                "The codespace runs on the host ${loaded.host}" + (loaded.hostDetail?.let { " ($it)" } ?: "") + ".",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Text("Preset", style = MaterialTheme.typography.titleSmall)
            Box {
                OutlinedButton(onClick = { presetMenu = true }) { Text(preset ?: "No preset") }
                DropdownMenu(expanded = presetMenu, onDismissRequest = { presetMenu = false }) {
                    for (choice in loaded.presets) {
                        MenuEntry(choice.name, if (choice.name == preset) LoomIcons.Check else LoomIcons.Circle, { presetMenu = false }) {
                            preset = choice.name
                        }
                    }
                    MenuEntry("No preset", if (preset == null) LoomIcons.Check else LoomIcons.Circle, { presetMenu = false }) {
                        preset = null
                    }
                }
            }
            loaded.presets.firstOrNull { it.name == preset }?.let { chosen ->
                Text(
                    if (chosen.modelAccounts.isEmpty()) {
                        "The preset names no model accounts."
                    } else {
                        "Model accounts: ${chosen.modelAccounts.joinToString(", ")}."
                    },
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            for (warning in warnings) {
                Text("Warning: $warning.", style = MaterialTheme.typography.bodySmall, color = LoomColors.warning)
            }
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                OutlinedTextField(
                    value = cpus,
                    onValueChange = { cpus = it },
                    label = { Text("CPUs") },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    modifier = Modifier.weight(1f),
                )
                OutlinedTextField(
                    value = memory,
                    onValueChange = { memory = it },
                    label = { Text("Memory") },
                    singleLine = true,
                    modifier = Modifier.weight(1f),
                )
            }
            Text("Base branches", style = MaterialTheme.typography.titleSmall)
            for (repository in loaded.repositories) {
                OutlinedTextField(
                    value = branches[repository.directory].orEmpty(),
                    onValueChange = { branches[repository.directory] = it },
                    label = { Text(repository.directory) },
                    placeholder = { Text(repository.baseBranch ?: "the default branch") },
                    singleLine = true,
                    textStyle = MaterialTheme.typography.bodyLarge.copy(fontFamily = FontFamily.Monospace),
                    modifier = Modifier.fillMaxWidth(),
                )
            }
            HorizontalDivider()
            OutlinedTextField(
                value = message,
                onValueChange = { message = it },
                label = { Text("First message (optional)") },
                minLines = 3,
                modifier = Modifier.fillMaxWidth(),
            )
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("Model", style = MaterialTheme.typography.bodyMedium)
                Box {
                    val label = models?.models?.firstOrNull { it.id == model }?.label ?: model ?: "Default model of the agent"
                    OutlinedButton(onClick = { modelMenu = true }) {
                        Text(label, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    }
                    DropdownMenu(expanded = modelMenu, onDismissRequest = { modelMenu = false }) {
                        for (choice in models?.models.orEmpty()) {
                            MenuEntry(choice.label, if (choice.id == model) LoomIcons.Check else LoomIcons.Circle, { modelMenu = false }) {
                                model = choice.id
                            }
                        }
                        MenuEntry(
                            "Default model of the agent",
                            if (model == null) LoomIcons.Check else LoomIcons.Circle,
                            { modelMenu = false },
                        ) { model = null }
                    }
                }
            }
            for (error in models?.errors.orEmpty()) {
                Text("Models: $error", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                Button(enabled = !creating && connected, onClick = ::create) {
                    if (creating) {
                        CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                        Box(Modifier.width(8.dp))
                    }
                    Text("Create")
                }
            }
        }
    }
}

@Composable
fun CodespaceProgressScreen(app: LoomApplication, loom: Loom, oracle: OracleLink, id: String) {
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    val codespace = oracle.codespace(id)
    val steps = remember(oracle.lists) { runCatching { oracle.connection().progress(id) }.getOrDefault(emptyList()) }
    Scaffold(
        topBar = {
            LoomTopBar(
                "Codespace start",
                subtitle = codespace?.let(::codespaceTitle),
                onBack = { app.navigator.pop() },
            )
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        Column(
            Modifier.fillMaxSize().padding(padding).verticalScroll(rememberScrollState()).padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            if (codespace == null) {
                Text("The oracle does not list this codespace.", color = MaterialTheme.colorScheme.onSurfaceVariant)
                return@Column
            }
            val color = when (codespace.status) {
                CodespaceStatus.READY -> LoomColors.success
                CodespaceStatus.FAILED -> MaterialTheme.colorScheme.error
                else -> MaterialTheme.colorScheme.primary
            }
            if (starting(codespace.status)) LinearProgressIndicator(Modifier.fillMaxWidth())
            Text(codespace.state, color = color, style = MaterialTheme.typography.titleMedium)
            Text(
                "${codespace.preset ?: "No preset"} · ${codespace.cpus} CPUs · ${codespace.memory}",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            HorizontalDivider()
            if (steps.isEmpty()) {
                Text(
                    "No steps yet. The oracle sends them while the codespace starts.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            for (step in steps) {
                Text(step, style = MaterialTheme.typography.bodySmall, fontFamily = FontFamily.Monospace)
            }
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                Button(
                    enabled = codespace.status == CodespaceStatus.READY,
                    onClick = { openCodespace(app, loom, oracle, codespace, scope, snackbar, replace = true) },
                ) { Text("Open") }
            }
        }
    }
    LaunchedEffect(Unit) { oracle.notices.collect { scope.launch { snackbar.showSnackbar(it) } } }
}
