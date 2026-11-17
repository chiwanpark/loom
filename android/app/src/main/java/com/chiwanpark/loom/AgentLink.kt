package com.chiwanpark.loom

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import com.chiwanpark.loom.core.Activity
import com.chiwanpark.loom.core.Agent
import com.chiwanpark.loom.core.AgentConnection
import com.chiwanpark.loom.core.AgentUpdate
import com.chiwanpark.loom.core.ConversationItem
import com.chiwanpark.loom.core.ConversationUpdate
import com.chiwanpark.loom.core.Forward
import com.chiwanpark.loom.core.Host
import com.chiwanpark.loom.core.Loom
import com.chiwanpark.loom.core.LoomException
import com.chiwanpark.loom.core.NoticeLevel
import com.chiwanpark.loom.core.Policy
import com.chiwanpark.loom.core.Reasoning
import com.chiwanpark.loom.core.SessionStatus
import com.chiwanpark.loom.core.SessionSummary
import com.chiwanpark.loom.core.TaskItem
import com.chiwanpark.loom.core.Terminal
import com.chiwanpark.loom.core.TerminalFrame
import com.chiwanpark.loom.core.TerminalPlace
import com.chiwanpark.loom.core.TerminalSummary
import com.chiwanpark.loom.core.WaitingMessage
import kotlin.math.min
import kotlinx.coroutines.Job
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.combine
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

    /** The session's own default for tool calls no rule matches; null follows the agent's configuration. */
    var policy by mutableStateOf<Policy?>(null)
    var contextTokens by mutableStateOf<ULong?>(null)
    var pendingApprovals by mutableStateOf(0)

    /** Messages sent while a turn ran, waiting to be delivered. */
    var waiting by mutableStateOf<List<WaitingMessage>>(emptyList())

    /** The key of the first user message the conversation can go back to; a compaction summarized the ones before. */
    var rewindFrom by mutableStateOf(0L)

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
        policy = update.policy
        contextTokens = update.contextTokens
        pendingApprovals = update.pendingApprovals.toInt()
        waiting = update.waiting
        rewindFrom = update.rewindFrom.toLong()
        loaded = true
        revision++
    }
}

/** A terminal on the agent's host that the app follows, by its number on the agent. */
class TerminalTab(val terminal: Terminal) {
    /** The terminal's number on the agent, which every client shows. */
    val number: Int = terminal.id().toInt()

    /** What it shows now; null until the first frame. */
    var frame by mutableStateOf<TerminalFrame?>(null)
        internal set
    private var job: Job? = null

    /** Whether its shell exited or it ended otherwise. */
    val ended: Boolean get() = frame?.let { it.exitStatus != null || it.ended != null } ?: false

    internal fun follow(link: AgentLink, scope: kotlinx.coroutines.CoroutineScope) {
        job = scope.launch {
            while (true) {
                frame = terminal.nextFrame() ?: break
                // At most one frame for each of the display's, however fast output comes.
                delay(16)
            }
            link.forgetTerminal(this@TerminalTab)
        }
    }

    internal fun stop() {
        job?.cancel()
        terminal.end()
        terminal.destroy()
    }
}

/**
 * A local port of the phone forwarded to `host:port` on the agent's host. It
 * keeps its local port when the link reconnects or follows a restarted
 * agent, and starts again then.
 */
class PortForwardState(val host: String, val port: Int) {
    /** The phone's port, once it listened. */
    var localPort by mutableStateOf<Int?>(null)
        internal set

    /** Why it does not listen now, if it does not. */
    var error by mutableStateOf<String?>(null)
        internal set

    /** How many connections it carried when last asked. */
    var connections by mutableStateOf(0)
        internal set

    internal var forward: Forward? = null

    /** Updates `connections` from the core. */
    fun poll() {
        connections = forward?.connections()?.toInt() ?: 0
    }

    internal fun close() {
        forward?.stop()
        forward?.destroy()
        forward = null
        connections = 0
    }
}

