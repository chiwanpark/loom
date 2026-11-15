package com.chiwanpark.loom

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import com.chiwanpark.loom.ui.LoomRoot
import com.chiwanpark.loom.ui.LoomTheme

class MainActivity : ComponentActivity() {
    private val askNotifications = registerForActivityResult(ActivityResultContracts.RequestPermission()) {}

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val app = application as LoomApplication
        if (savedInstanceState == null) openLink(intent)
        setContent {
            val settings = app.settings
            LoomTheme(
                textScale = settings.textScale,
                blockScale = settings.blockScale,
                textFont = settings.textFont,
                codeFont = settings.codeFont,
                textFallback = settings.textFallbackFont,
                codeFallback = settings.codeFallbackFont,
            ) {
                LoomRoot(app)
            }
        }
        askForNotifications(app)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        openLink(intent)
    }

    /**
     * A pairing link opened from outside the app goes to the pairing screen,
     * and a tapped notification to its session.
     */
    private fun openLink(intent: Intent?) {
        val app = application as LoomApplication
        if (intent?.action == Push.ACTION_OPEN_SESSION) {
            openSession(app, intent)
            return
        }
        val link = intent?.takeIf { it.action == Intent.ACTION_VIEW }?.dataString ?: return
        app.navigator.push(Screen.Pair(link))
    }

    private fun openSession(app: LoomApplication, intent: Intent) {
        val host = intent.getStringExtra(Push.EXTRA_HOST) ?: return
        val instance = intent.getStringExtra(Push.EXTRA_INSTANCE) ?: return
        val session = intent.getStringExtra(Push.EXTRA_SESSION) ?: return
        val loom = app.core.getOrNull() ?: return
        // The home screen did not load the hosts when the app starts here,
        // and following a restarted agent needs them.
        if (app.directory.hosts.isEmpty()) app.directory.refresh(loom)
        app.links.expect(loom, host, instance, intent.getStringExtra(Push.EXTRA_WORKSPACE))
        val current = app.navigator.current
        if (current is Screen.Conversation && current.host == host && current.session == session) return
        app.navigator.home()
        app.navigator.push(Screen.Conversation(host, instance, session))
    }

    /** Asks to show notifications, once there is a host to send them. */
    private fun askForNotifications(app: LoomApplication) {
        if (!app.push.available) return
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED) return
        val paired = app.core.getOrNull()?.hosts()?.isNotEmpty() ?: false
        if (paired) askNotifications.launch(Manifest.permission.POST_NOTIFICATIONS)
    }
}
