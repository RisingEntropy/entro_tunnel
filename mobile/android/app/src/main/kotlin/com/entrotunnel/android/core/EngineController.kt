package com.entrotunnel.android.core

import android.content.Context
import android.content.Intent
import androidx.core.content.ContextCompat
import com.entrotunnel.android.vpn.EntroVpnService
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/**
 * Drives the native core and exposes a polled [state]. Packet modes
 * (global/VPN) go through [EntroVpnService]; HTTP-proxy mode talks to the core
 * directly (no system VPN).
 */
object EngineController {

    data class UiState(
        val running: Boolean = false,
        val assignedIp: String? = null,
        val peers: List<Peer> = emptyList(),
        val error: String? = null,
        val logs: List<String> = emptyList(),
    )

    private val _state = MutableStateFlow(UiState())
    val state: StateFlow<UiState> = _state

    private val scope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    private var pollJob: Job? = null

    fun startPolling() {
        if (pollJob?.isActive == true) return
        pollJob = scope.launch {
            while (isActive) {
                val st = runCatching { ETJson.decodeFromString<NativeStatus>(Native.nativeStatus()) }.getOrNull()
                val logs = runCatching { Native.nativeLogs() }.getOrDefault("")
                    .split('\n').filter { it.isNotBlank() }
                _state.value = if (st != null) {
                    UiState(st.running, st.assignedIp, st.peers, st.error, logs)
                } else {
                    _state.value.copy(logs = logs)
                }
                delay(1000)
            }
        }
    }

    /** Packet modes — start the foreground VpnService (it does the handshake,
     *  builds the VPN, and hands the fd to the core). */
    fun startVpn(context: Context, profileJson: String, settingsJson: String) {
        val intent = Intent(context, EntroVpnService::class.java).apply {
            action = EntroVpnService.ACTION_START
            putExtra(EntroVpnService.EXTRA_PROFILE, profileJson)
            putExtra(EntroVpnService.EXTRA_SETTINGS, settingsJson)
        }
        ContextCompat.startForegroundService(context, intent)
    }

    /** HTTP-proxy mode — no system VPN; the core runs a local proxy. */
    fun startProxy(profileJson: String, settingsJson: String) {
        scope.launch { Native.nativeConnect(profileJson, settingsJson) }
    }

    fun stop(context: Context) {
        Native.nativeStop()
        context.stopService(Intent(context, EntroVpnService::class.java))
        _state.value = _state.value.copy(running = false)
    }
}