/**
 * The app's link to one agent: it connects, follows the agent's updates, and
 * reconnects after the link is lost, catching the open conversations up on
 * what they missed. When the agent is restarted, it runs as another instance;
 * the link then moves to that instance (see [AgentLinks.retain]).
 */
class AgentLink(
    private val app: LoomApplication,
    private val loom: Loom,
    val host: String,
    initialInstance: String,
) {
    /** The agent's instance on its host; it changes when the agent is restarted. */
    var instance by mutableStateOf(initialInstance)
        private set
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

    /** The terminals the app follows, oldest first. */
    val terminals = mutableStateListOf<TerminalTab>()

    /** Every terminal open on the agent, with where it is open; other clients' too. */
    var agentTerminals by mutableStateOf<List<TerminalSummary>>(emptyList())
        private set

    /** The local ports forwarded to the agent's host, oldest first. */
    val forwards = mutableStateListOf<PortForwardState>()

    /** Sessions the connection follows; it resubscribes them when it reconnects. */
    private val opened = mutableSetOf<String>()
    private var connection: AgentConnection? = null
    private var job: Job? = null
    private var checking = false
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

    /**
     * Ends the link. Its forwards end with it, unless `keepForwards`: then
     * they start again when the link starts again, as after [moveTo].
     */
    fun stop(keepForwards: Boolean = false) {
        job?.cancel()
        job = null
        forwards.forEach { it.close() }
        if (!keepForwards) {
            forwards.clear()
            app.forwarding.update()
        }
        closeTerminals()
        agentTerminals = emptyList()
        connection?.disconnect()
        connection?.destroy()
        connection = null
    }

    /**
     * Makes sure the link still works, as when the app comes back on screen or
     * the device changes networks: a link that died meanwhile would otherwise
     * only show it at the heartbeat's timeout, a minute or more later. A lost
     * link tries again at once instead of after its backoff.
     */
    fun checkLink() {
        when (val current = state) {
            LinkState.Connected -> {
                val conn = connection ?: return
                if (checking) return
                checking = true
                app.scope.launch {
                    // A link that does not answer counts as lost; the pump then reconnects.
                    try {
                        conn.checkLink(CHECK_TIMEOUT_MS)
                    } catch (e: Exception) {
                        if (e is kotlinx.coroutines.CancellationException) throw e
                    } finally {
                        checking = false
                    }
                }
            }
            is LinkState.Down -> if (!current.fatal) retryNow()
            LinkState.Connecting -> {}
        }
    }

    /** Follows the agent to the instance it runs as after a restart. */
    fun moveTo(newInstance: String) {
        if (newInstance == instance) return
        stop(keepForwards = true)
        instance = newInstance
        agent = null
        // The new connection follows no session yet; the screens open theirs again.
        opened.clear()
        state = LinkState.Connecting
        start()
    }

    /** The connection, or an error the screens can show. */
    fun connection(): AgentConnection = connection ?: throw LoomException.Closed("Not connected to the agent yet.")

    /**
     * Forwards a local port to `host:port` on the agent's host, or gives the
     * forward there is already. Throws when the phone cannot listen.
     */
    suspend fun forwardPort(host: String, port: Int): PortForwardState {
        forwards.firstOrNull { it.host == host && it.port == port }?.let { return it }
        val forward = PortForwardState(host, port)
        startForward(forward)
        forwards.add(forward)
        app.forwarding.update()
        return forward
    }

    /** Stops forwarding a port; its connections close. */
    fun stopForward(forward: PortForwardState) {
        forwards.remove(forward)
        forward.close()
        app.forwarding.update()
    }

    private suspend fun startForward(forward: PortForwardState) {
        val started = connection().forwardPort(forward.host, forward.port.toUShort(), forward.localPort?.toUShort())
        forward.forward = started
        forward.localPort = started.localPort().toInt()
        forward.error = null
    }

    /** Starts the forwards that do not listen, as after the link moved to a restarted agent. */
    private suspend fun restoreForwards() {
        for (forward in forwards.filter { it.forward == null }) {
            try {
                startForward(forward)
            } catch (e: LoomException) {
                forward.error = e.userMessage()
            }
        }
    }

    /** Opens a terminal of `cols` by `rows` cells on the agent's host. */
    suspend fun openTerminal(cols: Int, rows: Int): TerminalTab {
        val terminal = connection().openTerminal(cols.toUShort(), rows.toUShort())
        return follow(terminal)
    }

    /**
     * Takes over a terminal that another client opened or holds, at `cols` by
     * `rows` cells; it comes with its recent output.
     */
    suspend fun takeTerminal(number: Int, cols: Int, rows: Int): TerminalTab {
        terminal(number)?.let { tab ->
            if (!tab.ended) {
                tab.terminal.takeOver()
                return tab
            }
            forgetTerminal(tab)
        }
        return follow(connection().attachTerminal(number.toULong(), cols.toUShort(), rows.toUShort()))
    }

    private fun follow(terminal: Terminal): TerminalTab {
        val tab = TerminalTab(terminal)
        terminals.add(tab)
        tab.follow(this, app.scope)
        return tab
    }

    fun terminal(number: Int): TerminalTab? = terminals.firstOrNull { it.number == number }

    /** Where the agent says a terminal is open, or null for one it does not list (yet). */
    fun place(number: Int): TerminalPlace? = agentTerminals.firstOrNull { it.id.toInt() == number }?.place

    /**
     * The numbers of the agent's terminals and the app's, oldest first: those
     * the app follows and those of other clients.
     */
    val terminalNumbers: List<Int>
        get() = (agentTerminals.map { it.id.toInt() } + terminals.map { it.number }).distinct().sorted()

    /** The terminal to show first: the newest one here that runs, else the newest elsewhere. */
    fun defaultTerminal(): Int? =
        terminals.lastOrNull { !it.ended && place(it.number) !is TerminalPlace.Elsewhere }?.number
            ?: terminals.lastOrNull { !it.ended }?.number
            ?: agentTerminals.lastOrNull()?.id?.toInt()

    /** Closes a terminal, ending its shell. */
    fun closeTerminal(tab: TerminalTab) {
        terminals.remove(tab)
        tab.stop()
    }

    internal fun forgetTerminal(tab: TerminalTab) {
        terminals.remove(tab)
    }

    private fun closeTerminals() {
        terminals.toList().forEach(::closeTerminal)
    }

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
        // Forwards keep the link up while the app is away, for the browser.
        val wanted = combine(app.foreground, snapshotFlow { forwards.isNotEmpty() }) { shown, forwarding ->
            shown || forwarding
        }
        while (app.scope.isActive) {
            wanted.first { it }
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
                app.push.linked(this)
                restoreForwards()
                // Following the updates until the link is lost or closed.
                if (!pump(connection!!)) return
                state = LinkState.Down("Lost the connection to the agent.", fatal = false)
            } catch (e: LoomException) {
                fatal = e is LoomException.VersionMismatch || e is LoomException.Rejected ||
                    e is LoomException.Invalid
                state = LinkState.Down(e.message ?: e.toString(), fatal)
                // It may run as another instance now; the host's list says.
                if (e is LoomException.Offline) app.directory.refreshHost(loom, host)
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
                    is AgentUpdate.Terminals -> agentTerminals = update.terminals
                    is AgentUpdate.Notice -> noticeFlow.tryEmit(AppNotice(update.level, update.message))
                }
            }
            if (lost) return true
        }
    }

    private companion object {
        const val CHECK_TIMEOUT_MS: ULong = 5_000uL
    }
}

