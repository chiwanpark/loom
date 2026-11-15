package com.chiwanpark.loom

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat

/**
 * Keeps the app running while it forwards ports, so the browser and other
 * apps reach them while Loom is not on screen: a foreground service with a
 * notification that lists the ports and stops them.
 */
class Forwarding(private val app: LoomApplication) {
    /** The forwarded local ports, as the notification lists them. */
    fun summary(): List<String> = app.links.all.flatMap { link ->
        link.forwards.mapNotNull { forward -> forward.localPort?.let { "localhost:$it" } }
    }

    private fun any(): Boolean = app.links.all.any { it.forwards.isNotEmpty() }

    /** Starts, updates, or stops the service after the forwards changed. */
    fun update() {
        val intent = Intent(app, ForwardingService::class.java)
        if (any()) {
            app.startForegroundService(intent)
        } else {
            app.stopService(intent)
        }
    }
}

class ForwardingService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val app = application as LoomApplication
        if (intent?.action == ACTION_STOP) {
            app.links.stopForwards()
            stopSelf()
            return START_NOT_STICKY
        }
        ServiceCompat.startForeground(
            this,
            NOTIFICATION_ID,
            notification(app),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
        )
        return START_NOT_STICKY
    }

    private fun notification(app: LoomApplication): Notification {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL, "Forwarded ports", NotificationManager.IMPORTANCE_LOW),
        )
        val ports = app.forwarding.summary()
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val stop = PendingIntent.getService(
            this,
            1,
            Intent(this, ForwardingService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        return NotificationCompat.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_launcher_monochrome)
            .setContentTitle(if (ports.size == 1) "Forwarding a port" else "Forwarding ${ports.size} ports")
            .setContentText(ports.joinToString(", ").ifEmpty { "Starting…" })
            .setContentIntent(open)
            .setOngoing(true)
            .setSilent(true)
            .addAction(0, "Stop", stop)
            .build()
    }

    private companion object {
        const val CHANNEL = "forwarding"
        const val NOTIFICATION_ID = 7001
        const val ACTION_STOP = "com.chiwanpark.loom.STOP_FORWARDING"
    }
}
