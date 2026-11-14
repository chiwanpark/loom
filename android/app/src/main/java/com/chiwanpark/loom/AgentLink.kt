package com.chiwanpark.loom

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.chiwanpark.loom.core.Activity
import com.chiwanpark.loom.core.Agent
import com.chiwanpark.loom.core.AgentConnection
import com.chiwanpark.loom.core.AgentUpdate
import com.chiwanpark.loom.core.ConversationItem
import com.chiwanpark.loom.core.ConversationUpdate
import com.chiwanpark.loom.core.Host
import com.chiwanpark.loom.core.Loom
import com.chiwanpark.loom.core.LoomException
import com.chiwanpark.loom.core.NoticeLevel
import com.chiwanpark.loom.core.Reasoning
import com.chiwanpark.loom.core.SessionStatus
import com.chiwanpark.loom.core.SessionSummary
import com.chiwanpark.loom.core.TaskItem
import com.chiwanpark.loom.core.WaitingMessage
import kotlin.math.min
import kotlinx.coroutines.Job
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

/** How the app is linked to an agent. */
sealed interface LinkState {
    data object Connecting : LinkState

    data object Connected : LinkState

    /** Lost or failed; it retries by itself unless `fatal`. */
    data class Down(val message: String, val fatal: Boolean) : LinkState
}

/** A message for the user from the agent or about the connection. */
data class AppNotice(val level: NoticeLevel, val message: String)

/** A conversation item with the key that stays when earlier items go before it. */
data class ShownItem(val key: Long, val item: ConversationItem)

/** One session's conversation as the app shows it, kept up to date by the core. */
class ConversationState(val session: String) {
    /** The latest items of the session, oldest first; earlier ones load on demand. */
    val items = mutableStateListOf<ShownItem>()
    var loaded by mutableStateOf(false)

    /** Whether the session has items before [items]. */
    var hasOlder by mutableStateOf(false)
        private set
    var loadingEarlier by mutableStateOf(false)
    var title by mutableStateOf("")
    var status by mutableStateOf(SessionStatus.IDLE)
    var activity by mutableStateOf<Activity?>(null)
    var model by mutableStateOf<String?>(null)
    var reasoning by mutableStateOf<Reasoning?>(null)
    var contextTokens by mutableStateOf<ULong?>(null)
    var pendingApprovals by mutableStateOf(0)

    /** Messages sent while a turn ran, waiting to be delivered. */
    var waiting by mutableStateOf<List<WaitingMessage>>(emptyList())

    /** Counts the updates, so the screen can follow every change, also one that only grows the last item. */
    var revision by mutableStateOf(0L)
        private set

    fun apply(update: ConversationUpdate) {
        if (update.reset) items.clear()
        val len = update.len.toInt()
        if (items.size > len) items.removeRange(len, items.size)
        for (changed in update.items) {
            val index = changed.index.toInt()
            val shown = ShownItem(changed.key.toLong(), changed.item)
            if (index < items.size) items[index] = shown else items.add(shown)
        }
        hasOlder = update.hasOlder
        title = update.title
        status = update.status
        activity = update.activity
        model = update.model
        reasoning = update.reasoning
        contextTokens = update.contextTokens
        pendingApprovals = update.pendingApprovals.toInt()
        waiting = update.waiting
        loaded = true
        revision++
    }
}

/**
 * The app's link to one agent: it connects, follows the agent's updates, and
 * reconnects after the link is lost, catching the open conversations up on
 * what they missed.
 */
