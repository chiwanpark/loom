package com.chiwanpark.loom

import androidx.compose.runtime.mutableStateListOf

/** A place in the app. */
sealed interface Screen {
    /** Every session of every agent on the paired hosts. */
    data object Home : Screen

    /** The paired hosts, to rename or remove them. */
    data object Hosts : Screen

    /** Pairing with a host, with a link that came from outside the app. */
    data class Pair(val link: String = "") : Screen

    data class Conversation(val host: String, val instance: String, val session: String) : Screen

    /** The agent's notes, which its sessions share. */
    data class Notes(val host: String, val instance: String) : Screen

    data class Tasks(val host: String, val instance: String, val session: String) : Screen
}

/** The screens the user went through, the current one last. */
class Navigator {
    val stack = mutableStateListOf<Screen>(Screen.Home)

    val current: Screen get() = stack.last()

    fun push(screen: Screen) {
        stack.add(screen)
    }

    /** Replaces the current screen. */
    fun replace(screen: Screen) {
        stack[stack.lastIndex] = screen
    }

    /** Goes back; false when there is nowhere to go. */
    fun pop(): Boolean {
        if (stack.size <= 1) return false
        stack.removeAt(stack.lastIndex)
        return true
    }

    /** Goes back to the home screen. */
    fun home() {
        stack.clear()
        stack.add(Screen.Home)
    }

    /** Drops the screens of `host`, as when it is removed. */
    fun leaveHost(host: String) {
        stack.removeAll { screen ->
            when (screen) {
                is Screen.Conversation -> screen.host == host
                is Screen.Notes -> screen.host == host
                is Screen.Tasks -> screen.host == host
                else -> false
            }
        }
        if (stack.isEmpty()) stack.add(Screen.Home)
    }
}
