package com.chiwanpark.loom.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveableStateHolder
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.Screen
import com.chiwanpark.loom.userMessage

/** The current screen, and going back with the system's back gesture. */
@Composable
fun LoomRoot(app: LoomApplication) {
    val navigator = app.navigator
    BackHandler(enabled = navigator.stack.size > 1) { navigator.pop() }
    val loom = app.core.getOrElse { error ->
        Box(Modifier.fillMaxSize().padding(24.dp), contentAlignment = Alignment.Center) {
            Text(
                "Loom could not open its files: ${error.userMessage()}",
                color = MaterialTheme.colorScheme.error,
            )
        }
        return
    }
    val screen = navigator.current
    // Each screen on the stack keeps its saved state, such as a draft, while
    // another one is on top of it, also when two of a kind follow each other;
    // a screen's state goes when it leaves the stack.
    val states = rememberSaveableStateHolder()
    val keys = navigator.stack.mapIndexed { depth, it -> "$depth:$it" }
    val kept = remember { mutableSetOf<String>() }
    SideEffect {
        (kept - keys.toSet()).forEach(states::removeState)
        kept.clear()
        kept.addAll(keys)
    }
    states.SaveableStateProvider(keys.last()) {
        when (screen) {
            Screen.Home -> HomeScreen(app, loom)
            Screen.Hosts -> HostsScreen(app, loom)
            Screen.Font -> FontScreen(app)
            is Screen.Pair -> PairScreen(app, loom, screen.link)
            is Screen.Conversation -> ConversationScreen(
                app,
                app.links.get(loom, screen.host, screen.instance),
                screen.session,
            )
            is Screen.Notes -> NotesScreen(app, app.links.get(loom, screen.host, screen.instance))
            is Screen.Tasks -> TasksScreen(app, app.links.get(loom, screen.host, screen.instance), screen.session)
            is Screen.SystemPrompt -> SystemPromptScreen(
                app, app.links.get(loom, screen.host, screen.instance), screen.session,
            )
            is Screen.Files -> FilesScreen(app, app.links.get(loom, screen.host, screen.instance), screen.path)
            is Screen.File -> FileScreen(app, app.links.get(loom, screen.host, screen.instance), screen.path)
            is Screen.Changes -> ChangesScreen(app, app.links.get(loom, screen.host, screen.instance), screen.session)
            is Screen.Diff -> DiffScreen(
                app,
                app.links.get(loom, screen.host, screen.instance),
                screen.path,
                screen.oldPath,
                screen.session,
            )
            is Screen.Terminal -> TerminalScreen(app, app.links.get(loom, screen.host, screen.instance), screen.number)
            is Screen.Ports -> PortsScreen(app, app.links.get(loom, screen.host, screen.instance))
        }
    }
}
