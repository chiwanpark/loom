package com.chiwanpark.loom

import android.Manifest
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.content.pm.PackageManager
import android.util.Log
import com.chiwanpark.loom.core.LoomException
import com.chiwanpark.loom.core.PushKind
import com.chiwanpark.loom.core.PushNotice
import com.google.firebase.FirebaseApp
import com.google.firebase.messaging.FirebaseMessaging
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking

private const val TAG = "LoomPush"

/**
 * Notifications from agents, through Firebase Cloud Messaging: the app tells
 * each paired host its FCM token, and the host's agents send sealed notices
 * through their relay when a tool call waits for approval or a turn ends.
 * Builds without a Firebase project (see build.gradle.kts) have none.
 */
class Push(private val app: LoomApplication) {
    /** Whether this build has a Firebase project to receive notifications with. */
    val available: Boolean by lazy { FirebaseApp.getApps(app).isNotEmpty() }

    private var token: String? = null

    /** The token each host (by name) was told, and hosts being told now. */
    private val told = mutableMapOf<String, String>()
    private val telling = mutableSetOf<String>()

    fun start() {
        createChannels()
        if (!available) return
        FirebaseMessaging.getInstance().token
            .addOnSuccessListener { token -> app.scope.launch { tokenChanged(token) } }
            .addOnFailureListener { e -> Log.w(TAG, "no FCM token", e) }
    }

    /** FCM gave the app a token, maybe a new one: the hosts hear of it again. */
    fun tokenChanged(newToken: String) {
        if (newToken == token) return
        token = newToken
        told.clear()
        app.links.all.forEach(::linked)
    }

    /**
     * A link is up: tells its host where to send this device's notifications,
     * unless it knows. The token belongs to this device's key on the host,
     * so one agent of a host is enough.
     */
    fun linked(link: AgentLink) {
        val current = token ?: return
        if (link.state != LinkState.Connected) return
        val host = link.host
        if (told[host] == current || !telling.add(host)) return
        app.scope.launch {
            try {
                link.connection().setPushToken(current)
                told[host] = current
            } catch (e: LoomException) {
                Log.w(TAG, "could not register for notifications with $host: ${e.userMessage()}")
            } finally {
                telling.remove(host)
            }
        }
    }

    /** Tells the hosts again, as after pairing, which makes a host forget the token. */
    fun tellAgain() {
        told.clear()
        app.links.all.forEach(::linked)
    }

    private fun createChannels() {
        val manager = app.getSystemService(NotificationManager::class.java) ?: return
        manager.createNotificationChannels(
            listOf(
                NotificationChannel(CHANNEL_APPROVALS, "Approvals", NotificationManager.IMPORTANCE_HIGH)
                    .apply { description = "A tool call waits for your approval." },
                NotificationChannel(CHANNEL_FINISHED, "Finished turns", NotificationManager.IMPORTANCE_DEFAULT)
                    .apply { description = "An agent finished answering." },
                NotificationChannel(CHANNEL_FAILED, "Failed turns", NotificationManager.IMPORTANCE_HIGH)
                    .apply { description = "An agent's turn failed." },
            ),
        )
    }

    /** Shows a notice, unless its session is on screen. */
    fun show(notice: PushNotice) {
        if (onScreen(notice.host, notice.session)) return
        if (app.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) return
        val manager = app.getSystemService(NotificationManager::class.java) ?: return
        val title = notice.title?.takeIf { it.isNotBlank() } ?: "Session"
        val (channel, text) = when (notice.kind) {
            PushKind.APPROVAL -> CHANNEL_APPROVALS to "Waiting for approval" + (notice.detail?.let { ": $it" } ?: "")
            PushKind.FINISHED -> CHANNEL_FINISHED to (notice.detail ?: "Finished.")
            PushKind.FAILED -> CHANNEL_FAILED to "Failed" + (notice.detail?.let { ": $it" } ?: "")
        }
        val workspace = notice.workspace.trimEnd('/').substringAfterLast('/')
        val open = Intent(app, MainActivity::class.java).apply {
            action = ACTION_OPEN_SESSION
            putExtra(EXTRA_HOST, notice.host)
            putExtra(EXTRA_INSTANCE, notice.instance)
            putExtra(EXTRA_WORKSPACE, notice.workspace)
            putExtra(EXTRA_SESSION, notice.session)
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP
        }
        val pending = PendingIntent.getActivity(
            app,
            tag(notice.host, notice.session).hashCode(),
            open,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = android.app.Notification.Builder(app, channel)
            .setSmallIcon(R.drawable.ic_launcher_monochrome)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(android.app.Notification.BigTextStyle().bigText(text))
            .setSubText(if (app.directory.hosts.size > 1) "$workspace · ${notice.host}" else workspace)
            .setWhen(notice.at.toLong() * 1000)
            .setShowWhen(true)
            .setCategory(
                if (notice.kind == PushKind.APPROVAL) {
                    android.app.Notification.CATEGORY_REMINDER
                } else {
                    android.app.Notification.CATEGORY_STATUS
                },
            )
            .setContentIntent(pending)
            .setAutoCancel(true)
            .build()
        // One notification a session: a later notice replaces the earlier one.
        manager.notify(tag(notice.host, notice.session), NOTIFICATION_ID, notification)
    }

    /** Takes down a session's notification, as when it is opened. */
    fun clear(host: String, session: String) {
        app.getSystemService(NotificationManager::class.java)?.cancel(tag(host, session), NOTIFICATION_ID)
    }

    private fun onScreen(host: String, session: String): Boolean {
        if (!app.foreground.value) return false
        val screen = app.navigator.stack.lastOrNull() as? Screen.Conversation ?: return false
        return screen.host == host && screen.session == session
    }

    private fun tag(host: String, session: String) = "$host/$session"

    companion object {
        const val ACTION_OPEN_SESSION = "com.chiwanpark.loom.OPEN_SESSION"
        const val EXTRA_HOST = "host"
        const val EXTRA_INSTANCE = "instance"
        const val EXTRA_WORKSPACE = "workspace"
        const val EXTRA_SESSION = "session"
        private const val CHANNEL_APPROVALS = "approvals"
        private const val CHANNEL_FINISHED = "finished"
        private const val CHANNEL_FAILED = "failed"
        private const val NOTIFICATION_ID = 1
    }
}

/** Receives FCM's messages and tokens. */
class LoomMessagingService : FirebaseMessagingService() {
    private val app get() = application as LoomApplication

    override fun onNewToken(token: String) {
        app.scope.launch { app.push.tokenChanged(token) }
    }

    /** A sealed notice from an agent: `h` is its host's ID, `p` the notice. */
    override fun onMessageReceived(message: RemoteMessage) {
        val host = message.data["h"] ?: return
        val payload = message.data["p"] ?: return
        val loom = app.core.getOrNull() ?: return
        val notice = try {
            loom.openPush(host, payload)
        } catch (e: LoomException) {
            Log.w(TAG, "dropped a notification: ${e.userMessage()}")
            return
        }
        // The screens live on the main thread; the notification is up before
        // this returns and the system may end the process.
        runBlocking(Dispatchers.Main) { app.push.show(notice) }
    }
}
