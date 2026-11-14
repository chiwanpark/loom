package com.chiwanpark.loom.ui

import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SmallFloatingActionButton
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.ConversationState
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.core.Activity
import com.chiwanpark.loom.core.ApprovalDecision
import com.chiwanpark.loom.core.ConversationItem
import com.chiwanpark.loom.core.Model
import com.chiwanpark.loom.core.NoticeLevel
import com.chiwanpark.loom.core.Reasoning
import com.chiwanpark.loom.core.SessionStatus
import com.chiwanpark.loom.core.ToolArguments
import com.chiwanpark.loom.core.ToolState
import com.chiwanpark.loom.userMessage
import com.mikepenz.markdown.m3.Markdown
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.launch

/** A session's conversation: what the agent does, and what the user asks of it. */
@Composable
fun ConversationScreen(app: LoomApplication, link: AgentLink, session: String) {
    val conversation = link.conversation(session)
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    var menu by remember { mutableStateOf(false) }
    var pickModel by remember { mutableStateOf(false) }
    var pickReasoning by remember { mutableStateOf(false) }
    var clearing by remember { mutableStateOf(false) }

    // Follows the conversation while it is on the back stack, also after reconnecting.
    val connected = link.state == LinkState.Connected
    LaunchedEffect(connected) {
        if (connected) {
            try {
                link.openSession(session)
            } catch (e: Exception) {
                if (e is kotlinx.coroutines.CancellationException) throw e
                snackbar.showSnackbar(e.userMessage())
            }
        }
    }
    DisposableEffect(session) {
        onDispose {
            val stillThere = app.navigator.stack.any { it is Screen.Conversation && it.session == session }
            if (!stillThere) app.scope.launch { link.closeSession(session) }
        }
    }
    LaunchedEffect(link) {
        link.notices.collect { snackbar.showSnackbar(it.message) }
    }

    Scaffold(
        topBar = {
            LoomTopBar(
                conversation.title.ifEmpty { "Session" },
                subtitle = listOfNotNull(
                    conversation.model ?: link.agent?.defaultModel,
                    conversation.reasoning?.let { "${reasoningLabel(it).lowercase()} reasoning" },
                    conversation.contextTokens?.let { "${formatTokens(it)} context" },
                ).joinToString(" · ").ifEmpty { null },
                onBack = { app.navigator.pop() },
            ) {
                IconButton(onClick = { app.navigator.push(Screen.Tasks(link.host, link.instance, session)) }) {
                    LoomIcon(LoomIcons.Tasks, "Tasks")
                }
                IconButton(onClick = { app.navigator.push(Screen.Notes(link.host, link.instance, session)) }) {
                    LoomIcon(LoomIcons.Notes, "Notes")
                }
                IconButton(onClick = { menu = true }) { LoomIcon(LoomIcons.More, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    DropdownMenuItem(text = { Text("Model…") }, onClick = {
                        menu = false
                        pickModel = true
                    })
                    DropdownMenuItem(text = { Text("Reasoning effort…") }, onClick = {
                        menu = false
                        pickReasoning = true
                    })
                    DropdownMenuItem(text = { Text("Compact the context") }, onClick = {
                        menu = false
                        scope.attempt(snackbar) { link.connection().compactSession(session) }
                    })
                    DropdownMenuItem(text = { Text("Clear the conversation") }, onClick = {
                        menu = false
                        clearing = true
                    })
                }
            }
        },
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding).imePadding()) {
            LinkBanner(link.state, onRetry = link::retryNow)
            Box(Modifier.weight(1f)) {
                if (!conversation.loaded) {
                    CircularProgressIndicator(Modifier.align(Alignment.Center))
                } else {
                    Messages(
                        conversation,
                        onLoadEarlier = { app.scope.launch { link.loadEarlier(session) } },
                        onDecide = { callId, decision ->
                            scope.attempt(snackbar) { link.connection().resolveApproval(session, callId, decision) }
                        },
                    )
                }
            }
            Composer(
                busy = conversation.status != SessionStatus.IDLE,
                enabled = connected,
                onSend = { text ->
                    scope.attempt(snackbar) { link.connection().sendPrompt(session, text, emptyList()) }
                },
                onStop = { scope.attempt(snackbar) { link.connection().interrupt(session) } },
            )
        }
    }

    if (pickModel) {
        ModelDialog(link, current = conversation.model, onPick = { model ->
            scope.attempt(snackbar) { link.connection().setModel(session, model) }
        }, onDismiss = { pickModel = false })
    }
    if (pickReasoning) {
        ReasoningDialog(current = conversation.reasoning, onPick = { reasoning ->
            scope.attempt(snackbar) { link.connection().setReasoning(session, reasoning) }
        }, onDismiss = { pickReasoning = false })
    }
    if (clearing) {
        ConfirmDialog(
            title = "Clear the conversation?",
            text = "The agent forgets the conversation. The session's notes and tasks stay.",
            confirm = "Clear",
            onConfirm = { scope.attempt(snackbar) { link.connection().clearSession(session) } },
            onDismiss = { clearing = false },
        )
    }
}

