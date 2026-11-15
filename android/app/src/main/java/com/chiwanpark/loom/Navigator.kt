package com.chiwanpark.loom

import androidx.compose.runtime.mutableStateListOf

/** A screen about one agent on a host. */
sealed interface AgentScreen {
    val host: String
    val instance: String
}

/** A place in the app. */
sealed interface Screen {
    /** Every session of every agent on the paired hosts. */
    data object Home : Screen

    /** The paired hosts, to rename or remove them. */
    data object Hosts : Screen

    /** Pairing with a host, with a link that came from outside the app. */
    data class Pair(val link: String = "") : Screen

    data class Conversation(override val host: String, override val instance: String, val session: String) :
        Screen, AgentScreen

    /** The agent's notes, which its sessions share. */
    data class Notes(override val host: String, override val instance: String) : Screen, AgentScreen

    data class Tasks(override val host: String, override val instance: String, val session: String) :
        Screen, AgentScreen

    /** A directory of the agent's workspace; an empty `path` is the workspace itself. */
    data class Files(override val host: String, override val instance: String, val path: String = "") :
        Screen, AgentScreen

    /** A file of the agent's workspace, highlighted for its language. */
    data class File(override val host: String, override val instance: String, val path: String) :
        Screen, AgentScreen

    /** The workspace's uncommitted changes, and the files `session` changed. */
    data class Changes(override val host: String, override val instance: String, val session: String) :
        Screen, AgentScreen

    /**
     * A file's diff: what `session` changed in it when there is one, else its
     * uncommitted changes in git (`oldPath` is where a renamed file came from).
     */
    data class Diff(
        override val host: String,
        override val instance: String,
        val path: String,
        val oldPath: String? = null,
        val session: String? = null,
    ) : Screen, AgentScreen

    /**
     * A terminal on the agent's host, by its number among the agent's
     * terminals in the app; 0 opens a new one.
     */
    data class Terminal(override val host: String, override val instance: String, val number: Int = 0) :
        Screen, AgentScreen

    /** The text size and the fonts of text and code. */
    data object Font : Screen
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
        stack.removeAll { screen -> screen is AgentScreen && screen.host == host }
        if (stack.isEmpty()) stack.add(Screen.Home)
    }
}
