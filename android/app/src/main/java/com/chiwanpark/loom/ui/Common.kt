package com.chiwanpark.loom.ui

import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.exclude
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.IconButton
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.ProvideTextStyle
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.LinkState
import com.chiwanpark.loom.R
import com.chiwanpark.loom.userMessage
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch

/**
 * A top bar with a title, an optional subtitle, and a back button when
 * `onBack` is set; without one, the start screen shows the logo there. A
 * subtitle that is a path is cut at its start, so its last part shows.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun LoomTopBar(
    title: String,
    subtitle: String? = null,
    subtitleIsPath: Boolean = false,
    onBack: (() -> Unit)? = null,
    actions: @Composable () -> Unit = {},
) {
    TopAppBar(
        // Lower than Material's 64 dp, so the conversation gets more room.
        expandedHeight = 56.dp,
        title = {
            Column {
                Text(title, style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
                if (subtitle != null) {
                    Text(
                        subtitle,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 1,
                        overflow = if (subtitleIsPath) TextOverflow.StartEllipsis else TextOverflow.Ellipsis,
                    )
                }
            }
        },
        navigationIcon = {
            if (onBack != null) {
                IconButton(onClick = onBack) { LoomIcon(LoomIcons.Back, "Back") }
            } else {
                // Where the back button would be, so the title lines up as on the other screens.
                Box(Modifier.size(48.dp), contentAlignment = Alignment.Center) {
                    Image(painterResource(R.drawable.logo), "Loom logo", Modifier.size(32.dp))
                }
            }
        },
        actions = { actions() },
    )
}

/** Runs `action`, showing what went wrong in the snackbar. */
fun CoroutineScope.attempt(snackbar: SnackbarHostState, action: suspend () -> Unit) {
    launch {
        try {
            action()
        } catch (e: kotlinx.coroutines.CancellationException) {
            throw e
        } catch (e: Exception) {
            snackbar.showSnackbar(e.userMessage())
        }
    }
}

/**
 * The snackbar of a screen, kept above the keyboard. The Scaffold already keeps it
 * above the navigation bar, which the keyboard covers too, so only the rest is added.
 */
@Composable
fun LoomSnackbarHost(state: SnackbarHostState) {
    SnackbarHost(
        state,
        Modifier.windowInsetsPadding(WindowInsets.ime.exclude(WindowInsets.navigationBars)),
    )
}

/** A menu entry with an icon before its label; picking it closes the menu first. */
@Composable
fun MenuEntry(text: String, icon: Int, onClose: () -> Unit, onClick: () -> Unit) {
    DropdownMenuItem(
        text = { Text(text) },
        leadingIcon = { LoomIcon(icon, null, size = 18.dp) },
        onClick = {
            onClose()
            onClick()
        },
    )
}

/**
 * An outlined text field with less padding than Material's, which keeps
 * 16 dp around the text and is at least 56 dp high.
 */
@Composable
fun CompactTextField(
    value: String,
    onValueChange: (String) -> Unit,
    modifier: Modifier = Modifier,
    placeholder: String? = null,
    trailingIcon: (@Composable () -> Unit)? = null,
    maxLines: Int = Int.MAX_VALUE,
    textStyle: TextStyle = MaterialTheme.typography.bodyLarge,
    keyboardOptions: KeyboardOptions = KeyboardOptions.Default,
    keyboardActions: KeyboardActions = KeyboardActions.Default,
) {
    val interaction = remember { MutableInteractionSource() }
    val colors = OutlinedTextFieldDefaults.colors()
    BasicTextField(
        value = value,
        onValueChange = onValueChange,
        modifier = modifier.heightIn(min = 40.dp),
        textStyle = textStyle.copy(color = MaterialTheme.colorScheme.onSurface),
        cursorBrush = SolidColor(MaterialTheme.colorScheme.primary),
        maxLines = maxLines,
        keyboardOptions = keyboardOptions,
        keyboardActions = keyboardActions,
        interactionSource = interaction,
        decorationBox = { inner ->
            OutlinedTextFieldDefaults.DecorationBox(
                value = value,
                innerTextField = inner,
                enabled = true,
                singleLine = false,
                visualTransformation = VisualTransformation.None,
                interactionSource = interaction,
                placeholder = placeholder?.let { { Text(it, style = textStyle) } },
                trailingIcon = trailingIcon,
                colors = colors,
                contentPadding = PaddingValues(horizontal = 12.dp, vertical = 8.dp),
                container = {
                    OutlinedTextFieldDefaults.Container(
                        enabled = true,
                        isError = false,
                        interactionSource = interaction,
                        colors = colors,
                    )
                },
            )
        },
    )
}

