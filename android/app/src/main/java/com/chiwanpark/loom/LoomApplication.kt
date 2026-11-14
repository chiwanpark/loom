package com.chiwanpark.loom

import android.app.Application
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
    lateinit var links: AgentLinks
        private set
    lateinit var directory: Directory
        private set

    private val foregroundState = MutableStateFlow(false)

    /** Whether the app is on screen; connections only reconnect while it is. */
    val foreground: StateFlow<Boolean> get() = foregroundState

    override fun onCreate() {
        super.onCreate()
        links = AgentLinks(this)
        directory = Directory(this)
        ProcessLifecycleOwner.get().lifecycle.addObserver(object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) {
                foregroundState.value = true
            }

            override fun onStop(owner: LifecycleOwner) {
                foregroundState.value = false
            }
        })
    }
}
