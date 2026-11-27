package com.chiwanpark.loom.ui

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.consumeWindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.widthIn
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.PrimaryScrollableTabRow
import androidx.compose.material3.Surface
import androidx.compose.material3.Tab
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveableStateHolder
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.core.SessionStatus
import com.chiwanpark.loom.userMessage
import kotlinx.coroutines.launch

private sealed interface CodespaceTab {
    val key: String

    data class Session(val id: String) : CodespaceTab {
        override val key: String get() = "s:$id"
    }

    data class Terminal(val number: Int) : CodespaceTab {
        override val key: String get() = "t:$number"
    }
}

@Composable
fun CodespaceScreen(app: LoomApplication, link: AgentLink, session: String?, terminal: Int?) {
    val scope = rememberCoroutineScope()
    val states = rememberSaveableStateHolder()
    var adding by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val seen = remember { mutableSetOf<String>() }
    val sessions = link.sessions.filter { !it.archived }.sortedBy { it.createdAt }
    val terminals = link.terminalNumbers
    val listed: List<CodespaceTab> =
        sessions.map { CodespaceTab.Session(it.id) } + terminals.map { CodespaceTab.Terminal(it) }
    val selected: CodespaceTab? = when {
        terminal != null -> CodespaceTab.Terminal(terminal)
        session != null -> CodespaceTab.Session(session)
        else -> listed.firstOrNull()
    }
    val tabs = if (selected is CodespaceTab.Terminal && selected !in listed) listed + selected else listed

    fun show(tab: CodespaceTab?) {
        val screen = when (tab) {
            is CodespaceTab.Session -> Screen.Codespace(link.host, link.instance, session = tab.id)
            is CodespaceTab.Terminal -> Screen.Codespace(link.host, link.instance, terminal = tab.number)
            null -> Screen.Codespace(link.host, link.instance)
        }
        if (app.navigator.current is Screen.Codespace) app.navigator.replace(screen)
    }

    fun newSession() = scope.launch {
        try {
            show(CodespaceTab.Session(link.awaitConnection().createSession(null).id))
        } catch (e: kotlinx.coroutines.CancellationException) {
            throw e
        } catch (e: Exception) {
            error = e.userMessage()
        }
    }

    fun archive(id: String) = scope.launch {
        try {
            link.connection().archiveSession(id, true)
            if (selected == CodespaceTab.Session(id)) {
                show(tabs.firstOrNull { it != CodespaceTab.Session(id) })
            }
        } catch (e: kotlinx.coroutines.CancellationException) {
            throw e
        } catch (e: Exception) {
            error = e.userMessage()
        }
    }

    val ids = sessions.map { it.id }
    LaunchedEffect(selected, ids) {
        val shown = (selected as? CodespaceTab.Session)?.id
        if (shown != null && shown in seen && shown !in ids) show(tabs.firstOrNull())
        seen.addAll(ids)
    }
    DisposableEffect(link) {
        onDispose {
            if (!app.navigator.follows(link.host, link.instance)) {
                for (open in link.sessions) {
                    app.scope.launch { link.closeSession(open.id) }
                }
            }
        }
    }

    Column(Modifier.fillMaxSize()) {
        Surface(color = MaterialTheme.colorScheme.surfaceContainer) {
            Row(Modifier.fillMaxWidth().statusBarsPadding(), verticalAlignment = Alignment.CenterVertically) {
                Box(Modifier.weight(1f)) {
                    if (tabs.isNotEmpty()) {
                        PrimaryScrollableTabRow(
                            selectedTabIndex = tabs.indexOf(selected).coerceAtLeast(0),
                            edgePadding = 0.dp,
                            containerColor = MaterialTheme.colorScheme.surfaceContainer,
                            divider = {},
                        ) {
                            for (tab in tabs) {
                                Tab(
                                    selected = tab == selected,
                                    onClick = { show(tab) },
                                    text = {
                                        TabLabel(link, tab, onClose = {
                                            if (tab is CodespaceTab.Session) archive(tab.id)
                                        })
                                    },
                                )
                            }
                        }
                    } else {
                        Text(
                            codespaceName(link),
                            style = MaterialTheme.typography.titleSmall,
                            modifier = Modifier.padding(horizontal = 16.dp),
                        )
                    }
                }
                Box {
                    IconButton(onClick = { adding = true }) { LoomIcon(LoomIcons.Plus, "New tab") }
                    DropdownMenu(expanded = adding, onDismissRequest = { adding = false }) {
                        val close = { adding = false }
                        MenuEntry("New session", LoomIcons.MessageSquareCode, close) { newSession() }
                        MenuEntry("New terminal", LoomIcons.Terminal, close) { show(CodespaceTab.Terminal(0)) }
                    }
                }
            }
        }
        HorizontalDivider()
        error?.let { message ->
            Text(
                message,
                color = MaterialTheme.colorScheme.error,
                style = MaterialTheme.typography.bodySmall,
                modifier = Modifier.padding(horizontal = 12.dp, vertical = 4.dp),
            )
        }
        Box(Modifier.weight(1f).fillMaxWidth().consumeWindowInsets(WindowInsets.statusBars)) {
            when (selected) {
                is CodespaceTab.Session -> states.SaveableStateProvider(selected.key) {
                    ConversationScreen(
                        app,
                        link,
                        selected.id,
                        onTerminal = { show(CodespaceTab.Terminal(link.defaultTerminal() ?: 0)) },
                        onSession = { id -> show(CodespaceTab.Session(id)) },
                    )
                }
                is CodespaceTab.Terminal -> states.SaveableStateProvider(selected.key) {
                    TerminalScreen(
                        app,
                        link,
                        selected.number,
                        onShow = { number -> show(CodespaceTab.Terminal(number)) },
                        onClosed = { show(tabs.firstOrNull { it != selected }) },
                    )
                }
                null -> CenteredMessage(
                    if (link.state == LinkState.Connected) {
                        "No open sessions. Start one with + above."
                    } else {
                        "Connecting to the codespace…"
                    },
                )
            }
        }
    }
}

private fun codespaceName(link: AgentLink): String = link.workspace?.substringAfterLast('/') ?: "Codespace"

@Composable
private fun TabLabel(link: AgentLink, tab: CodespaceTab, onClose: () -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        when (tab) {
            is CodespaceTab.Session -> {
                val summary = link.sessions.firstOrNull { it.id == tab.id }
                if (summary != null && summary.status != SessionStatus.IDLE) {
                    SessionStatusIcon(summary.status)
                } else {
                    LoomIcon(LoomIcons.MessageSquareCode, null, size = 18.dp)
                }
                Text(
                    summary?.title ?: "Session",
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.widthIn(max = 160.dp).padding(start = 6.dp),
                )
                IconButton(onClick = onClose, modifier = Modifier.size(28.dp)) {
                    LoomIcon(LoomIcons.X, "Archive the session", size = 16.dp)
                }
            }
            is CodespaceTab.Terminal -> {
                LoomIcon(LoomIcons.Terminal, null, size = 18.dp)
                val title = link.terminal(tab.number)?.frame?.title?.takeIf { it.isNotBlank() }
                Text(
                    title ?: if (tab.number == 0) "Terminal" else "Terminal ${tab.number}",
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.widthIn(max = 160.dp).padding(start = 6.dp),
                )
            }
        }
    }
}
