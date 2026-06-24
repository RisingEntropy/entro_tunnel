package com.entrotunnel.android.vpn

import android.app.Notification
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.VpnService
import android.os.Build
import android.os.ParcelFileDescriptor
import androidx.core.app.NotificationCompat
import com.entrotunnel.android.App
import com.entrotunnel.android.MainActivity
import com.entrotunnel.android.core.ETJson
import com.entrotunnel.android.core.NetConfig
import com.entrotunnel.android.core.Native
import com.entrotunnel.android.core.RouteRule
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import java.net.Inet4Address
import java.net.InetAddress

/**
 * The system VPN. The OS gives us a TUN fd after the user approves the consent
 * dialog; the Rust core reads/writes packets on it. We set the address / routes /
 * DNS on the [Builder] — no `ip`/`route` commands (Android forbids them).
 *
 * Flow: `nativeConnect` (handshake → network config) → build the VPN with the
 * server-assigned IP → `establish()` → hand the fd to `nativeStartBridge`.
 */
class EntroVpnService : VpnService() {

    private var tun: ParcelFileDescriptor? = null
    private val scope = CoroutineScope(Dispatchers.IO + SupervisorJob())

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        val profileJson = intent?.getStringExtra(EXTRA_PROFILE)
        val settingsJson = intent?.getStringExtra(EXTRA_SETTINGS)
        if (profileJson == null || settingsJson == null) {
            stopSelf()
            return START_NOT_STICKY
        }
        startForegroundCompat(notification("Connecting…"))
        scope.launch { connect(profileJson, settingsJson) }
        return START_STICKY
    }

    private fun connect(profileJson: String, settingsJson: String) {
        val raw = Native.nativeConnect(profileJson, settingsJson)
        val cfg = runCatching { ETJson.decodeFromString<NetConfig>(raw) }.getOrNull()
        if (cfg == null || cfg.error != null) {
            updateNotification("Failed: ${cfg?.error ?: "bad native response"}")
            stopSelf()
            return
        }

        val ip = cfg.assignedIp ?: "10.66.0.2"
        val builder = Builder()
            .setSession("EntroTunnel")
            .setMtu(cfg.mtu)
            .addAddress(ip, cfg.prefixLen)

        if (cfg.mode == "vpn") {
            // VPN-LAN: route only the virtual subnet through the tunnel.
            builder.addRoute(networkAddress(ip, cfg.prefixLen), cfg.prefixLen)
        } else {
            // Global proxy: capture all traffic.
            builder.addRoute("0.0.0.0", 0)
            builder.addRoute("::", 0) // sink IPv6 so it doesn't leak around the tunnel
        }
        cfg.dns.forEach { runCatching { builder.addDnsServer(it) } }

        // Exclude ourselves so the engine's socket to the server bypasses the VPN
        // (otherwise the tunnel's carrier traffic would loop).
        runCatching { builder.addDisallowedApplication(packageName) }
        builder.setBlocking(false)

        val pfd = builder.establish()
        if (pfd == null) {
            updateNotification("Failed: VpnService.establish() returned null")
            stopSelf()
            return
        }
        tun = pfd
        val err = Native.nativeStartBridge(pfd.fd)
        if (err.isNotEmpty()) {
            updateNotification("Failed: $err")
            stopSelf()
            return
        }
        updateNotification("Connected · $ip")
    }

    override fun onRevoke() {
        // Another VPN took over, or the user revoked consent.
        stopSelf()
    }

    override fun onDestroy() {
        Native.nativeStop()
        runCatching { tun?.close() }
        tun = null
        scope.cancel()
        super.onDestroy()
    }

    // ---- notification ----

    private fun notification(text: String): Notification {
        val open = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return NotificationCompat.Builder(this, App.NOTIF_CHANNEL)
            .setContentTitle("EntroTunnel")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.ic_lock_lock)
            .setOngoing(true)
            .setContentIntent(open)
            .build()
    }

    private fun updateNotification(text: String) {
        val nm = getSystemService(NotificationManager::class.java)
        nm?.notify(App.NOTIF_ID, notification(text))
    }

    private fun startForegroundCompat(n: Notification) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(App.NOTIF_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(App.NOTIF_ID, n)
        }
    }

    companion object {
        const val ACTION_START = "com.entrotunnel.android.action.START"
        const val ACTION_STOP = "com.entrotunnel.android.action.STOP"
        const val EXTRA_PROFILE = "profile_json"
        const val EXTRA_SETTINGS = "settings_json"

        /** IPv4 network address for a host IP + prefix (for the VPN-LAN route). */
        fun networkAddress(ip: String, prefix: Int): String {
            val addr = (InetAddress.getByName(ip) as? Inet4Address)?.address ?: return ip
            val mask = if (prefix == 0) 0 else (-0x1 shl (32 - prefix))
            val v = ((addr[0].toInt() and 0xff) shl 24) or
                ((addr[1].toInt() and 0xff) shl 16) or
                ((addr[2].toInt() and 0xff) shl 8) or
                (addr[3].toInt() and 0xff)
            val net = v and mask
            return "${(net ushr 24) and 0xff}.${(net ushr 16) and 0xff}.${(net ushr 8) and 0xff}.${net and 0xff}"
        }
    }
}
