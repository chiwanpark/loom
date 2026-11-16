package com.chiwanpark.loom.ui

import androidx.compose.animation.animateContentSize
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
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
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.core.SystemPromptInfo
import com.chiwanpark.loom.core.ToolInfo

/**
 * What a session sends to the model besides the conversation: the system
 * prompt and the tools, as the session's next model call would have them.
 */
@Composable
fun SystemPromptScreen(app: LoomApplication, link: AgentLink, session: String) {
    val snackbar = remember { SnackbarHostState() }
    var reload by remember { mutableIntStateOf(0) }
    var tab by rememberSaveable { mutableIntStateOf(0) }
    val loaded = rememberLoaded(link, reload, session) { link.connection().systemPrompt(session) }
    val prompt = (loaded as? Loaded.Ready<SystemPromptInfo>)?.value

    Scaffold(
        topBar = {
            LoomTopBar("System prompt", subtitle = link.conversation(session).title, onBack = { app.navigator.pop() }) {
                IconButton(onClick = { reload++ }) { LoomIcon(LoomIcons.Refresh, "Read it again") }
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            LinkBanner(link.state, onRetry = link::retryNow)
            PrimaryTabRow(selectedTabIndex = tab) {
                Tab(selected = tab == 0, onClick = { tab = 0 }, text = { Text("Prompt") })
                Tab(
                    selected = tab == 1,
                    onClick = { tab = 1 },
                    text = { Text(prompt?.tools?.size?.let { "Tools ($it)" } ?: "Tools") },
                )
            }
            LoadedContent(loaded, onRetry = { reload++ }) { info ->
                if (tab == 0) PromptText(info) else ToolList(info.tools)
            }
        }
    }
}

@Composable
private fun PromptText(info: SystemPromptInfo) {
    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState())) {
        info.warnings.forEach { warning ->
            Text(
                warning,
                style = MaterialTheme.typography.bodySmall,
                color = LoomColors.warning,
                modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 2.dp),
            )
        }
        Text(
            formatSize(info.text.toByteArray().size.toULong()),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 2.dp),
        )
        SelectionContainer {
            Text(
                info.text,
                style = codeStyle(),
                modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp),
            )
        }
    }
}

@Composable
private fun ToolList(tools: List<ToolInfo>) {
    if (tools.isEmpty()) {
        CenteredMessage("The model has no tools.")
        return
    }
    LazyColumn(Modifier.fillMaxSize()) {
        items(tools, key = { it.name }) { tool ->
            ToolRow(tool)
            HorizontalDivider()
        }
    }
}

/** A tool's name and the first line of its description; opened, all of it and its parameters. */
@Composable
private fun ToolRow(tool: ToolInfo) {
    var expanded by rememberSaveable(tool.name) { mutableStateOf(false) }
    Column(Modifier.fillMaxWidth().animateContentSize()) {
        Row(
            Modifier
                .fillMaxWidth()
                .clickable { expanded = !expanded }
                .padding(horizontal = 12.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            LoomIcon(
                LoomIcons.ChevronRight,
                if (expanded) "Collapse" else "Expand",
                Modifier.rotate(if (expanded) 90f else 0f),
                size = 16.dp,
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Column(Modifier.weight(1f)) {
                Text(tool.name, style = codeStyle(), fontWeight = FontWeight.SemiBold)
                if (!expanded) {
                    Text(
                        tool.description.lineSequence().firstOrNull().orEmpty(),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
        }
        if (expanded) {
            SelectionContainer {
                Column(
                    Modifier.fillMaxWidth().padding(start = 36.dp, end = 12.dp, bottom = 10.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    Text(tool.description, style = MaterialTheme.typography.bodyMedium)
                    Text(
                        "Parameters",
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Text(tool.parameters, style = codeStyle())
                }
            }
        }
    }
}
