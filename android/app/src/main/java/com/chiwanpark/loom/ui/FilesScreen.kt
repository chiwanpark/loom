package com.chiwanpark.loom.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.core.EntryKind
import com.chiwanpark.loom.core.FileEntry
import com.chiwanpark.loom.userMessage

/** Something a screen asks the agent for: on its way, there, or why it is not. */
sealed interface Loaded<out T> {
    data object Loading : Loaded<Nothing>

    data class Ready<T>(val value: T) : Loaded<T>

    data class Failed(val message: String) : Loaded<Nothing>
}

/**
 * What `load` gets from the agent, asked for once the link is up and again
 * when `reload` grows. What came before stays shown while it reloads.
 */
@Composable
fun <T> rememberLoaded(link: AgentLink, reload: Int, vararg keys: Any?, load: suspend () -> T): Loaded<T> {
    var state by remember(link, *keys) { mutableStateOf<Loaded<T>>(Loaded.Loading) }
    val connected = link.state == LinkState.Connected
    LaunchedEffect(link, connected, reload, *keys) {
        if (!connected) return@LaunchedEffect
        state = try {
            Loaded.Ready(load())
        } catch (e: kotlinx.coroutines.CancellationException) {
            throw e
        } catch (e: Exception) {
            Loaded.Failed(e.userMessage())
        }
    }
    return state
}

/** A loading indicator, or a message with a retry button, while `loaded` is not ready. */
@Composable
fun <T> LoadedContent(loaded: Loaded<T>, onRetry: () -> Unit, content: @Composable (T) -> Unit) {
    when (loaded) {
        Loaded.Loading -> Box(Modifier.fillMaxSize(), Alignment.Center) { CircularProgressIndicator() }
        is Loaded.Failed -> CenteredMessage(loaded.message, error = true) {
            TextButton(onClick = onRetry) { Text("Retry") }
        }
        is Loaded.Ready -> content(loaded.value)
    }
}

@Composable
fun CenteredMessage(text: String, error: Boolean = false, action: @Composable () -> Unit = {}) {
    Box(Modifier.fillMaxSize().padding(32.dp), Alignment.Center) {
        Column(horizontalAlignment = Alignment.CenterHorizontally) {
            Text(
                text,
                style = MaterialTheme.typography.bodyLarge,
                color = if (error) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
            )
            action()
        }
    }
}

/** A size in bytes the way people read it: 812 B, 4.2 KB, 13 MB. */
fun formatSize(bytes: ULong): String {
    val value = bytes.toDouble()
    return when {
        value < 1024 -> "$bytes B"
        value < 1024 * 1024 -> "%.1f KB".format(value / 1024)
        value < 1024.0 * 1024 * 1024 -> "%.1f MB".format(value / (1024 * 1024))
        else -> "%.1f GB".format(value / (1024.0 * 1024 * 1024))
    }
}

/** The workspace path for a screen's subtitle: the agent's workspace, then `path` in it. */
internal fun pathSubtitle(link: AgentLink, path: String): String? {
    val workspace = link.workspace ?: return path.ifEmpty { null }
    return if (path.isEmpty()) workspace else "${workspace.trimEnd('/')}/$path"
}