/**
 * The links the app has made, one per agent, kept while the app runs. A
 * link moves to the instance of its agent's workspace when the agent is
 * restarted, and the screens that name the old instance find it there.
 */
class AgentLinks(private val app: LoomApplication) {
    private val links = mutableStateMapOf<Pair<String, String>, AgentLink>()

    /** Old instances and the ones their agents run as now, by host. */
    private val moved = mutableMapOf<Pair<String, String>, String>()

    /** Every link, in a stable order; reading it in a composable follows changes. */
    val all: List<AgentLink> get() = links.values.sortedWith(compareBy({ it.host }, { it.workspace }))

    fun get(loom: Loom, host: String, instance: String): AgentLink {
        val key = resolve(host to instance)
        return links.getOrPut(key) { AgentLink(app, loom, host, key.second) }.also { it.start() }
    }

    /**
     * Links to an agent that a notification named, with its workspace: when
     * the agent was restarted since, the link moves to the instance of that
     * workspace (see [retain]), also before it ever connected.
     */
    fun expect(loom: Loom, host: String, instance: String, workspace: String?) {
        val link = get(loom, host, instance)
        if (link.workspace == null && workspace != null) {
            link.listed = Agent(instance, workspace, null, null)
        }
    }

    /** Checks every link, as when the app comes back on screen or the network changes. */
    fun checkAll() {
        links.values.forEach { it.checkLink() }
    }

