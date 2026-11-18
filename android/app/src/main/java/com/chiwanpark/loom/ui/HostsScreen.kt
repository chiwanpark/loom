package com.chiwanpark.loom.ui

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Build
import android.provider.Settings
import androidx.compose.foundation.background
import androidx.compose.foundation.border
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
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.core.Host
import com.chiwanpark.loom.core.HostColor
import com.chiwanpark.loom.core.Loom
import com.chiwanpark.loom.userMessage
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning
import kotlinx.coroutines.launch

/** The paired hosts. */
@Composable
fun HostsScreen(app: LoomApplication, loom: Loom) {
    var hosts by remember { mutableStateOf(loom.hosts()) }
    var showKey by remember { mutableStateOf(false) }
    var menu by remember { mutableStateOf(false) }
    var renaming by remember { mutableStateOf<Host?>(null) }
    var removing by remember { mutableStateOf<Host?>(null) }
    var updating by remember { mutableStateOf<Host?>(null) }
    var coloring by remember { mutableStateOf<Host?>(null) }
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()

    Scaffold(
        topBar = {
            LoomTopBar("Hosts", onBack = { app.navigator.pop() }) {
                IconButton(onClick = { menu = true }) { LoomIcon(LoomIcons.More, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    val close = { menu = false }
                    MenuEntry("This device's key", LoomIcons.KeyRound, close) { showKey = true }
                }
            }
        },
        floatingActionButton = {
            FloatingActionButton(onClick = { app.navigator.push(Screen.Pair()) }) {
                LoomIcon(LoomIcons.Plus, "Pair with a host")
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        if (hosts.isEmpty()) {
            Box(Modifier.fillMaxSize().padding(padding).padding(32.dp), contentAlignment = Alignment.Center) {
                Text(
                    "No hosts yet. Run `loom-agent pair` on a host whose agents use a relay, " +
                        "then pair with the QR code or link it prints.",
                    style = MaterialTheme.typography.bodyLarge,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        } else {
            LazyColumn(Modifier.fillMaxSize().padding(padding)) {
                items(hosts, key = { it.name }) { host ->
                    HostRow(
                        host,
                        onRename = { renaming = host },
                        onColor = { coloring = host },
                        onRemove = { removing = host },
                        onUpdate = { updating = host },
                    )
                    HorizontalDivider()
                }
            }
        }
    }

    if (showKey) DeviceKeyDialog(loom.deviceKey()) { showKey = false }
    renaming?.let { host ->
        TextInputDialog(
            title = "Rename host",
            initial = host.name,
            label = "Name",
            confirm = "Rename",
            onConfirm = { name ->
                scope.attempt(snackbar) {
                    loom.renameHost(host.name, name)
                    app.links.forget(host.name)
                    app.navigator.leaveHost(host.name)
                    hosts = loom.hosts()
                    app.directory.refresh(loom)
                }
            },
            onDismiss = { renaming = null },
        )
    }
    coloring?.let { host ->
        HostColorDialog(
            host = host.name,
            current = host.color,
            palette = loom.hostPalette(),
            onPick = { color ->
                coloring = null
                scope.attempt(snackbar) {
                    loom.setHostColor(host.name, color)
                    hosts = loom.hosts()
                    app.directory.refresh(loom)
                }
            },
            onDismiss = { coloring = null },
        )
    }
    removing?.let { host ->
        ConfirmDialog(
            title = "Remove ${host.name}?",
            text = "This device forgets the host. The host still lists it until you run " +
                "`loom-agent clients revoke` there.",
            confirm = "Remove",
            onConfirm = {
                scope.attempt(snackbar) {
                    loom.removeHost(host.name)
                    app.links.forget(host.name)
                    app.navigator.leaveHost(host.name)
                    hosts = loom.hosts()
                    app.directory.refresh(loom)
                }
            },
            onDismiss = { removing = null },
        )
    }
    updating?.let { host ->
        val links = app.links.all.filter { it.host == host.name && it.state == LinkState.Connected }
        UpdateAgentsDialog(
            host = host.name,
            links = links,
            onConfirm = { version ->
                scope.attempt(snackbar) {
                    val link = links.firstOrNull { it.state == LinkState.Connected }
                        ?: throw IllegalStateException("No agent on ${host.name} is connected.")
                    val update = link.connection().updateAgent(version)
                    val count = update.restarting.size
                    var message = "Installed ${update.version}; " +
                        if (count == 1) "1 agent restarts." else "$count agents restart."
                    if (update.notRestarted.isNotEmpty()) {
                        message += " ${update.notRestarted.size} of another build keep their version."
                    }
                    snackbar.showSnackbar(message)
                }
            },
            onDismiss = { updating = null },
        )
    }
}

@Composable
private fun HostRow(
    host: Host,
    onRename: () -> Unit,
    onColor: () -> Unit,
    onRemove: () -> Unit,
    onUpdate: () -> Unit,
) {
    var menu by remember { mutableStateOf(false) }
    CompactListItem(
        modifier = Modifier.clickable { menu = true },
        leadingContent = { ColorDot(hostColor(host.color)) },
        headlineContent = { Text(host.name, color = hostColor(host.color) ?: Color.Unspecified) },
        supportingContent = { Text(host.relayUrl.substringAfter("://")) },
        trailingContent = {
            Box {
                IconButton(onClick = { menu = true }) { LoomIcon(LoomIcons.More, "More") }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                    val close = { menu = false }
                    MenuEntry("Rename", LoomIcons.Pencil, close, onRename)
                    MenuEntry("Color…", LoomIcons.Palette, close, onColor)
                    MenuEntry("Update agents…", LoomIcons.Download, close, onUpdate)
                    MenuEntry("Remove", LoomIcons.Trash, close, onRemove)
                }
            }
        },
    )
}

/** A host's color as a dot; a host without one has an outline. */
@Composable
private fun ColorDot(color: Color?) {
    val shape = CircleShape
    val modifier = Modifier.size(16.dp).clip(shape)
    if (color != null) {
        Box(modifier.background(color))
    } else {
        Box(modifier.border(1.5.dp, MaterialTheme.colorScheme.outline, shape))
    }
}

/** Asks for the color that marks the host's sessions on the start screen. */
@Composable
private fun HostColorDialog(
    host: String,
    current: String?,
    palette: List<HostColor>,
    onPick: (String?) -> Unit,
    onDismiss: () -> Unit,
) {
    val selected = hostColor(current)
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Color of $host") },
        text = {
            Column {
                Text(
                    "With several hosts, the start screen shows the host's name in this color.",
                    style = MaterialTheme.typography.bodyMedium,
                    modifier = Modifier.padding(bottom = 8.dp),
                )
                palette.forEach { choice ->
                    val color = hostColor(choice.color)
                    ColorChoice(choice.name, color, selected = color != null && color == selected) {
                        onPick(choice.color)
                    }
                }
                ColorChoice("None", null, selected = selected == null) { onPick(null) }
            }
        },
        confirmButton = {},
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun ColorChoice(name: String, color: Color?, selected: Boolean, onClick: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onClick).padding(horizontal = 4.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        ColorDot(color)
        Text(name, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
        if (selected) {
            LoomIcon(LoomIcons.Check, "Selected", size = 20.dp, tint = MaterialTheme.colorScheme.primary)
        }
    }
}

/**
 * Asks for the release of loom-agent to install on a host. Empty means the
 * latest release; the default is the app's own version.
 */
@Composable
private fun UpdateAgentsDialog(
    host: String,
    links: List<AgentLink>,
    onConfirm: (String?) -> Unit,
    onDismiss: () -> Unit,
) {
    val context = LocalContext.current
    val appVersion = remember {
        context.packageManager.getPackageInfo(context.packageName, 0).versionName.orEmpty()
    }
    var version by remember { mutableStateOf(appVersion) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Update agents on $host") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                if (links.isEmpty()) {
                    Text("Open an agent on $host first. The update goes through a connected agent.")
                } else {
                    Text(
                        "All agents on $host that run the replaced binary restart at once. " +
                            "Their running turns stop.",
                    )
                    links.forEach { link ->
                        Text(
                            "${link.workspace ?: link.instance}: ${link.agent?.version ?: "unknown"}",
                            style = MaterialTheme.typography.bodySmall,
                        )
                    }
                    OutlinedTextField(
                        value = version,
                        onValueChange = { version = it },
                        label = { Text("Version") },
                        supportingText = { Text("Leave empty for the latest release") },
                        singleLine = true,
                    )
                }
            }
        },
        confirmButton = {
            TextButton(
                enabled = links.isNotEmpty(),
                onClick = {
                    onDismiss()
                    onConfirm(version.trim().ifEmpty { null })
                },
            ) { Text("Update") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun DeviceKeyDialog(key: String, onDismiss: () -> Unit) {
    val context = LocalContext.current
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("This device's key") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Text("Hosts list this device by its key in `loom-agent clients list`.")
                SelectionContainer {
                    Text(key, fontFamily = LoomFonts.code)
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss) { Text("Done") } },
        dismissButton = {
            TextButton(onClick = { copy(context, "Loom device key", key) }) { Text("Copy") }
        },
    )
}

/** Pairing with a host by the link `loom-agent pair` prints, scanned or pasted. */
@Composable
fun PairScreen(app: LoomApplication, loom: Loom, initialLink: String) {
    val context = LocalContext.current
    var link by remember { mutableStateOf(initialLink) }
    var name by remember { mutableStateOf("") }
    var label by remember { mutableStateOf(defaultLabel(context)) }
    var pairing by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val scope = rememberCoroutineScope()

    Scaffold(topBar = { LoomTopBar("Pair with a host", onBack = { app.navigator.pop() }) }) { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .imePadding()
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Text(
                "On the host, run `loom-agent pair`. Scan the QR code it prints, or paste its " +
                    "loom:// link. The link works once, for 10 minutes.",
                style = MaterialTheme.typography.bodyMedium,
            )
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = {
                    scan(context, onResult = { link = it }, onError = { error = it })
                }) { Text("Scan QR code") }
                OutlinedButton(onClick = { paste(context)?.let { link = it.trim() } }) { Text("Paste link") }
            }
            OutlinedTextField(
                value = link,
                onValueChange = { link = it },
                label = { Text("Pairing link") },
                modifier = Modifier.fillMaxWidth(),
                minLines = 2,
            )
            OutlinedTextField(
                value = name,
                onValueChange = { name = it },
                label = { Text("Name for the host") },
                placeholder = { Text("such as devbox") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = label,
                onValueChange = { label = it },
                label = { Text("This device, as the host lists it") },
                supportingText = { Text("Letters, digits, '-', '_', '.', or '@'") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            Button(
                enabled = !pairing && link.isNotBlank() && label.isNotBlank(),
                onClick = {
                    pairing = true
                    error = null
                    scope.launch {
                        try {
                            val host = loom.pair(link, label, name)
                            app.links.forget(host.name)
                            // The host forgot where this device's notifications go.
                            app.push.tellAgain()
                            app.navigator.home()
                            app.directory.refresh(loom)
                        } catch (e: Exception) {
                            if (e is kotlinx.coroutines.CancellationException) throw e
                            error = e.userMessage()
                        } finally {
                            pairing = false
                        }
                    }
                },
                modifier = Modifier.fillMaxWidth(),
            ) {
                if (pairing) {
                    CircularProgressIndicator(Modifier.padding(end = 8.dp), strokeWidth = 2.dp)
                }
                Text(if (pairing) "Pairing…" else "Pair")
            }
        }
    }
}

/** The phone's name with what labels allow, as in Galaxy-S23. */
private fun defaultLabel(context: Context): String {
    val name = Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
        ?.takeIf { it.isNotBlank() } ?: Build.MODEL
    return name.trim()
        .replace(Regex("\\s+"), "-")
        .filter { it.isLetterOrDigit() || it in "-_.@" }
        .take(64)
        .ifEmpty { "phone" }
}

private fun scan(context: Context, onResult: (String) -> Unit, onError: (String) -> Unit) {
    val options = GmsBarcodeScannerOptions.Builder()
        .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
        .build()
    GmsBarcodeScanning.getClient(context, options).startScan()
        .addOnSuccessListener { barcode -> barcode.rawValue?.let { onResult(it.trim()) } }
        .addOnFailureListener { e -> onError("Could not scan: ${e.userMessage()}") }
}

private fun paste(context: Context): String? {
    val clipboard = context.getSystemService(ClipboardManager::class.java)
    return clipboard.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(context)?.toString()
}

fun copy(context: Context, label: String, text: String) {
    context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText(label, text))
}