class AgentLink(
    private val app: LoomApplication,
    private val loom: Loom,
    val host: String,
    val instance: String,
) {
    var state by mutableStateOf<LinkState>(LinkState.Connecting)
        private set
    var agent by mutableStateOf<Agent?>(null)
        private set

    /** The agent as its host's relay listed it, before the link is up. */
    var listed by mutableStateOf<Agent?>(null)

    /** The agent's workspace, for showing which agent this is. */
    val workspace: String? get() = agent?.workspace ?: listed?.workspace
    var sessions by mutableStateOf<List<SessionSummary>>(emptyList())
        private set

    private val conversations = mutableMapOf<String, ConversationState>()

    /** Sessions the connection follows; it resubscribes them when it reconnects. */
    private val opened = mutableSetOf<String>()
    private var connection: AgentConnection? = null
    private var job: Job? = null
    private val retry = MutableSharedFlow<Unit>(extraBufferCapacity = 1)

    private val noticeFlow = MutableSharedFlow<AppNotice>(extraBufferCapacity = 16)
    private val tasksFlow = MutableSharedFlow<Pair<String, List<TaskItem>>>(extraBufferCapacity = 16)
    private val notesFlow = MutableSharedFlow<String>(extraBufferCapacity = 16)

    val notices: SharedFlow<AppNotice> get() = noticeFlow

    /** Task lists as the agent or another client changed them, by session. */
    val tasksChanged: SharedFlow<Pair<String, List<TaskItem>>> get() = tasksFlow

    /** The versions of the agent's notes as they change. */
    val notesChanged: SharedFlow<String> get() = notesFlow

    fun start() {
        if (job?.isActive == true) return
        job = app.scope.launch { run() }
    }

    /** Tries again now instead of waiting, also after a fatal error. */
    fun retryNow() {
        retry.tryEmit(Unit)
    }

    fun stop() {
        job?.cancel()
        connection?.disconnect()
        connection?.destroy()
        connection = null
    }

    /** The connection, or an error the screens can show. */
    fun connection(): AgentConnection = connection ?: throw LoomException.Closed("Not connected to the agent yet.")

    fun conversation(session: String): ConversationState =
        conversations.getOrPut(session) { ConversationState(session) }

    /** Follows a session's conversation until [closeSession]; no-op if it does. */
    suspend fun openSession(session: String) {
        conversation(session)
        if (session in opened) return
        connection().openSession(session)
        opened.add(session)
    }

    /** Loads the items before the ones a conversation shows, unless it is loading them already. */
    suspend fun loadEarlier(session: String) {
        val conversation = conversations[session] ?: return
        if (conversation.loadingEarlier || !conversation.hasOlder) return
        conversation.loadingEarlier = true
        try {
            connection().loadEarlier(session)
        } catch (e: LoomException) {
            noticeFlow.tryEmit(AppNotice(NoticeLevel.ERROR, "Could not load earlier messages: ${e.userMessage()}"))
        } finally {
            conversation.loadingEarlier = false
        }
    }

    suspend fun closeSession(session: String) {
        conversations.remove(session)
        if (opened.remove(session)) runCatching { connection?.closeSession(session) }
    }

    private suspend fun run() {
        var wait = 1_000L
        while (app.scope.isActive) {
            app.foreground.first { it }
            state = LinkState.Connecting
            var fatal = false
            try {
                val conn = connection
                if (conn == null) {
                    val opened = loom.connect(host, instance)
                    connection = opened
                    agent = opened.agent()
                } else {
                    conn.reconnect()
                }
                state = LinkState.Connected
                wait = 1_000L
                // Following the updates until the link is lost or closed.
                if (!pump(connection!!)) return
                state = LinkState.Down("Lost the connection to the agent.", fatal = false)
            } catch (e: LoomException) {
                fatal = e is LoomException.VersionMismatch || e is LoomException.Rejected ||
                    e is LoomException.Invalid
                state = LinkState.Down(e.message ?: e.toString(), fatal)
            }
            if (fatal) {
                retry.first()
            } else {
                withTimeoutOrNull(wait) { retry.first() }
                wait = min(wait * 2, 30_000L)
            }
        }
    }

    /** Applies updates until the link is lost (true) or closed for good (false). */
    private suspend fun pump(conn: AgentConnection): Boolean {
        while (true) {
            val updates = conn.nextUpdates() ?: return false
            var lost = false
            for (update in updates) {
                when (update) {
                    is AgentUpdate.Link -> if (!update.connected) lost = true
                    is AgentUpdate.Sessions -> sessions = update.sessions
                    is AgentUpdate.Conversation ->
                        conversations[update.update.session]?.apply(update.update)
                    is AgentUpdate.NotesChanged -> notesFlow.tryEmit(update.version)
                    is AgentUpdate.TasksChanged -> tasksFlow.tryEmit(update.session to update.tasks)
                    is AgentUpdate.Notice -> noticeFlow.tryEmit(AppNotice(update.level, update.message))
                }
            }
            if (lost) return true
        }
    }
}

/** The links the app has made, one per agent, kept while the app runs. */
class AgentLinks(private val app: LoomApplication) {
    private val links = mutableStateMapOf<Pair<String, String>, AgentLink>()

    /** Every link, in a stable order; reading it in a composable follows changes. */
    val all: List<AgentLink> get() = links.values.sortedWith(compareBy({ it.host }, { it.workspace }))

    fun get(loom: Loom, host: String, instance: String): AgentLink =
        links.getOrPut(host to instance) { AgentLink(app, loom, host, instance) }.also { it.start() }

    /** Ends the links to a host's agents, as when it is renamed or removed. */
    fun forget(host: String) {
        links.keys.filter { it.first == host }.forEach { key -> links.remove(key)?.stop() }
    }

    /** Ends the links to a host's agents that are not in `instances` any more. */
    fun retain(host: String, instances: Collection<String>) {
        links.keys.filter { it.first == host && it.second !in instances }
            .forEach { key -> links.remove(key)?.stop() }
    }

    /** Ends the links to hosts that are not paired any more. */
    fun retainHosts(hosts: Collection<String>) {
        links.keys.filter { it.first !in hosts }.forEach { key -> links.remove(key)?.stop() }
    }
}

/** What the app knows of the agents on a host. */
sealed interface HostAgents {
    data object Loading : HostAgents

    data class Loaded(val agents: List<Agent>) : HostAgents

    data class Failed(val message: String) : HostAgents
}

/**
 * The paired hosts and the agents on them. Refreshing it asks each host's
 * relay which agents are there and links to each of them.
 */
class Directory(private val app: LoomApplication) {
    var hosts by mutableStateOf<List<Host>>(emptyList())
        private set
    val agents = mutableStateMapOf<String, HostAgents>()
    var refreshing by mutableStateOf(false)
        private set
    private var job: Job? = null

    fun refresh(loom: Loom) {
        hosts = loom.hosts()
        val names = hosts.map { it.name }
        agents.keys.retainAll(names.toSet())
        app.links.retainHosts(names)
        job?.cancel()
        job = app.scope.launch {
            refreshing = true
            try {
                coroutineScope { names.forEach { host -> launch { load(loom, host) } } }
            } finally {
                refreshing = false
            }
        }
    }

    private suspend fun load(loom: Loom, host: String) {
        if (agents[host] !is HostAgents.Loaded) agents[host] = HostAgents.Loading
        try {
            val list = loom.listAgents(host)
            agents[host] = HostAgents.Loaded(list)
            app.links.retain(host, list.map { it.instance })
            list.forEach { agent -> app.links.get(loom, host, agent.instance).listed = agent }
        } catch (e: LoomException) {
            agents[host] = HostAgents.Failed(e.userMessage())
        }
    }
}

/** A message for an error from the core or elsewhere. */
fun Throwable.userMessage(): String = message?.takeIf { it.isNotBlank() } ?: toString()
