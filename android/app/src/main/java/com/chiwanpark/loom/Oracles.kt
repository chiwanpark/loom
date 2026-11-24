package com.chiwanpark.loom

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import com.chiwanpark.loom.core.Codespace
import com.chiwanpark.loom.core.CodespaceStatus
import com.chiwanpark.loom.core.Loom
import com.chiwanpark.loom.core.LoomException
import com.chiwanpark.loom.core.OracleConnection
import com.chiwanpark.loom.core.OracleLists
import kotlin.math.min
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

class OracleLink(private val app: LoomApplication, private val loom: Loom, val host: String) {
    var lists by mutableStateOf<OracleLists?>(null)
        private set
    var state by mutableStateOf<LinkState>(LinkState.Connecting)
        private set

    private var connection: OracleConnection? = null
    private var job: Job? = null
    private val retry = MutableSharedFlow<Unit>(extraBufferCapacity = 1)
    private val noticeFlow = MutableSharedFlow<String>(extraBufferCapacity = 16)

    val notices: SharedFlow<String> get() = noticeFlow

    val codespaces: List<Codespace> get() = lists?.codespaces ?: emptyList()

    fun codespace(id: String): Codespace? = codespaces.firstOrNull { it.id == id }

    fun connection(): OracleConnection =
        connection ?: throw LoomException.Closed("Not connected to the oracle yet.")

    fun start() {
        if (job?.isActive == true) return
        job = app.scope.launch { run() }
    }

    fun retryNow() {
        retry.tryEmit(Unit)
    }

    fun stop() {
        job?.cancel()
        job = null
        connection?.disconnect()
        connection?.destroy()
        connection = null
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
                    connection = loom.connectOracle(host)
                } else {
                    conn.reconnect()
                }
                state = LinkState.Connected
                wait = 1_000L
                update(connection!!.lists())
                app.push.oracleLinked(this)
                if (!follow(connection!!)) return
                state = LinkState.Down("Lost the connection to the oracle.", fatal = false)
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

    private suspend fun follow(conn: OracleConnection): Boolean {
        while (true) {
            val next = conn.nextLists() ?: return false
            update(next)
            if (!next.connected) return true
        }
    }

    private fun update(next: OracleLists) {
        val before = lists?.codespaces?.associate { it.id to it.status } ?: emptyMap()
        for (codespace in next.codespaces) {
            val old = before[codespace.id] ?: continue
            if (old == codespace.status) continue
            when (codespace.status) {
                CodespaceStatus.READY -> noticeFlow.tryEmit("The codespace ${codespaceTitle(codespace)} is ready.")
                CodespaceStatus.FAILED -> noticeFlow.tryEmit(
                    "The codespace ${codespaceTitle(codespace)} did not start: ${codespace.error ?: "no reason"}",
                )
                CodespaceStatus.STOPPED -> noticeFlow.tryEmit("The codespace ${codespaceTitle(codespace)} is stopped.")
                else -> {}
            }
        }
        lists = next
        val instances = next.codespaces
            .filter { it.status != CodespaceStatus.CLOSING && it.status != CodespaceStatus.CLOSED && it.status != CodespaceStatus.STOPPED }
            .mapNotNull { it.instance }
            .toSet()
        app.links.retainInstances(host, instances)
    }

    suspend fun awaitCodespace(id: String): Codespace? {
        repeat(20) {
            codespace(id)?.let { return it }
            delay(100)
        }
        return codespace(id)
    }
}

class OracleLinks(private val app: LoomApplication) {
    private val links = mutableStateMapOf<String, OracleLink>()

    val all: List<OracleLink> get() = links.values.sortedBy { it.host }

    fun get(loom: Loom, host: String): OracleLink =
        links.getOrPut(host) { OracleLink(app, loom, host) }.also { it.start() }

    fun find(host: String): OracleLink? = links[host]

    fun retainHosts(hosts: Collection<String>) {
        links.keys.filter { it !in hosts }.forEach { host -> links.remove(host)?.stop() }
    }

    fun checkAll() {
        links.values.forEach { link -> if (link.state is LinkState.Down) link.retryNow() }
    }
}

fun codespaceTitle(codespace: Codespace): String = "${codespace.workspace} · ${codespace.branch}"
