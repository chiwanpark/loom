package com.chiwanpark.loom

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import com.chiwanpark.loom.ui.LoomRoot
import com.chiwanpark.loom.ui.LoomTheme

class MainActivity : ComponentActivity() {
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
            ) {
                LoomRoot(app)
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        openLink(intent)
    }

    /** A pairing link opened from outside the app goes to the pairing screen. */
    private fun openLink(intent: Intent?) {
        val link = intent?.takeIf { it.action == Intent.ACTION_VIEW }?.dataString ?: return
        (application as LoomApplication).navigator.push(Screen.Pair(link))
    }
}
