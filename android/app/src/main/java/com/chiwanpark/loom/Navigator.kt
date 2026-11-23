package com.chiwanpark.loom

import androidx.compose.runtime.mutableStateListOf

/** A screen about one agent on a host. */
sealed interface AgentScreen {
    val host: String
    val instance: String
}

sealed interface OracleScreen {
    val host: String
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

    /** The system prompt and the tools of the session's next model call. */
    data class SystemPrompt(override val host: String, override val instance: String, val session: String) :
        Screen, AgentScreen

    /** A directory of the agent's workspace, or of the worktree of the session `root`; an empty `path` is the top folder. */
    data class Files(
        override val host: String,
        override val instance: String,
        val path: String = "",
        val root: String? = null,
    ) : Screen, AgentScreen

    /** A file of the agent's workspace, or of the worktree of the session `root`, highlighted for its language. */
    data class File(
        override val host: String,
        override val instance: String,
        val path: String,
        val root: String? = null,
    ) : Screen, AgentScreen

    /** The uncommitted changes of the workspace, or of the worktree of `session`, and the files `session` changed. */
    data class Changes(override val host: String, override val instance: String, val session: String) :
        Screen, AgentScreen

    /**
     * A file's diff: what `session` changed in it when there is one, else its
     * uncommitted changes in git in the workspace, or in the worktree of the
     * session `root` (`oldPath` is where a renamed file came from).
     */
    data class Diff(
        override val host: String,
        override val instance: String,
        val path: String,
        val oldPath: String? = null,
        val session: String? = null,
        val root: String? = null,
    ) : Screen, AgentScreen

    /**
     * A terminal on the agent's host, by its number on the agent (one that
     * another client holds too); 0 opens a new one.
     */
    data class Terminal(override val host: String, override val instance: String, val number: Int = 0) :
        Screen, AgentScreen

    /** The ports programs listen on, on the agent's host, and the phone's ports forwarded to them. */
    data class Ports(override val host: String, override val instance: String) : Screen, AgentScreen

    /** The text size and the fonts of text and code. */
    data object Font : Screen

    data class NewCodespace(override val host: String, val workspace: String) : Screen, OracleScreen

    data class CodespaceProgress(override val host: String, val codespace: String) : Screen, OracleScreen
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

    /** Drops the conversation of a session that is gone, with the screens opened from it. */
    fun leaveSession(host: String, instance: String, session: String) {
        val first = stack.indexOfFirst {
            it is Screen.Conversation && it.host == host && it.instance == instance && it.session == session
        }
        if (first > 0) {
            while (stack.size > first) stack.removeAt(stack.lastIndex)
        }
    }

    /** Drops the screens of `host`, as when it is removed. */
    fun leaveHost(host: String) {
        stack.removeAll { screen ->
            (screen is AgentScreen && screen.host == host) ||
                (screen is OracleScreen && screen.host == host)
        }
        if (stack.isEmpty()) stack.add(Screen.Home)
    }
}
