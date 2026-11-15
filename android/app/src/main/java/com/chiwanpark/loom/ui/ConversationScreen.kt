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
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.LocalMinimumInteractiveComponentSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SmallFloatingActionButton
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.isCtrlPressed
import androidx.compose.ui.input.key.key
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.type
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.ConversationState
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.core.Activity
import com.chiwanpark.loom.core.ApprovalDecision
import com.chiwanpark.loom.core.Delivery
import com.chiwanpark.loom.core.WaitingMessage
import com.chiwanpark.loom.core.ConversationItem
import com.chiwanpark.loom.core.Model
import com.chiwanpark.loom.core.NoticeLevel
import com.chiwanpark.loom.core.Policy
import com.chiwanpark.loom.core.Reasoning
import com.chiwanpark.loom.core.SessionStatus
import com.chiwanpark.loom.core.ToolArguments
import com.chiwanpark.loom.core.ToolState
import com.chiwanpark.loom.userMessage
import com.mikepenz.markdown.m3.Markdown
import com.mikepenz.markdown.m3.markdownTypography
import com.mikepenz.markdown.model.MarkdownTypography
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
    var pickPolicy by remember { mutableStateOf(false) }
    var clearing by remember { mutableStateOf(false) }
    var draft by rememberSaveable { mutableStateOf("") }

    // Follows the conversation while it is on the back stack, also after reconnecting
    // and after the agent was restarted, when the link may be another one.
    val connected = link.state == LinkState.Connected
    LaunchedEffect(link, connected) {
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
    // Its notification has been seen.
    LaunchedEffect(link.host, session) {
        app.push.clear(link.host, session)
    }

    Scaffold(
        topBar = {
            // The model, reasoning effort, and context sit above the prompt box, where they have room.
            LoomTopBar(
                conversation.title.ifEmpty { "Session" },
                subtitle = workspaceSubtitle(link, manyHosts = app.directory.hosts.size > 1),
                subtitleIsPath = true,
                onBack = { app.navigator.pop() },
            ) {
                IconButton(onClick = { app.navigator.push(Screen.Tasks(link.host, link.instance, session)) }) {
                    LoomIcon(LoomIcons.Tasks, "Tasks")
                }
                IconButton(onClick = { app.navigator.push(Screen.Notes(link.host, link.instance)) }) {
                    LoomIcon(LoomIcons.Notes, "Notes")
                }
                IconButton(onClick = { menu = true }) { LoomIcon(LoomIcons.More, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    val close = { menu = false }
                    MenuEntry("Changes", LoomIcons.GitCompare, close) {
                        app.navigator.push(Screen.Changes(link.host, link.instance, session))
                    }
                    MenuEntry("Files", LoomIcons.Folder, close) {
                        app.navigator.push(Screen.Files(link.host, link.instance))
                    }
                    MenuEntry("Terminal", LoomIcons.Terminal, close) {
                        // Another client's terminal shows first when the app has none.
                        app.navigator.push(Screen.Terminal(link.host, link.instance, link.defaultTerminal() ?: 0))
                    }
                    MenuEntry("Ports", LoomIcons.Plug, close) {
                        app.navigator.push(Screen.Ports(link.host, link.instance))
                    }
                    MenuEntry("Font", LoomIcons.ALargeSmall, close) { app.navigator.push(Screen.Font) }
                    MenuEntry("Compact the context", LoomIcons.FoldVertical, close) {
                        scope.attempt(snackbar) { link.connection().compactSession(session) }
                    }
                    MenuEntry("Clear the conversation", LoomIcons.Eraser, close) { clearing = true }
                }
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding).imePadding()) {
            LinkBanner(link.state, onRetry = link::retryNow)
            Box(Modifier.weight(1f).fillMaxWidth()) {
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
            Waiting(conversation.waiting, enabled = connected, onTakeBack = { id ->
                scope.attempt(snackbar) {
                    val taken = link.connection().takeBack(session, id)
                    draft = listOf(draft.trimEnd(), taken.text).filter { it.isNotEmpty() }.joinToString("\n\n")
                }
            })
            Composer(
                text = draft,
                onTextChange = { draft = it },
                busy = conversation.status != SessionStatus.IDLE,
                enabled = connected,
                info = {
                    SessionInfo(
                        model = conversation.model ?: link.agent?.defaultModel,
                        reasoning = conversation.reasoning,
                        policy = conversation.policy,
                        contextTokens = conversation.contextTokens,
                        enabled = connected,
                        onModel = { pickModel = true },
                        onReasoning = { pickReasoning = true },
                        onPolicy = { pickPolicy = true },
                    )
                },
                onSend = { text, delivery ->
                    scope.attempt(snackbar) { link.connection().sendPrompt(session, text, emptyList(), delivery) }
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
    if (pickPolicy) {
        PolicyDialog(current = conversation.policy, onPick = { policy ->
            scope.attempt(snackbar) { link.connection().setPolicy(session, policy) }
        }, onDismiss = { pickPolicy = false })
    }
    if (clearing) {
        ConfirmDialog(
            title = "Clear the conversation?",
            text = "The agent forgets the conversation. The session's tasks and the agent's notes stay.",
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
            contentPadding = androidx.compose.foundation.layout.PaddingValues(horizontal = 8.dp, vertical = 6.dp),
            verticalArrangement = Arrangement.Bottom,
        ) {
            statusRow(conversation)?.let { row ->
                item(key = "activity") {
                    Box(Modifier.padding(top = if (items.isEmpty()) 0.dp else 8.dp)) { row() }
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
    Box(Modifier.fillMaxWidth().padding(vertical = 4.dp), contentAlignment = Alignment.Center) {
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
    item is ConversationItem.User -> 16.dp
    previous is ConversationItem.Tool && item is ConversationItem.Tool -> 3.dp
    else -> 8.dp
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
                    .padding(horizontal = 10.dp, vertical = 6.dp),
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
                    Modifier.fillMaxWidth().padding(start = 8.dp, end = 8.dp, bottom = 8.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
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
            shape = RoundedCornerShape(14.dp),
            modifier = Modifier.widthIn(max = 360.dp),
        ) {
            Column(Modifier.padding(horizontal = 10.dp, vertical = 7.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
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
    Column(Modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(8.dp)) {
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
                                style = blockStyle(MaterialTheme.typography.bodySmall),
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
                Markdown(
                    content = item.text,
                    typography = answerTypography(),
                    modifier = Modifier.fillMaxWidth().padding(horizontal = 2.dp),
                )
            }
        }
    }
}

/**
 * The styles of an answer: headings in title sizes rather than the
 * renderer's display sizes, which fill a phone's screen, and code in the
 * code font.
 */
@Composable
private fun answerTypography(): MarkdownTypography {
    val type = MaterialTheme.typography
    val code = LoomFonts.code
    return markdownTypography(
        h1 = type.headlineSmall,
        h2 = type.titleLarge,
        h3 = type.titleMedium,
        h4 = type.titleMedium,
        h5 = type.titleSmall,
        h6 = type.titleSmall,
        code = type.bodySmall.copy(fontFamily = code),
        inlineCode = type.bodyLarge.copy(fontFamily = code, fontSize = TextUnit.Unspecified),
    )
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
                fontFamily = LoomFonts.code,
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
            Text(it, style = blockStyle(MaterialTheme.typography.bodySmall), color = LoomColors.warning)
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
private fun ButtonIcon(icon: Int, label: String, widest: List<String> = emptyList()) {
    LoomIcon(icon, null, size = ButtonDefaults.IconSize)
    Spacer(Modifier.width(ButtonDefaults.IconSpacing))
    // The other labels in `widest` take room unseen, so the button keeps its
    // width when its label changes.
    Box(contentAlignment = Alignment.Center) {
        widest.filter { it != label }.forEach {
            Text(it, Modifier.alpha(0f).clearAndSetSemantics {})
        }
        Text(label)
    }
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
            fontFamily = LoomFonts.code,
            style = blockStyle(MaterialTheme.typography.bodySmall),
            color = color,
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(max = 320.dp)
                .background(MaterialTheme.colorScheme.surfaceContainerHigh, RoundedCornerShape(6.dp))
                .verticalScroll(rememberScrollState())
                .horizontalScroll(rememberScrollState())
                .padding(horizontal = 6.dp, vertical = 4.dp),
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
private fun Composer(
    text: String,
    onTextChange: (String) -> Unit,
    busy: Boolean,
    enabled: Boolean,
    info: @Composable () -> Unit = {},
    onSend: (String, Delivery) -> Unit,
    onStop: () -> Unit,
) {
    val canSend = enabled && text.isNotBlank()
    fun send(delivery: Delivery) {
        if (!canSend) return
        onSend(text, delivery)
        onTextChange("")
    }
    HorizontalDivider()
    // The text field takes the whole width, and the buttons always have the row
    // below it, so nothing moves when a turn starts or ends.
    Column(
        Modifier.fillMaxWidth().padding(horizontal = 6.dp, vertical = 4.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        info()
        CompactTextField(
            value = text,
            onValueChange = onTextChange,
            placeholder = if (busy) "Queue a message, or steer this turn" else "Message",
            modifier = Modifier.fillMaxWidth().onPreviewKeyEvent { event ->
                // Ctrl+Enter on a hardware keyboard steers, as on the desktop, and sends while idle.
                if (event.type == KeyEventType.KeyDown && event.key == Key.Enter && event.isCtrlPressed) {
                    send(if (busy) Delivery.STEER else Delivery.QUEUE)
                    true
                } else {
                    false
                }
            },
            maxLines = 6,
        )
        // Stop and Steer stay in place while idle, disabled; the last button
        // sends while idle and queues while a turn runs, as wide either way.
        Row(
            Modifier.fillMaxWidth(),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedButton(
                onClick = onStop,
                enabled = enabled && busy,
                contentPadding = ButtonDefaults.ButtonWithIconContentPadding,
                colors = ButtonDefaults.outlinedButtonColors(contentColor = MaterialTheme.colorScheme.error),
            ) { ButtonIcon(LoomIcons.Stop, "Stop") }
            Spacer(Modifier.weight(1f))
            FilledTonalButton(
                onClick = { send(Delivery.STEER) },
                enabled = canSend && busy,
                contentPadding = ButtonDefaults.ButtonWithIconContentPadding,
            ) { ButtonIcon(LoomIcons.CornerDownRight, "Steer") }
            Button(
                onClick = { send(Delivery.QUEUE) },
                enabled = canSend,
                contentPadding = ButtonDefaults.ButtonWithIconContentPadding,
            ) { ButtonIcon(LoomIcons.Send, if (busy) "Queue" else "Send", widest = listOf("Queue", "Send")) }
        }
    }
}

/** The messages that wait for the running turn, each with a way to take it back into the composer. */
@Composable
private fun Waiting(messages: List<WaitingMessage>, enabled: Boolean, onTakeBack: (ULong) -> Unit) {
    if (messages.isEmpty()) return
    HorizontalDivider()
    // Rows as low as their take-back button, 32 dp instead of Material's
    // 48 dp touch target, so waiting messages leave the conversation room.
    CompositionLocalProvider(LocalMinimumInteractiveComponentSize provides 0.dp) {
        Column(Modifier.fillMaxWidth().padding(start = 10.dp, end = 4.dp, top = 2.dp, bottom = 2.dp)) {
            messages.forEach { message ->
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        when (message.delivery) {
                            Delivery.STEER -> "Steer"
                            Delivery.QUEUE -> "Queued"
                        },
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.primary,
                        modifier = Modifier.padding(end = 6.dp),
                    )
                    Text(
                        message.text.replace('\n', ' '),
                        style = MaterialTheme.typography.bodyMedium,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.weight(1f),
                    )
                    IconButton(
                        onClick = { onTakeBack(message.id) },
                        enabled = enabled,
                        modifier = Modifier.size(32.dp),
                    ) {
                        LoomIcon(LoomIcons.X, "Take back to edit", size = 16.dp)
                    }
                }
            }
        }
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

/** What the session does with tool calls that no policy rule in the agent's configuration matches. */
@Composable
private fun PolicyDialog(current: Policy?, onPick: (Policy?) -> Unit, onDismiss: () -> Unit) {
    val choices = listOf<Policy?>(null, Policy.ASK, Policy.ALLOW, Policy.DENY)
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Tool calls") },
        text = {
            Column {
                Text(
                    "For the tool calls of this session that no rule in the agent's policy matches.",
                    style = MaterialTheme.typography.bodyMedium,
                    modifier = Modifier.padding(bottom = 8.dp),
                )
                choices.forEach { choice ->
                    ChoiceRow(policyLabel(choice), policyDetail(choice), selected = current == choice) {
                        onDismiss()
                        onPick(choice)
                    }
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

fun policyLabel(policy: Policy?): String = when (policy) {
    null -> "Default"
    Policy.ASK -> "Ask"
    Policy.ALLOW -> "Allow"
    Policy.DENY -> "Deny"
}

private fun policyDetail(policy: Policy?): String = when (policy) {
    null -> "As the agent's configuration says"
    Policy.ASK -> "Wait for your approval"
    Policy.ALLOW -> "Run them without asking"
    Policy.DENY -> "Refuse them"
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

/**
 * The session's model, reasoning effort, policy for tool calls, and context
 * used, in a row above the prompt box. All but the context open their pickers.
 */
@Composable
private fun SessionInfo(
    model: String?,
    reasoning: Reasoning?,
    policy: Policy?,
    contextTokens: ULong?,
    enabled: Boolean,
    onModel: () -> Unit,
    onReasoning: () -> Unit,
    onPolicy: () -> Unit,
) {
    Row(
        Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        // The model, the effort, and the policy on the left, the model shortened when they
        // do not fit; the context on the right.
        Row(Modifier.weight(1f), verticalAlignment = Alignment.CenterVertically) {
            InfoItem(
                LoomIcons.Bot,
                // The provider takes room the model's own name needs; the picker shows it.
                model?.substringAfter('/') ?: "Default model",
                description = "Model",
                onClick = onModel.takeIf { enabled },
                modifier = Modifier.weight(1f, fill = false),
            )
            InfoItem(
                LoomIcons.Brain,
                reasoning?.let(::reasoningLabel) ?: "Default",
                description = "Reasoning effort",
                onClick = onReasoning.takeIf { enabled },
            )
            InfoItem(
                LoomIcons.ShieldCheck,
                policyLabel(policy),
                description = "Tool calls without a rule",
                onClick = onPolicy.takeIf { enabled },
            )
        }
        contextTokens?.let { InfoItem(LoomIcons.Gauge, formatTokens(it), description = "Context used", onClick = null) }
    }
}

/** An icon and a short value, which opens a picker when it has `onClick`. */
@Composable
private fun InfoItem(
    icon: Int,
    text: String,
    description: String,
    onClick: (() -> Unit)?,
    modifier: Modifier = Modifier,
) {
    val color = MaterialTheme.colorScheme.onSurfaceVariant
    Row(
        modifier
            .clip(RoundedCornerShape(6.dp))
            .then(
                if (onClick != null) {
                    Modifier.clickable(onClickLabel = description, onClick = onClick)
                } else {
                    Modifier
                },
            )
            .semantics(mergeDescendants = true) { contentDescription = "$description: $text" }
            .padding(horizontal = 6.dp, vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        LoomIcon(icon, null, size = 14.dp, tint = color)
        Text(
            text,
            style = MaterialTheme.typography.labelMedium,
            color = color,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}
