package com.entrotunnel.android.core

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

/** Shared JSON config — must match the Rust serde field names exactly. */
val ETJson = Json {
    ignoreUnknownKeys = true
    encodeDefaults = true
    explicitNulls = false
}

@Serializable
data class ServerEntry(
    val name: String,
    val host: String,
    val port: Int,
    val transport: String = "tcp", // tcp | ws | quic
    val token: String = "",
    @SerialName("noise_psk") val noisePsk: String = "",
    @SerialName("tls_skip_verify") val tlsSkipVerify: Boolean = false,
    @SerialName("server_name") val serverName: String? = null,
)

/** A profile is *server config only* — no mode (that is a local choice). */
@Serializable
data class Profile(
    val name: String,
    @SerialName("selected_server") val selectedServer: String? = null,
    val servers: List<ServerEntry> = emptyList(),
) {
    fun activeServer(): ServerEntry? =
        servers.firstOrNull { it.name == selectedServer } ?: servers.firstOrNull()
}

@Serializable
data class RouteRule(
    val target: String,
    val via: String, // tunnel | direct | <nic>
    val gateway: String? = null,
)

/** Device-local connection settings (the "local connection" choices). */
@Serializable
data class ConnectionSettings(
    val mode: String = "global_proxy", // global_proxy | system_proxy | http_proxy | vpn
    @SerialName("requested_ip") val requestedIp: String? = null,
    @SerialName("client_name") val clientName: String? = null,
    @SerialName("tun_name") val tunName: String = "et0",
    @SerialName("http_listen") val httpListen: String = "127.0.0.1:7890",
    @SerialName("join_vpn") val joinVpn: Boolean = false,
    val routes: List<RouteRule> = emptyList(),
    @SerialName("split_mode") val splitMode: String = "blacklist", // blacklist | whitelist
    val chain: List<String> = emptyList(),
)

/** Result of [Native.nativeConnect]: the VpnService.Builder config (or error). */
@Serializable
data class NetConfig(
    val mode: String? = null,
    @SerialName("assigned_ip") val assignedIp: String? = null,
    @SerialName("prefix_len") val prefixLen: Int = 32,
    val gateway: String? = null,
    val mtu: Int = 1380,
    val dns: List<String> = emptyList(),
    val error: String? = null,
)

@Serializable
data class Peer(val ip: String, val name: String = "")

@Serializable
data class NativeStatus(
    val running: Boolean = false,
    @SerialName("assigned_ip") val assignedIp: String? = null,
    val peers: List<Peer> = emptyList(),
    val error: String? = null,
)

val MODES = listOf("global_proxy", "system_proxy", "http_proxy", "vpn")

fun modeLabel(mode: String): String = when (mode) {
    "global_proxy" -> "Global proxy (VPN)"
    "system_proxy" -> "System proxy"
    "http_proxy" -> "HTTP proxy"
    "vpn" -> "VPN (virtual LAN)"
    else -> mode
}

/** Packet modes go through the system VPN (VpnService); others don't. */
fun isVpnMode(mode: String): Boolean = mode == "global_proxy" || mode == "vpn"
