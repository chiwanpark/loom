package com.chiwanpark.loom

import android.app.Application
import android.net.ConnectivityManager
import android.net.Network
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ProcessLifecycleOwner
import com.chiwanpark.loom.core.Loom
import java.io.File
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch

/**
 * Holds what outlives a screen: the Rust core, the connections to agents, and
 * where the user is in the app.
 */
class LoomApplication : Application() {
    /** Runs the connections; on the main thread, where Compose state lives. */
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)

    /** The core, or why it could not open its files: the device's key and paired hosts. */
    val core: Result<Loom> by lazy { runCatching { Loom(File(filesDir, "core").path) } }

    val navigator = Navigator()
    val settings by lazy { Settings(this) }
    lateinit var links: AgentLinks
        private set
    lateinit var oracles: OracleLinks
        private set
    lateinit var directory: Directory
        private set
    lateinit var push: Push
        private set
    lateinit var forwarding: Forwarding
        private set

    private val foregroundState = MutableStateFlow(false)

    /** Whether the app is on screen; connections only reconnect while it is. */
    val foreground: StateFlow<Boolean> get() = foregroundState

    override fun onCreate() {
        super.onCreate()
        forwarding = Forwarding(this)
        links = AgentLinks(this)
        oracles = OracleLinks(this)
        directory = Directory(this)
        push = Push(this)
        push.start()
        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) {
                foregroundState.value = true
                // Links may have died while the app was away.
                links.checkAll()
                oracles.checkAll()
            }

            override fun onStop(owner: LifecycleOwner) {
                foregroundState.value = false
            }
        })
        followNetwork()
    }

    /**
     * Checks the links when the device moves to another network, as from
     * Wi-Fi to mobile data, or gets one back: their connections went with
     * the old one, which would otherwise show only at the heartbeat's timeout.
     */
    private fun followNetwork() {
        val connectivity = getSystemService(ConnectivityManager::class.java) ?: return
        connectivity.registerDefaultNetworkCallback(object : ConnectivityManager.NetworkCallback() {
            // Called on a binder thread; the links live on the main thread.
            private var current: Network? = null
            private var first = true

            override fun onAvailable(network: Network) {
                val changed = !first && network != current
                first = false
                current = network
                if (changed) {
                    scope.launch {
                        if (foreground.value) {
                            links.checkAll()
                            oracles.checkAll()
                        }
                    }
                }
            }

            override fun onLost(network: Network) {
                if (network == current) current = null
            }
        })
    }
}
