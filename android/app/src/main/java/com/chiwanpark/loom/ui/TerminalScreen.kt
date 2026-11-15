package com.chiwanpark.loom.ui

import android.content.ClipData
import android.content.ClipboardManager
import android.graphics.Typeface
import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFontFamilyResolver
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.TerminalTab
import com.chiwanpark.loom.core.KeyModifiers
import com.chiwanpark.loom.core.TerminalKey
import com.chiwanpark.loom.userMessage

private val NO_MODIFIERS = KeyModifiers(ctrl = false, alt = false, shift = false)

/**
 * A terminal on the agent's host, running the agent's shell in its
 * workspace, with a row of keys the phone's keyboard lacks.
 */
@Composable
fun TerminalScreen(app: LoomApplication, link: AgentLink, number: Int) {
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val connected = link.state == LinkState.Connected
    var menu by remember { mutableStateOf(false) }
    var failed by remember { mutableStateOf<String?>(null) }
    var attempt by remember { mutableStateOf(0) }

    // Number 0 opens a terminal, then stands for it.
    LaunchedEffect(link, connected, number, attempt) {
        if (number != 0 || !connected) return@LaunchedEffect
        failed = null
        try {
            val tab = link.openTerminal(80, 24)
            app.navigator.replace(Screen.Terminal(link.host, link.instance, tab.number))
        } catch (e: kotlinx.coroutines.CancellationException) {
            throw e
        } catch (e: Exception) {
            failed = e.userMessage()
        }
    }
    val tab = if (number == 0) null else link.terminal(number)
    fun newTerminal() = app.navigator.replace(Screen.Terminal(link.host, link.instance))
    val clipboard = remember(context) { context.getSystemService(ClipboardManager::class.java) }

    Scaffold(
        topBar = {
            LoomTopBar(
                tab?.frame?.title?.takeIf { it.isNotBlank() } ?: if (number == 0) "Terminal" else "Terminal $number",
                subtitle = workspaceSubtitle(link, manyHosts = app.directory.hosts.size > 1),
                subtitleIsPath = true,
                onBack = { app.navigator.pop() },
            ) {
                IconButton(onClick = { menu = true }) { LoomIcon(LoomIcons.More, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    val close = { menu = false }
                    MenuEntry("New terminal", LoomIcons.Plus, close, ::newTerminal)
                    for (other in link.terminals) {
                        if (other.number == number) continue
                        val title = other.frame?.title?.takeIf { it.isNotBlank() }
                        MenuEntry(
                            listOfNotNull("Terminal ${other.number}", title).joinToString(": "),
                            LoomIcons.Terminal,
                            close,
                        ) { app.navigator.replace(Screen.Terminal(link.host, link.instance, other.number)) }
                    }
                    if (tab != null) {
                        MenuEntry("Paste", LoomIcons.ClipboardPaste, close) {
                            val text = clipboard?.primaryClip?.takeIf { it.itemCount > 0 }
                                ?.getItemAt(0)?.coerceToText(context)?.toString()
                            if (!text.isNullOrEmpty()) tab.terminal.paste(text)
                        }
                        MenuEntry("Copy the screen", LoomIcons.Copy, close) {
                            clipboard?.setPrimaryClip(ClipData.newPlainText("Terminal", tab.terminal.text()))
                        }
                        MenuEntry("Close the terminal", LoomIcons.X, close) {
                            link.closeTerminal(tab)
                            app.navigator.pop()
                        }
                    }
                }
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding).imePadding()) {
            LinkBanner(link.state, onRetry = link::retryNow)
            when {
                number == 0 -> failed?.let { message ->
                    CenteredMessage("Could not open a terminal: $message", error = true) {
                        TextButton(onClick = {
                            attempt++
                            if (!connected) link.retryNow()
                        }) { Text("Retry") }
                    }
                } ?: Box(Modifier.fillMaxSize(), Alignment.Center) { CircularProgressIndicator() }
                tab == null -> CenteredMessage("This terminal is closed.") {
                    Button(onClick = ::newTerminal) { Text("New terminal") }
                }
                else -> key(tab.number) { TerminalBody(app, tab, onNew = ::newTerminal, onClose = {
                    link.closeTerminal(tab)
                    app.navigator.pop()
                }) }
            }
        }
    }
}

@Composable
private fun ColumnScope.TerminalBody(app: LoomApplication, tab: TerminalTab, onNew: () -> Unit, onClose: () -> Unit) {
    var ctrl by remember { mutableStateOf(false) }
    var alt by remember { mutableStateOf(false) }
    var view by remember { mutableStateOf<TerminalView?>(null) }
    val armed by rememberUpdatedState(KeyModifiers(ctrl = ctrl, alt = alt, shift = false))

    fun take(): KeyModifiers {
        val modifiers = armed
        ctrl = false
        alt = false
        return modifiers
    }

    val terminal = tab.terminal
    val dark = isSystemInDarkTheme()
    val colors = MaterialTheme.colorScheme
    val palette = remember(dark, colors) {
        TerminalPalette(
            colors.onSurface.toArgb(),
            colors.surface.toArgb(),
            if (dark) TerminalPalette.DARK else TerminalPalette.LIGHT,
        )
    }
    val family = LoomFonts.code
    val resolver = LocalFontFamilyResolver.current
    val typeface = remember(family, resolver) { resolver.resolve(family) }.value as? Typeface ?: Typeface.MONOSPACE
    val textSize = app.settings.terminalTextSize
    val frame = tab.frame

    AndroidView(
        factory = { context ->
            TerminalView(context).apply {
                listener = object : TerminalView.Listener {
                    override fun onText(text: String) = terminal.sendText(text)

                    override fun onKey(key: TerminalKey, modifiers: KeyModifiers) = terminal.sendKey(key, modifiers)

                    override fun onResize(cols: Int, rows: Int) = terminal.resize(cols.toUShort(), rows.toUShort())

                    override fun onScroll(lines: Int) = terminal.scroll(lines)

                    override fun onTextSize(sp: Float) = app.settings.updateTerminalTextSize(sp)

                    override fun takeModifiers(): KeyModifiers = take()
                }
                view = this
                post { showKeyboard() }
            }
        },
        update = { view ->
            view.palette = palette
            view.setFont(typeface, textSize)
            view.frame = frame
        },
        modifier = Modifier.fillMaxWidth().weight(1f),
    )
    if (frame != null && (frame.exitStatus != null || frame.ended != null)) {
        Row(
            Modifier.fillMaxWidth().background(MaterialTheme.colorScheme.secondaryContainer)
                .padding(horizontal = 12.dp, vertical = 2.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                frame.ended ?: "The shell exited with status ${frame.exitStatus}.",
                modifier = Modifier.weight(1f),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSecondaryContainer,
            )
            TextButton(onClick = onNew) { Text("New") }
            TextButton(onClick = onClose) { Text("Close") }
        }
    }
    HorizontalDivider()
    ExtraKeys(
        ctrl = ctrl,
        alt = alt,
        onCtrl = { ctrl = !ctrl },
        onAlt = { alt = !alt },
        onKey = { key -> terminal.sendKey(key, take()) },
        onText = { text ->
            val modifiers = take()
            if (modifiers.ctrl || modifiers.alt) terminal.sendKey(TerminalKey.Char(text), modifiers) else terminal.sendText(text)
        },
        onKeyboard = { view?.showKeyboard() },
    )
}

/** The keys a phone's keyboard lacks: Esc, Tab, Ctrl and Alt for the next key, arrows, and a few symbols. */
@Composable
private fun ExtraKeys(
    ctrl: Boolean,
    alt: Boolean,
    onCtrl: () -> Unit,
    onAlt: () -> Unit,
    onKey: (TerminalKey) -> Unit,
    onText: (String) -> Unit,
    onKeyboard: () -> Unit,
) {
    Row(
        Modifier
            .fillMaxWidth()
            .background(MaterialTheme.colorScheme.surfaceContainer)
            .horizontalScroll(rememberScrollState())
            .padding(horizontal = 4.dp, vertical = 4.dp),
        horizontalArrangement = Arrangement.spacedBy(4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        ExtraKey("Esc") { onKey(TerminalKey.Escape) }
        ExtraKey("Tab") { onKey(TerminalKey.Tab) }
        ExtraKey("Ctrl", on = ctrl, onClick = onCtrl)
        ExtraKey("Alt", on = alt, onClick = onAlt)
        ExtraKey("←") { onKey(TerminalKey.Left) }
        ExtraKey("↓") { onKey(TerminalKey.Down) }
        ExtraKey("↑") { onKey(TerminalKey.Up) }
        ExtraKey("→") { onKey(TerminalKey.Right) }
        ExtraKey("Home") { onKey(TerminalKey.Home) }
        ExtraKey("End") { onKey(TerminalKey.End) }
        ExtraKey("PgUp") { onKey(TerminalKey.PageUp) }
        ExtraKey("PgDn") { onKey(TerminalKey.PageDown) }
        for (symbol in listOf("|", "~", "/", "-", "_", "`")) {
            ExtraKey(symbol) { onText(symbol) }
        }
        IconButton(onClick = onKeyboard, modifier = Modifier.height(36.dp)) {
            LoomIcon(LoomIcons.Keyboard, "Show the keyboard", size = 20.dp)
        }
    }
}

@Composable
private fun ExtraKey(label: String, on: Boolean = false, onClick: () -> Unit) {
    Surface(
        onClick = onClick,
        shape = RoundedCornerShape(6.dp),
        color = if (on) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.surfaceContainerHighest,
        contentColor = if (on) MaterialTheme.colorScheme.onPrimary else MaterialTheme.colorScheme.onSurface,
        modifier = Modifier.height(36.dp).widthIn(min = 40.dp),
    ) {
        Box(contentAlignment = Alignment.Center, modifier = Modifier.padding(horizontal = 10.dp)) {
            Text(
                label,
                fontFamily = LoomFonts.code,
                fontWeight = if (on) FontWeight.Bold else FontWeight.Normal,
                style = MaterialTheme.typography.labelLarge,
            )
        }
    }
}