/** A directory of the agent's workspace, or of the worktree of the session [root]: its directories, then its files. */
@Composable
fun FilesScreen(app: LoomApplication, link: AgentLink, path: String, root: String?) {
    val snackbar = remember { SnackbarHostState() }
    var reload by remember { mutableIntStateOf(0) }
    val listing = rememberLoaded(link, reload, path, root) { link.connection().listDirectory(path, root) }

    Scaffold(
        topBar = {
            LoomTopBar(
                path.substringAfterLast('/').ifEmpty { shortWorkspace(link.workspace) ?: "Files" },
                subtitle = pathSubtitle(link, path),
                subtitleIsPath = true,
                onBack = { app.navigator.pop() },
            ) {
                IconButton(onClick = { reload++ }) { LoomIcon(LoomIcons.Refresh, "Refresh") }
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            LinkBanner(link.state, onRetry = link::retryNow)
            LoadedContent(listing, onRetry = { reload++ }) { directory ->
                if (directory.entries.isEmpty()) {
                    CenteredMessage("This directory is empty.")
                    return@LoadedContent
                }
                LazyColumn(Modifier.fillMaxSize()) {
                    items(directory.entries, key = { it.name }) { entry ->
                        EntryRow(entry) {
                            when (entry.kind) {
                                EntryKind.DIRECTORY ->
                                    app.navigator.push(Screen.Files(link.host, link.instance, entry.path, root))
                                EntryKind.FILE ->
                                    app.navigator.push(Screen.File(link.host, link.instance, entry.path, root))
                                EntryKind.OTHER -> {}
                            }
                        }
                        HorizontalDivider()
                    }
                    if (directory.truncated) {
                        item(key = "truncated") {
                            Text(
                                "Showing the first ${directory.entries.size} entries.",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                modifier = Modifier.padding(12.dp),
                            )
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun EntryRow(entry: FileEntry, onOpen: () -> Unit) {
    val icon = when (entry.kind) {
        EntryKind.DIRECTORY -> if (entry.symlink) LoomIcons.FolderSymlink else LoomIcons.Folder
        EntryKind.FILE -> if (entry.symlink) LoomIcons.FileSymlink else LoomIcons.File
        EntryKind.OTHER -> LoomIcons.File
    }
    val detail = when (entry.kind) {
        EntryKind.FILE -> formatSize(entry.size)
        EntryKind.DIRECTORY -> null
        EntryKind.OTHER -> "Not a file or directory"
    }
    CompactListItem(
        // What git ignores is dimmed, as on the desktop.
        modifier = Modifier
            .clickable(enabled = entry.kind != EntryKind.OTHER, onClick = onOpen)
            .alpha(if (entry.ignored || entry.kind == EntryKind.OTHER) 0.5f else 1f),
        leadingContent = {
            LoomIcon(
                icon,
                null,
                size = 20.dp,
                tint = if (entry.kind == EntryKind.DIRECTORY) MaterialTheme.colorScheme.primary else LocalContentColor.current,
            )
        },
        headlineContent = { Text(entry.name, maxLines = 1, overflow = TextOverflow.MiddleEllipsis) },
        supportingContent = detail?.let { { Text(it, color = MaterialTheme.colorScheme.onSurfaceVariant) } },
        trailingContent = if (entry.kind == EntryKind.DIRECTORY) {
            { LoomIcon(LoomIcons.ChevronRight, null, size = 18.dp, tint = MaterialTheme.colorScheme.onSurfaceVariant) }
        } else {
            null
        },
    )
}

/** A button that turns line wrapping of code on and off, lit while it is on. */
@Composable
fun WrapButton(app: LoomApplication) {
    val wrap = app.settings.wrapCode
    IconButton(onClick = { app.settings.updateWrapCode(!wrap) }) {
        LoomIcon(
            LoomIcons.WrapText,
            if (wrap) "Don't wrap long lines" else "Wrap long lines",
            tint = if (wrap) MaterialTheme.colorScheme.primary else LocalContentColor.current,
        )
    }
}

/**
 * A file of the agent's workspace, highlighted for its language, to read;
 * images and PDF files show in the media viewer instead.
 */
@Composable
fun FileScreen(app: LoomApplication, link: AgentLink, path: String, root: String?) {
    mediaKindOf(path)?.let { kind ->
        MediaScreen(app, link, path, kind, root)
        return
    }
    val snackbar = remember { SnackbarHostState() }
    var reload by remember { mutableIntStateOf(0) }
    val file = rememberLoaded(link, reload, path, root) { link.connection().readFile(path, root) }
    val colors = SyntaxColors.current

    Scaffold(
        topBar = {
            LoomTopBar(
                path.substringAfterLast('/'),
                subtitle = pathSubtitle(link, path),
                subtitleIsPath = true,
                onBack = { app.navigator.pop() },
            ) {
                WrapButton(app)
                IconButton(onClick = { reload++ }) { LoomIcon(LoomIcons.Refresh, "Reload") }
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            LinkBanner(link.state, onRetry = link::retryNow)
            LoadedContent(file, onRetry = { reload++ }) { view ->
                val notes = buildList {
                    add(view.language ?: "Plain text")
                    add(formatSize(view.size))
                    if (view.truncated) add("only the first ${view.lines.size} lines")
                    if (view.invalidUtf8) add("not all UTF-8")
                }
                Text(
                    notes.joinToString(" · "),
                    style = MaterialTheme.typography.bodySmall,
                    color = if (view.truncated || view.invalidUtf8) {
                        LoomColors.warning
                    } else {
                        MaterialTheme.colorScheme.onSurfaceVariant
                    },
                    modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 2.dp),
                )
                when {
                    view.binary -> CenteredMessage("This is a binary file, which Loom does not show.")
                    view.lines.isEmpty() -> CenteredMessage("This file is empty.")
                    else -> {
                        val digits = view.lines.size.toString().length
                        val widest = remember(view) { view.lines.maxOf { columns(it.text) } }
                        CodeList(
                            count = view.lines.size,
                            widest = widest,
                            gutterColumns = digits,
                            wrap = app.settings.wrapCode,
                            content = view to colors,
                            modifier = Modifier.fillMaxSize(),
                            gutter = { index -> LineNumber(index + 1, digits) },
                        ) { index ->
                            val line = view.lines[index]
                            highlighted(line.text, line.spans, colors)
                        }
                    }
                }
            }
        }
    }
}
