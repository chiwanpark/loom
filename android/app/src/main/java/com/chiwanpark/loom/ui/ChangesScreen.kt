package com.chiwanpark.loom.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.PrimaryTabRow
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Tab
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.core.ChangeKind
import com.chiwanpark.loom.core.DiffKind
import com.chiwanpark.loom.core.DiffView

/**
 * What changed in the agent's workspace, or in the worktree of the session:
 * its uncommitted changes in git, with the files this session changed marked,
 * and the files this session changed with its tool calls, as the desktop's
 * Changes panel shows them.
 */
@Composable
fun ChangesScreen(app: LoomApplication, link: AgentLink, session: String) {
    val snackbar = remember { SnackbarHostState() }
    var reload by remember { mutableIntStateOf(0) }
    val git = rememberLoaded(link, reload, session) { link.connection().gitChanges(session) }
    val touched = rememberLoaded(link, reload, session) { link.connection().sessionChanges(session) }
    // Until the user picks a tab, a workspace without git opens on the session's changes.
    var picked by rememberSaveable { mutableIntStateOf(-1) }
    val gitChanges = (git as? Loaded.Ready)?.value
    val sessionChanges = (touched as? Loaded.Ready)?.value
    val tab = when {
        picked >= 0 -> picked
        gitChanges?.repository == false -> 1
        else -> 0
    }

    Scaffold(
        topBar = {
            LoomTopBar(
                "Changes",
                subtitle = gitChanges?.branch?.let { "On $it" } ?: workspaceSubtitle(link, app.directory.hosts.size > 1),
                subtitleIsPath = gitChanges?.branch == null,
                onBack = { app.navigator.pop() },
            ) {
                IconButton(onClick = { reload++ }) { LoomIcon(LoomIcons.Refresh, "Refresh") }
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            LinkBanner(link.state, onRetry = link::retryNow)
            PrimaryTabRow(selectedTabIndex = tab) {
                Tab(
                    selected = tab == 0,
                    onClick = { picked = 0 },
                    text = { Text(gitChanges?.files?.size?.let { "Uncommitted ($it)" } ?: "Uncommitted") },
                )
                Tab(
                    selected = tab == 1,
                    onClick = { picked = 1 },
                    text = { Text(sessionChanges?.size?.let { "This session ($it)" } ?: "This session") },
                )
            }
            if (tab == 0) {
                LoadedContent(git, onRetry = { reload++ }) { changes ->
                    val mine = sessionChanges.orEmpty().map { it.displayPath }.toSet()
                    when {
                        !changes.repository -> CenteredMessage("The workspace is not a git repository.")
                        changes.files.isEmpty() -> CenteredMessage("No uncommitted changes.")
                        else -> LazyColumn(Modifier.fillMaxSize()) {
                            items(changes.files, key = { it.path }) { change ->
                                ChangeRow(
                                    path = change.path,
                                    detail = change.oldPath?.let { "← $it" },
                                    kind = change.kind,
                                    added = change.added,
                                    removed = change.removed,
                                    binary = change.binary,
                                    bySession = change.path in mine,
                                ) {
                                    app.navigator.push(
                                        Screen.Diff(link.host, link.instance, change.path, change.oldPath, root = session),
                                    )
                                }
                                HorizontalDivider()
                            }
                            if (changes.truncated) {
                                item(key = "truncated") { Truncated("Showing the first ${changes.files.size} files.") }
                            }
                        }
                    }
                }
            } else {
                LoadedContent(touched, onRetry = { reload++ }) { changes ->
                    if (changes.isEmpty()) {
                        CenteredMessage("This session has not changed any files yet.")
                        return@LoadedContent
                    }
                    LazyColumn(Modifier.fillMaxSize()) {
                        items(changes, key = { it.path }) { change ->
                            ChangeRow(
                                path = change.displayPath,
                                detail = null,
                                kind = if (change.created) ChangeKind.ADDED else ChangeKind.MODIFIED,
                                added = change.added,
                                removed = change.removed,
                                binary = false,
                                bySession = false,
                            ) {
                                app.navigator.push(
                                    Screen.Diff(link.host, link.instance, change.path, session = session),
                                )
                            }
                            HorizontalDivider()
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun Truncated(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(12.dp),
    )
}

/** The icon of a kind of change, in its color. */
@Composable
private fun changeIcon(kind: ChangeKind): Pair<Int, Color> = when (kind) {
    ChangeKind.ADDED, ChangeKind.UNTRACKED -> LoomIcons.FilePlus to LoomColors.success
    ChangeKind.DELETED -> LoomIcons.FileMinus to MaterialTheme.colorScheme.error
    ChangeKind.RENAMED, ChangeKind.COPIED -> LoomIcons.FileSymlink to LoomColors.hunk
    ChangeKind.CONFLICTED -> LoomIcons.TriangleAlert to LoomColors.warning
    ChangeKind.MODIFIED, ChangeKind.TYPE_CHANGED -> LoomIcons.FilePen to MaterialTheme.colorScheme.onSurfaceVariant
}

private fun kindLabel(kind: ChangeKind): String = when (kind) {
    ChangeKind.MODIFIED -> "Modified"
    ChangeKind.ADDED -> "Added"
    ChangeKind.DELETED -> "Deleted"
    ChangeKind.RENAMED -> "Renamed"
    ChangeKind.COPIED -> "Copied"
    ChangeKind.TYPE_CHANGED -> "Type changed"
    ChangeKind.UNTRACKED -> "Untracked"
    ChangeKind.CONFLICTED -> "Conflicted"
}

/** Lines added and removed, as `+3 −1` in green and red. */
@Composable
fun LineCounts(added: UInt, removed: UInt) {
    val green = LoomColors.success
    val red = MaterialTheme.colorScheme.error
    Text(
        buildAnnotatedString {
            withStyle(SpanStyle(color = green)) { append("+$added") }
            append(" ")
            withStyle(SpanStyle(color = red)) { append("−$removed") }
        },
        style = MaterialTheme.typography.labelMedium,
        fontFamily = LoomFonts.code,
    )
}

@Composable
private fun ChangeRow(
    path: String,
    detail: String?,
    kind: ChangeKind,
    added: UInt,
    removed: UInt,
    binary: Boolean,
    bySession: Boolean,
    onOpen: () -> Unit,
) {
    val (icon, color) = changeIcon(kind)
    CompactListItem(
        modifier = Modifier.clickable(onClick = onOpen),
        leadingContent = { LoomIcon(icon, kindLabel(kind), size = 20.dp, tint = color) },
        headlineContent = {
            Text(path.substringAfterLast('/'), maxLines = 1, overflow = TextOverflow.MiddleEllipsis)
        },
        supportingContent = {
            val directory = path.substringBeforeLast('/', "")
            Text(
                listOfNotNull(directory.ifEmpty { null }, detail).joinToString("  "),
                maxLines = 1,
                overflow = TextOverflow.StartEllipsis,
            )
        },
        trailingContent = {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp),
                modifier = Modifier.padding(end = 8.dp),
            ) {
                // The session changed this file too.
                if (bySession) {
                    LoomIcon(LoomIcons.Bot, "Changed by this session", size = 16.dp, tint = MaterialTheme.colorScheme.primary)
                }
                if (binary) {
                    Text("binary", style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
                } else {
                    LineCounts(added, removed)
                }
            }
        },
    )
}

/** A file's diff, as the session changed it or as it differs from git's last commit. */
@Composable
fun DiffScreen(
    app: LoomApplication,
    link: AgentLink,
    path: String,
    oldPath: String?,
    session: String?,
    root: String?,
) {
    val snackbar = remember { SnackbarHostState() }
    var reload by remember { mutableIntStateOf(0) }
    val diff = rememberLoaded(link, reload, path, oldPath, session, root) {
        if (session != null) {
            link.connection().sessionDiff(session, path)
        } else {
            link.connection().gitDiff(path, oldPath, root)
        }
    }
    val shown = (diff as? Loaded.Ready)?.value?.path ?: path

    Scaffold(
        topBar = {
            LoomTopBar(
                shown.substringAfterLast('/'),
                subtitle = if (session != null) "Changed by this session: $shown" else shown,
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
            LoadedContent(diff, onRetry = { reload++ }) { view ->
                Row(
                    Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 2.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    LineCounts(view.added, view.removed)
                    val notes = listOfNotNull(
                        view.language,
                        "only its start; the diff is too long".takeIf { view.truncated },
                    )
                    Text(
                        notes.joinToString(" · "),
                        style = MaterialTheme.typography.bodySmall,
                        color = if (view.truncated) LoomColors.warning else MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                when {
                    view.binary -> CenteredMessage("This is a binary file, which Loom does not diff.")
                    view.rows.isEmpty() -> CenteredMessage("No differences.")
                    else -> DiffRows(app, view)
                }
            }
        }
    }
}

@Composable
private fun DiffRows(app: LoomApplication, view: DiffView) {
    val colors = SyntaxColors.current
    val added = LoomColors.success
    val removed = MaterialTheme.colorScheme.error
    val hunk = LoomColors.hunk
    val muted = MaterialTheme.colorScheme.onSurfaceVariant
    val hunkBackground = MaterialTheme.colorScheme.surfaceContainerHigh
    val digits = remember(view) {
        (view.rows.maxOfOrNull { maxOf(it.old ?: 0u, it.new ?: 0u) } ?: 0u).toString().length
    }
    val widest = remember(view) { view.rows.maxOfOrNull { columns(it.text) } ?: 0 }
    CodeList(
        count = view.rows.size,
        widest = widest + 2,
        gutterColumns = digits * 2,
        wrap = app.settings.wrapCode,
        content = Triple(view, colors, added),
        modifier = Modifier.fillMaxSize(),
        background = { index ->
            when (view.rows[index].kind) {
                DiffKind.ADDED -> added.copy(alpha = 0.14f)
                DiffKind.REMOVED -> removed.copy(alpha = 0.14f)
                DiffKind.HUNK -> hunkBackground
                DiffKind.CONTEXT, DiffKind.NOTE -> null
            }
        },
        gutter = { index ->
            val row = view.rows[index]
            Row {
                LineNumber(row.old?.toInt(), digits)
                LineNumber(row.new?.toInt(), digits)
            }
        },
    ) { index ->
        val row = view.rows[index]
        when (row.kind) {
            DiffKind.HUNK -> buildAnnotatedString { withStyle(SpanStyle(color = hunk)) { append(row.text) } }
            DiffKind.NOTE -> buildAnnotatedString { withStyle(SpanStyle(color = muted)) { append(row.text) } }
            else -> buildAnnotatedString {
                val sign = when (row.kind) {
                    DiffKind.ADDED -> "+ "
                    DiffKind.REMOVED -> "- "
                    else -> "  "
                }
                withStyle(SpanStyle(color = if (row.kind == DiffKind.ADDED) added else if (row.kind == DiffKind.REMOVED) removed else muted)) {
                    append(sign)
                }
                append(highlighted(row.text, row.spans, colors))
            }
        }
    }
}
