package com.entrotunnel.android

import android.Manifest
import android.content.Intent
import android.net.VpnService
import android.os.Build
import android.os.Bundle
import android.util.Base64
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts.RequestPermission
import androidx.activity.result.contract.ActivityResultContracts.StartActivityForResult
import com.entrotunnel.android.core.ConnectionSettings
import com.entrotunnel.android.core.ETJson
import com.entrotunnel.android.core.EngineController
import com.entrotunnel.android.core.Profile
import com.entrotunnel.android.core.isVpnMode
import com.entrotunnel.android.data.ProfileStore
import com.entrotunnel.android.ui.AppRoot
import com.entrotunnel.android.ui.EntroTheme
import kotlinx.serialization.encodeToString

class MainActivity : ComponentActivity() {

    lateinit var store: ProfileStore
        private set

    private var pendingStart: (() -> Unit)? = null
    private lateinit var vpnConsent: ActivityResultLauncher<Intent>
    private lateinit var notifPermission: ActivityResultLauncher<String>

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        store = ProfileStore(this)

        vpnConsent = registerForActivityResult(StartActivityForResult()) { res ->
            if (res.resultCode == RESULT_OK) pendingStart?.invoke()
            pendingStart = null
        }
        notifPermission = registerForActivityResult(RequestPermission()) { }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            notifPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
        }

        EngineController.startPolling()
        setContent {
            EntroTheme {
                AppRoot(store = store, onConnect = ::connect, onDisconnect = ::disconnect)
            }
        }
    }

    /** Start a session. Packet modes go through the system-VPN consent flow. */
    fun connect(profile: Profile, settings: ConnectionSettings) {
        val profileJson = ETJson.encodeToString(profile)
        val settingsJson = ETJson.encodeToString(settings)
        if (isVpnMode(settings.mode)) {
            val start = { EngineController.startVpn(this, profileJson, settingsJson) }
            val prep = VpnService.prepare(this)
            if (prep != null) {
                pendingStart = start
                vpnConsent.launch(prep)
            } else {
                start()
            }
        } else {
            EngineController.startProxy(profileJson, settingsJson)
        }
    }

    fun disconnect() = EngineController.stop(this)

    companion object {
        /** Decode an `entro://<base64-json>` profile link (best-effort across
         *  the standard and URL-safe base64 alphabets). */
        fun decodeEntroLink(link: String): Profile? {
            val b64 = link.trim().removePrefix("entro://")
            for (flags in intArrayOf(Base64.DEFAULT, Base64.URL_SAFE or Base64.NO_WRAP)) {
                runCatching {
                    val json = String(Base64.decode(b64, flags))
                    return ETJson.decodeFromString<Profile>(json)
                }
            }
            return null
        }
    }
}