/**
 * The conversation, newest at the bottom. It follows new messages while the
 * reader is at the bottom, and stays put once they scroll up to read.
 */
@Composable
private fun Messages(
    conversation: ConversationState,
    onLoadEarlier: () -> Unit,
    onDecide: (String, ApprovalDecision) -> Unit,
) {
    val items = conversation.items
    val listState = rememberLazyListState()
    val scope = rememberCoroutineScope()
    // Only the latest items are loaded; earlier ones come when the reader nears the top.
    LaunchedEffect(listState) {
        // Also after a page lands, in case the top is still in view.
        snapshotFlow {
            val info = listState.layoutInfo
            val nearTop = info.totalItemsCount > 0 &&
                (info.visibleItemsInfo.lastOrNull()?.index ?: 0) >= info.totalItemsCount - 3
            Triple(nearTop, conversation.items.size, conversation.loadingEarlier)
        }
            .filter { (nearTop, _, loading) -> nearTop && !loading }
            .collect { if (conversation.hasOlder) onLoadEarlier() }
    }
    // Whether to keep the newest message in view; only the reader's own scrolling changes it.
    var follow by remember { mutableStateOf(true) }
    LaunchedEffect(listState) {
        snapshotFlow { listState.isScrollInProgress }
            .filter { !it }
            .collect { follow = listState.firstVisibleItemIndex == 0 && listState.firstVisibleItemScrollOffset < 64 }
    }
    LaunchedEffect(conversation.revision, conversation.activity) {
        if (follow) listState.scrollToItem(0)
    }
    Box(Modifier.fillMaxSize()) {
        // Reversed, so the newest item sits at the bottom and grows upward.
        LazyColumn(
            state = listState,
            reverseLayout = true,
            modifier = Modifier.fillMaxSize(),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(12.dp),
            verticalArrangement = Arrangement.Bottom,
        ) {
            statusRow(conversation)?.let { row ->
                item(key = "activity") {
                    Box(Modifier.padding(top = if (items.isEmpty()) 0.dp else 12.dp)) { row() }
                }
            }
            // Keyed by item, so items that load above keep the ones in view in place.
            items(count = items.size, key = { items[items.size - 1 - it].key }) { reversed ->
                val index = items.size - 1 - reversed
                val item = items[index].item
                Box(Modifier.padding(top = gapBefore(items.getOrNull(index - 1)?.item, item))) {
                    when (item) {
                        is ConversationItem.User -> UserMessage(item)
                        is ConversationItem.Assistant -> AssistantMessage(item)
                        is ConversationItem.Tool -> ToolCall(item, onDecide)
                        is ConversationItem.Notice -> Notice(item)
                    }
                }
            }
            item(key = "start") { SessionStart(conversation, onLoadEarlier) }
        }
        if (!follow) {
            SmallFloatingActionButton(
                onClick = {
                    follow = true
                    scope.launch { listState.scrollToItem(0) }
                },
                modifier = Modifier.align(Alignment.BottomEnd).padding(16.dp),
            ) { LoomIcon(LoomIcons.ArrowDown, "Jump to the newest message", size = 20.dp) }
        }
    }
}