    /** Stops every forward of every link. */
    fun stopForwards() {
        links.values.forEach { link -> link.forwards.toList().forEach(link::stopForward) }
    }

    /** Ends the links to a host's agents, as when it is renamed or removed. */
    fun forget(host: String) {
        links.keys.filter { it.first == host }.forEach { key -> links.remove(key)?.stop() }
        moved.keys.removeAll { it.first == host }
    }

    /**
     * Takes in the agents the host's relay lists now. A link to an instance
     * that is gone moves to the listed agent of its workspace, which is the
     * same agent restarted; without one, it stays (offline) while a screen
     * shows it and ends otherwise.
     */
    fun retain(host: String, agents: List<Agent>) {
        val listed = agents.map { it.instance }.toSet()
        for ((key, link) in links.filter { it.key.first == host && it.key.second !in listed }) {
            val successor = link.workspace?.let { workspace ->
                agents.firstOrNull { it.workspace == workspace }
            }
            when {
                successor != null -> {
                    val target = host to successor.instance
                    links.remove(key)
                    moved[key] = successor.instance
                    if (target in links) {
                        link.stop()
                    } else {
                        link.moveTo(successor.instance)
                        links[target] = link
                    }
                }
                !inUse(key) && link.forwards.isEmpty() -> links.remove(key)?.stop()
            }
        }
    }

    /** Ends the links to hosts that are not paired any more. */
    fun retainHosts(hosts: Collection<String>) {
        links.keys.filter { it.first !in hosts }.forEach { key -> links.remove(key)?.stop() }
        moved.keys.removeAll { it.first !in hosts }
    }

    /** The instance an agent runs as now, following its restarts. */
    private fun resolve(key: Pair<String, String>): Pair<String, String> {
        var current = key
        val seen = mutableSetOf(current)
        while (true) {
            val next = current.first to (moved[current] ?: return current)
            if (!seen.add(next)) return current
            current = next
        }
    }

    /** Whether a screen on the back stack shows the agent. */
    private fun inUse(key: Pair<String, String>): Boolean = app.navigator.stack.any { screen ->
        screen is AgentScreen && resolve(screen.host to screen.instance) == key
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
    private val loading = mutableSetOf<String>()

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

    /** Asks one host's relay for its agents again, as when one went offline. */
    fun refreshHost(loom: Loom, host: String) {
        if (hosts.none { it.name == host }) return
        app.scope.launch { load(loom, host) }
    }

    private suspend fun load(loom: Loom, host: String) {
        if (!loading.add(host)) return
        try {
            loadAgents(loom, host)
        } finally {
            loading.remove(host)
        }
    }

    private suspend fun loadAgents(loom: Loom, host: String) {
        if (agents[host] !is HostAgents.Loaded) agents[host] = HostAgents.Loading
        try {
            val list = loom.listAgents(host)
            agents[host] = HostAgents.Loaded(list)
            app.links.retain(host, list)
            list.forEach { agent -> app.links.get(loom, host, agent.instance).listed = agent }
        } catch (e: LoomException) {
            agents[host] = HostAgents.Failed(e.userMessage())
        }
    }
}

/** A message for an error from the core or elsewhere. */
fun Throwable.userMessage(): String = message?.takeIf { it.isNotBlank() } ?: toString()
