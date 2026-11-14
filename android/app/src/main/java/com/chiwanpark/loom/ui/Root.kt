package com.chiwanpark.loom.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.key
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
    // Each screen on the stack keeps its own state, also when two of a kind follow each other.
    key(navigator.stack.size, screen) {
        when (screen) {
            Screen.Home -> HomeScreen(app, loom)
            Screen.Hosts -> HostsScreen(app, loom)
            is Screen.Pair -> PairScreen(app, loom, screen.link)
            is Screen.Conversation -> ConversationScreen(
                app,
                app.links.get(loom, screen.host, screen.instance),
                screen.session,
            )
            is Screen.Notes -> NotesScreen(app, app.links.get(loom, screen.host, screen.instance), screen.session)
            is Screen.Tasks -> TasksScreen(app, app.links.get(loom, screen.host, screen.instance), screen.session)
        }
    }
}
