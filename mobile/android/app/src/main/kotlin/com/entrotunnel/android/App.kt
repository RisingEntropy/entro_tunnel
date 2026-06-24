package com.entrotunnel.android

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.os.Build

class App : Application() {
    override fun onCreate() {
        super.onCreate()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val ch = NotificationChannel(
                NOTIF_CHANNEL,
                "VPN status",
                NotificationManager.IMPORTANCE_LOW,
            ).apply { description = "EntroTunnel connection status" }
            getSystemService(NotificationManager::class.java).createNotificationChannel(ch)
        }
    }

    companion object {
        const val NOTIF_CHANNEL = "entrotunnel_vpn"
        const val NOTIF_ID = 1001
    }
}