/**
 * A list row like Material's ListItem, with less space around it: the slots
 * keep ListItem's names, so it reads the same.
 */
@Composable
fun CompactListItem(
    headlineContent: @Composable () -> Unit,
    modifier: Modifier = Modifier,
    overlineContent: (@Composable () -> Unit)? = null,
    supportingContent: (@Composable () -> Unit)? = null,
    leadingContent: (@Composable () -> Unit)? = null,
    trailingContent: (@Composable () -> Unit)? = null,
) {
    Row(
        modifier.fillMaxWidth().heightIn(min = 48.dp).padding(start = 12.dp, end = 4.dp, top = 4.dp, bottom = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        leadingContent?.invoke()
        Column(Modifier.weight(1f)) {
            val muted = MaterialTheme.colorScheme.onSurfaceVariant
            overlineContent?.let {
                CompositionLocalProvider(LocalContentColor provides muted) {
                    ProvideTextStyle(MaterialTheme.typography.labelSmall, it)
                }
            }
            ProvideTextStyle(MaterialTheme.typography.bodyLarge, headlineContent)
            supportingContent?.let {
                CompositionLocalProvider(LocalContentColor provides muted) {
                    ProvideTextStyle(MaterialTheme.typography.bodySmall, it)
                }
            }
        }
        trailingContent?.invoke()
    }
}

/** One choice in a dialog's list, with a check mark when it is the current one. */
@Composable
fun ChoiceRow(
    title: String,
    detail: String?,
    selected: Boolean,
    titleFont: FontFamily? = null,
    onClick: () -> Unit,
) {
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onClick).padding(horizontal = 4.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.bodyLarge, fontFamily = titleFont)
            if (detail != null) {
                Text(
                    detail,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        if (selected) {
            LoomIcon(LoomIcons.Check, "Selected", size = 20.dp, tint = MaterialTheme.colorScheme.primary)
        }
    }
}

@Composable
fun ConfirmDialog(
    title: String,
    text: String,
    confirm: String,
    onConfirm: () -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = { Text(text) },
        confirmButton = {
            TextButton(onClick = {
                onDismiss()
                onConfirm()
            }) { Text(confirm) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/** Asks for a line of text, such as a new name. */
@Composable
fun TextInputDialog(
    title: String,
    initial: String,
    label: String,
    confirm: String,
    onConfirm: (String) -> Unit,
    onDismiss: () -> Unit,
) {
    var text by remember { mutableStateOf(initial) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            OutlinedTextField(
                value = text,
                onValueChange = { text = it },
                label = { Text(label) },
                singleLine = true,
            )
        },
        confirmButton = {
            TextButton(onClick = {
                onDismiss()
                onConfirm(text)
            }) { Text(confirm) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/** Shows that the link to an agent is down or coming up, with a way to retry. */
@Composable
fun LinkBanner(state: LinkState, onRetry: () -> Unit) {
    val (text, error) = when (state) {
        LinkState.Connected -> return
        LinkState.Connecting -> "Connecting…" to false
        is LinkState.Down -> state.message to true
    }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .background(
                if (error) MaterialTheme.colorScheme.errorContainer else MaterialTheme.colorScheme.secondaryContainer,
            )
            .padding(horizontal = 12.dp, vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        if (!error) CircularProgressIndicator(Modifier.size(16.dp), strokeWidth = 2.dp)
        Text(
            text,
            modifier = Modifier.weight(1f),
            style = MaterialTheme.typography.bodyMedium,
            color = if (error) {
                MaterialTheme.colorScheme.onErrorContainer
            } else {
                MaterialTheme.colorScheme.onSecondaryContainer
            },
        )
        if (error) TextButton(onClick = onRetry) { Text("Retry") }
    }
}

/** The last part of a path, as a short name for a workspace. */
fun shortWorkspace(path: String?): String? = path?.trimEnd('/')?.substringAfterLast('/')?.ifEmpty { path }

/**
 * The agent's workspace directory, for a top bar's subtitle, with its host
 * in front when several hosts are paired.
 */
fun workspaceSubtitle(link: AgentLink, manyHosts: Boolean): String? =
    link.workspace?.let { if (manyHosts) "${link.host}: $it" else it }