/** The top of the list: earlier items loading, a way to load them, or the start of the session. */
@Composable
private fun SessionStart(conversation: ConversationState, onLoadEarlier: () -> Unit) {
    Box(Modifier.fillMaxWidth().padding(vertical = 8.dp), contentAlignment = Alignment.Center) {
        when {
            conversation.loadingEarlier -> CircularProgressIndicator(Modifier.size(24.dp), strokeWidth = 2.dp)
            conversation.hasOlder -> IconButton(onClick = onLoadEarlier) {
                LoomIcon(
                    LoomIcons.ChevronsUp,
                    "Load earlier messages",
                    size = 20.dp,
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            else -> Text(
                "Beginning of the session",
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

/**
 * The space above an item, as on the desktop: little between tool calls, so
 * a run of them reads as one block, and the most above a prompt, where a
 * turn starts.
 */
private fun gapBefore(previous: ConversationItem?, item: ConversationItem): Dp = when {
    previous == null -> 0.dp
    item is ConversationItem.User -> 24.dp
    previous is ConversationItem.Tool && item is ConversationItem.Tool -> 4.dp
    else -> 12.dp
}

/**
 * The row under the items while the session does something no item shows:
 * the model thinking before any reasoning streams in, or waiting for the
 * workspace. Running tools, approvals, and answers show on their own blocks.
 */
private fun statusRow(conversation: ConversationState): (@Composable () -> Unit)? =
    when (conversation.activity) {
        is Activity.Thinking -> if (reasoningStreams(conversation)) {
            null
        } else {
            { Block(header = { ReasoningHeader("Thinking…", busy = true) }) }
        }
        is Activity.WaitingLock -> {
            {
                Block(header = {
                    SpinningIcon(null, tint = MaterialTheme.colorScheme.primary)
                    Text(
                        "Waiting for the workspace…",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = pulse()),
                        modifier = Modifier.weight(1f),
                    )
                })
            }
        }
        else -> null
    }

/** Whether the reasoning of the answer being written streams in, so its block says the model thinks. */
private fun reasoningStreams(conversation: ConversationState): Boolean {
    val last = conversation.items.lastOrNull()?.item
    return last is ConversationItem.Assistant && last.streaming && last.reasoning.isNotEmpty()
}

/**
 * The frame of every block that is not text: tool calls, the reasoning of an
 * answer, and the status row. A header row opens and closes the body.
 */
@Composable
private fun Block(
    border: Color = MaterialTheme.colorScheme.outlineVariant,
    expanded: Boolean = false,
    onToggle: (() -> Unit)? = null,
    header: @Composable RowScope.() -> Unit,
    body: (@Composable ColumnScope.() -> Unit)? = null,
) {
    Surface(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(8.dp),
        border = BorderStroke(1.dp, border),
        color = MaterialTheme.colorScheme.surface,
    ) {
        Column(Modifier.animateContentSize()) {
            Row(
                Modifier
                    .fillMaxWidth()
                    .then(if (onToggle != null) Modifier.clickable(onClick = onToggle) else Modifier)
                    .padding(horizontal = 12.dp, vertical = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                header()
                if (onToggle != null) {
                    LoomIcon(
                        LoomIcons.ChevronRight,
                        if (expanded) "Collapse" else "Expand",
                        Modifier.rotate(if (expanded) 90f else 0f),
                        size = 16.dp,
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
            if (body != null) {
                Column(
                    Modifier.fillMaxWidth().padding(start = 12.dp, end = 12.dp, bottom = 10.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) { body() }
            }
        }
    }
}

/** The header of a reasoning block: the brain, and a label that pulses while the model thinks. */
@Composable
private fun RowScope.ReasoningHeader(label: String, busy: Boolean) {
    LoomIcon(LoomIcons.Brain, null, size = 16.dp, tint = MaterialTheme.colorScheme.onSurfaceVariant)
    Text(
        label,
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = if (busy) pulse() else 1f),
        modifier = Modifier.weight(1f),
    )
}

/** An alpha that fades in and out, for the label of work in progress. */
@Composable
private fun pulse(): Float {
    val alpha by rememberInfiniteTransition(label = "pulse").animateFloat(
        initialValue = 1f,
        targetValue = 0.35f,
        animationSpec = infiniteRepeatable(tween(900), RepeatMode.Reverse),
        label = "alpha",
    )
    return alpha
}

@Composable
private fun UserMessage(item: ConversationItem.User) {
    Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.CenterEnd) {
        Surface(
            color = MaterialTheme.colorScheme.primaryContainer,
            shape = RoundedCornerShape(16.dp),
            modifier = Modifier.widthIn(max = 340.dp),
        ) {
            Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                if (item.text.isNotEmpty()) SelectionContainer { Text(item.text) }
                if (item.images.isNotEmpty()) {
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                        LoomIcon(LoomIcons.Image, "Images", size = 14.dp)
                        Text("${item.images.size}", style = MaterialTheme.typography.labelMedium)
                    }
                }
            }
        }
    }
}

@Composable
private fun AssistantMessage(item: ConversationItem.Assistant) {
    var showReasoning by remember { mutableStateOf(false) }
    val thinking = item.streaming && item.text.isEmpty()
    Column(Modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        if (item.reasoning.isNotBlank()) {
            Block(
                expanded = showReasoning,
                onToggle = { showReasoning = !showReasoning },
                header = { ReasoningHeader(if (thinking) "Thinking…" else "Reasoning", busy = thinking) },
                body = if (showReasoning) {
                    {
                        SelectionContainer {
                            Text(
                                item.reasoning,
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                } else {
                    null
                },
            )
        }
        if (item.text.isNotEmpty()) {
            SelectionContainer {
                Markdown(content = item.text, modifier = Modifier.fillMaxWidth().padding(horizontal = 2.dp))
            }
        }
    }
}

/**
 * A tool call: its status and name, and once opened its arguments and
 * output. A call that needs approval opens by itself and keeps the buttons
 * to decide.
 */
@Composable
private fun ToolCall(item: ConversationItem.Tool, onDecide: (String, ApprovalDecision) -> Unit) {
    val waiting = item.state == ToolState.WAITING_APPROVAL
    // Null until the reader opens or closes the block.
    var toggled by remember(item.callId) { mutableStateOf<Boolean?>(null) }
    val expanded = toggled ?: waiting
    val status = toolStatus(item.state)
    Block(
        border = if (waiting) LoomColors.warning else MaterialTheme.colorScheme.outlineVariant,
        expanded = expanded,
        onToggle = { toggled = !expanded },
        header = {
            if (item.state == ToolState.RUNNING) {
                SpinningIcon(status.description, tint = status.color)
            } else {
                LoomIcon(status.icon, status.description, size = 16.dp, tint = status.color)
            }
            Text(
                item.name,
                fontFamily = FontFamily.Monospace,
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.primary.copy(
                    alpha = if (item.state == ToolState.RUNNING) pulse() else 1f,
                ),
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
        },
        body = if (expanded || waiting) {
            { ToolBody(item, expanded, onDecide) }
        } else {
            null
        },
    )
}

@Composable
private fun ToolBody(item: ConversationItem.Tool, expanded: Boolean, onDecide: (String, ApprovalDecision) -> Unit) {
    val waiting = item.state == ToolState.WAITING_APPROVAL
    if (expanded) {
        item.approvalReason?.takeIf { waiting }?.let {
            Text(it, style = MaterialTheme.typography.bodySmall, color = LoomColors.warning)
        }
        when (val arguments = item.arguments) {
            is ToolArguments.Code -> CodeBlock(arguments.text)
            is ToolArguments.Diff -> DiffBlock(arguments.text)
            else -> {}
        }
        val diff = item.diff
        val result = item.result
        when {
            diff != null -> DiffBlock(diff)
            result != null -> if (result.isNotBlank()) CodeBlock(result, error = item.isError)
            item.output.isNotEmpty() -> CodeBlock(item.output, cut = item.outputTruncated)
        }
    }
    if (waiting) {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(
                onClick = { onDecide(item.callId, ApprovalDecision.ALLOW) },
                contentPadding = ButtonDefaults.ButtonWithIconContentPadding,
            ) { ButtonIcon(LoomIcons.Check, "Allow") }
            FilledTonalButton(
                onClick = { onDecide(item.callId, ApprovalDecision.ALLOW_FOR_SESSION) },
                contentPadding = ButtonDefaults.ButtonWithIconContentPadding,
            ) { ButtonIcon(LoomIcons.CheckCheck, "Always") }
            OutlinedButton(
                onClick = { onDecide(item.callId, ApprovalDecision.DENY) },
                contentPadding = ButtonDefaults.ButtonWithIconContentPadding,
                colors = ButtonDefaults.outlinedButtonColors(contentColor = MaterialTheme.colorScheme.error),
            ) { ButtonIcon(LoomIcons.X, "Deny") }
        }
    }
}

@Composable
private fun ButtonIcon(icon: Int, label: String) {
    LoomIcon(icon, null, size = ButtonDefaults.IconSize)
    Spacer(Modifier.width(ButtonDefaults.IconSpacing))
    Text(label)
}

/** How a tool call's state shows: an icon in a color, described for screen readers. */
private class ToolStatus(val icon: Int, val color: Color, val description: String)

@Composable
private fun toolStatus(state: ToolState): ToolStatus = when (state) {
    ToolState.REQUESTED -> ToolStatus(LoomIcons.Loader, MaterialTheme.colorScheme.onSurfaceVariant, "Queued")
    ToolState.WAITING_APPROVAL -> ToolStatus(LoomIcons.TriangleAlert, LoomColors.warning, "Needs approval")
    ToolState.RUNNING -> ToolStatus(LoomIcons.LoaderCircle, MaterialTheme.colorScheme.primary, "Running")
    ToolState.SUCCEEDED -> ToolStatus(LoomIcons.CircleCheck, LoomColors.success, "Done")
    ToolState.FAILED -> ToolStatus(LoomIcons.CircleX, MaterialTheme.colorScheme.error, "Failed")
    ToolState.DENIED -> ToolStatus(LoomIcons.Ban, MaterialTheme.colorScheme.error, "Denied")
}

/** The most lines a block shows; earlier ones are counted instead, as on the desktop. */
private const val MAX_BLOCK_LINES = 400

/** The last [MAX_BLOCK_LINES] lines of `text`, saying how many were left out. */
private fun clip(text: String, cut: Boolean = false): String {
    val lines = text.trimEnd('\n').lines()
    val skipped = lines.size - MAX_BLOCK_LINES
    return when {
        skipped > 0 -> "… $skipped earlier lines hidden …\n" + lines.drop(skipped).joinToString("\n")
        cut -> "…\n" + lines.joinToString("\n")
        else -> lines.joinToString("\n")
    }
}

/** Monospaced text such as a command or output, scrolling within a bounded box. */
@Composable
private fun CodeBlock(text: String, error: Boolean = false, cut: Boolean = false) {
    val shown = remember(text, cut) { AnnotatedString(clip(text, cut)) }
    MonoBlock(shown, if (error) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurface)
}

/** Unified diff lines, added ones green, removed ones red, hunk headers apart. */
@Composable
private fun DiffBlock(diff: String) {
    val added = LoomColors.success
    val removed = MaterialTheme.colorScheme.error
    val hunk = LoomColors.hunk
    val shown = remember(diff, added, removed, hunk) {
        buildAnnotatedString {
            clip(diff).lines().forEachIndexed { ix, line ->
                if (ix > 0) append('\n')
                val color = when {
                    line.startsWith("+") && !line.startsWith("+++") -> added
                    line.startsWith("-") && !line.startsWith("---") -> removed
                    line.startsWith("@@") -> hunk
                    else -> null
                }
                if (color != null) withStyle(SpanStyle(color = color)) { append(line) } else append(line)
            }
        }
    }
    MonoBlock(shown, MaterialTheme.colorScheme.onSurface)
}

@Composable
private fun MonoBlock(text: AnnotatedString, color: Color) {
    SelectionContainer {
        Text(
            text,
            fontFamily = FontFamily.Monospace,
            style = MaterialTheme.typography.bodySmall,
            color = color,
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(max = 320.dp)
                .background(MaterialTheme.colorScheme.surfaceContainerHigh, RoundedCornerShape(6.dp))
                .verticalScroll(rememberScrollState())
                .horizontalScroll(rememberScrollState())
                .padding(8.dp),
        )
    }
}

@Composable
private fun Notice(item: ConversationItem.Notice) {
    val (icon, color) = when (item.level) {
        NoticeLevel.ERROR -> LoomIcons.CircleX to MaterialTheme.colorScheme.error
        NoticeLevel.WARNING -> LoomIcons.TriangleAlert to LoomColors.warning
        NoticeLevel.INFO -> LoomIcons.Info to MaterialTheme.colorScheme.onSurfaceVariant
    }
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 2.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        LoomIcon(icon, null, Modifier.padding(top = 2.dp), size = 14.dp, tint = color)
        Text(item.text, style = MaterialTheme.typography.bodySmall, color = color)
    }
}

@Composable
private fun Composer(busy: Boolean, enabled: Boolean, onSend: (String) -> Unit, onStop: () -> Unit) {
    var text by remember { mutableStateOf("") }
    HorizontalDivider()
    Row(
        Modifier.fillMaxWidth().padding(8.dp),
        verticalAlignment = Alignment.Bottom,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        OutlinedTextField(
            value = text,
            onValueChange = { text = it },
            placeholder = { Text("Message") },
            modifier = Modifier.weight(1f),
            maxLines = 6,
        )
        if (busy) {
            IconButton(onClick = onStop, enabled = enabled) { LoomIcon(LoomIcons.Stop, "Stop the turn") }
        }
        IconButton(
            onClick = {
                onSend(text)
                text = ""
            },
            enabled = enabled && text.isNotBlank(),
        ) { LoomIcon(LoomIcons.Send, "Send") }
    }
}

@Composable
private fun ModelDialog(link: AgentLink, current: String?, onPick: (String?) -> Unit, onDismiss: () -> Unit) {
    var models by remember { mutableStateOf<List<Model>?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(Unit) {
        try {
            models = link.connection().listModels()
        } catch (e: Exception) {
            if (e is kotlinx.coroutines.CancellationException) throw e
            error = e.userMessage()
        }
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Model") },
        text = {
            when {
                error != null -> Text(error!!, color = MaterialTheme.colorScheme.error)
                models == null -> CircularProgressIndicator()
                else -> LazyColumn(Modifier.heightIn(max = 420.dp)) {
                    item {
                        ChoiceRow(
                            "Agent's default",
                            link.agent?.defaultModel,
                            selected = current == null,
                        ) {
                            onDismiss()
                            onPick(null)
                        }
                    }
                    items(models!!, key = { it.modelRef }) { model ->
                        ChoiceRow(model.displayName, model.modelRef, selected = current == model.modelRef) {
                            onDismiss()
                            onPick(model.modelRef)
                        }
                    }
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun ReasoningDialog(current: Reasoning?, onPick: (Reasoning?) -> Unit, onDismiss: () -> Unit) {
    val choices = listOf<Reasoning?>(null) + Reasoning.entries
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Reasoning effort") },
        text = {
            Column {
                choices.forEach { choice ->
                    ChoiceRow(choice?.let(::reasoningLabel) ?: "Default", null, selected = current == choice) {
                        onDismiss()
                        onPick(choice)
                    }
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun ChoiceRow(title: String, detail: String?, selected: Boolean, onClick: () -> Unit) {
    ListItem(
        modifier = Modifier.clickable(onClick = onClick),
        headlineContent = { Text(title) },
        supportingContent = detail?.let { { Text(it) } },
        trailingContent = if (selected) {
            { LoomIcon(LoomIcons.Check, "Selected", size = 20.dp, tint = MaterialTheme.colorScheme.primary) }
        } else {
            null
        },
    )
}

fun reasoningLabel(reasoning: Reasoning): String = when (reasoning) {
    Reasoning.NONE -> "None"
    Reasoning.MINIMAL -> "Minimal"
    Reasoning.LOW -> "Low"
    Reasoning.MEDIUM -> "Medium"
    Reasoning.HIGH -> "High"
    Reasoning.XHIGH -> "Extra high"
}

private fun formatTokens(tokens: ULong): String = when {
    tokens >= 1_000_000u -> "%.1fM".format(tokens.toDouble() / 1_000_000)
    tokens >= 1_000u -> "${tokens / 1_000u}k"
    else -> tokens.toString()
}
