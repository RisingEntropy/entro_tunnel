package com.entrotunnel.android.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.entrotunnel.android.MainActivity
import com.entrotunnel.android.core.*
import com.entrotunnel.android.data.LocalState
import com.entrotunnel.android.data.ProfileStore

private enum class Tab(val label: String, val icon: androidx.compose.ui.graphics.vector.ImageVector) {
    Home("Home", Icons.Filled.Home),
    Profiles("Profiles", Icons.Filled.List),
    Logs("Logs", Icons.Filled.Article),
}

@Composable
fun AppRoot(
    store: ProfileStore,
    onConnect: (Profile, ConnectionSettings) -> Unit,
    onDisconnect: () -> Unit,
) {
    var profiles by remember { mutableStateOf(store.loadProfiles()) }
    var local by remember { mutableStateOf(store.loadState()) }
    var tab by remember { mutableStateOf(Tab.Home) }
    val status by EngineController.state.collectAsStateWithLifecycle()

    fun reload() {
        profiles = store.loadProfiles()
        local = store.loadState()
    }
    fun updateState(s: LocalState) {
        local = s
        store.saveState(s)
    }
    // Server choice lives on the Profile (selected_server), like the desktop GUI.
    fun selectServer(profile: Profile, serverName: String) {
        store.upsert(profile.copy(selectedServer = serverName))
        profiles = store.loadProfiles()
    }

    Scaffold(
        bottomBar = {
            NavigationBar {
                Tab.entries.forEach { t ->
                    NavigationBarItem(
                        selected = tab == t,
                        onClick = { tab = t },
                        icon = { Icon(t.icon, t.label) },
                        label = { Text(t.label) },
                    )
                }
            }
        }
    ) { pad ->
        Box(Modifier.padding(pad)) {
            when (tab) {
                Tab.Home -> HomeTab(profiles, local, status, ::updateState, ::selectServer, onConnect, onDisconnect)
                Tab.Profiles -> ProfilesTab(profiles, store, ::reload)
                Tab.Logs -> LogsTab(status.logs)
            }
        }
    }
}

@Composable
private fun HomeTab(
    profiles: List<Profile>,
    local: LocalState,
    status: EngineController.UiState,
    onUpdate: (LocalState) -> Unit,
    onSelectServer: (Profile, String) -> Unit,
    onConnect: (Profile, ConnectionSettings) -> Unit,
    onDisconnect: () -> Unit,
) {
    val s = local.settings
    val active = profiles.firstOrNull { it.name == local.activeProfile } ?: profiles.firstOrNull()
    val running = status.running
    val chain = s.chain

    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        status.error?.let { Card { Text(it, Modifier.padding(12.dp), color = MaterialTheme.colorScheme.error) } }

        // Connection hero.
        Card {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text(
                    if (running) "Connected" else if (active != null) "Ready" else "No profile",
                    style = MaterialTheme.typography.headlineSmall,
                )
                Text(
                    when {
                        active == null -> "Create or import a profile"
                        chain.size >= 2 -> "${modeLabel(s.mode)} · ${active.name} · chain (${chain.size} hops)"
                        else -> "${modeLabel(s.mode)} · ${active.name} · ${active.activeServer()?.name ?: "—"}"
                    },
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                status.assignedIp?.takeIf { running }?.let {
                    Text("virtual IP  $it", fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall)
                }
                Spacer(Modifier.height(8.dp))
                Button(
                    onClick = { if (running) onDisconnect() else active?.let { onConnect(it, s) } },
                    enabled = active != null,
                    modifier = Modifier.fillMaxWidth(),
                    colors = if (running) ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.secondary) else ButtonDefaults.buttonColors(),
                ) { Text(if (running) "Disconnect" else "Connect") }
            }
        }

        // Profile / mode pickers.
        Card {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(10.dp)) {
                Text("Connection", style = MaterialTheme.typography.labelLarge)
                Dropdown("Profile", active?.name ?: "—", profiles.map { it.name }, enabled = !running) { name ->
                    onUpdate(local.copy(activeProfile = name))
                }
                val servers = active?.servers ?: emptyList()
                Dropdown(
                    "Server",
                    active?.activeServer()?.name ?: "—",
                    servers.map { it.name },
                    enabled = !running && servers.isNotEmpty(),
                ) { name ->
                    active?.let { onSelectServer(it, name) }
                }
                if (chain.size >= 2) {
                    Text(
                        "A proxy chain is set below — it overrides this single server while active.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Dropdown("Mode", modeLabel(s.mode), MODES.map { modeLabel(it) }, enabled = !running) { label ->
                    val m = MODES.first { modeLabel(it) == label }
                    onUpdate(local.copy(settings = s.copy(mode = m)))
                }
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Checkbox(
                        checked = s.mode == "vpn" || s.joinVpn,
                        enabled = !running && s.mode != "vpn",
                        onCheckedChange = { onUpdate(local.copy(settings = s.copy(joinVpn = it))) },
                    )
                    Text("Join this server's VPN LAN")
                }
            }
        }

        // Proxy chain (per connection).
        Card {
            Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("Proxy chain", style = MaterialTheme.typography.labelLarge)
                Text(
                    if (chain.size >= 2) "you → ${chain.joinToString(" → ")} → internet"
                    else "Optional. Relay through 2+ servers; the mode runs at the last hop.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                val servers = active?.servers ?: emptyList()
                chain.forEachIndexed { i, hop ->
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        Text("${i + 1}")
                        Box(Modifier.weight(1f)) {
                            Dropdown("", hop, servers.map { it.name }, enabled = !running) { name ->
                                onUpdate(local.copy(settings = s.copy(chain = chain.toMutableList().also { it[i] = name })))
                            }
                        }
                        IconButton(enabled = !running, onClick = {
                            onUpdate(local.copy(settings = s.copy(chain = chain.filterIndexed { idx, _ -> idx != i })))
                        }) { Icon(Icons.Filled.Close, "remove") }
                    }
                }
                TextButton(enabled = !running && servers.isNotEmpty(), onClick = {
                    onUpdate(local.copy(settings = s.copy(chain = chain + (servers.firstOrNull()?.name ?: ""))))
                }) { Text("+ Add hop") }
            }
        }

        // VPN peers.
        if (running && (s.mode == "vpn" || s.joinVpn) && status.peers.isNotEmpty()) {
            Card {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    Text("VPN peers (${status.peers.size})", style = MaterialTheme.typography.labelLarge)
                    status.peers.forEach {
                        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                            Text(it.name.ifEmpty { "—" })
                            Text(it.ip, fontFamily = FontFamily.Monospace)
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun ProfilesTab(profiles: List<Profile>, store: ProfileStore, onChange: () -> Unit) {
    var importing by remember { mutableStateOf(false) }
    Column(Modifier.fillMaxSize().padding(16.dp)) {
        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
            Text("Profiles", style = MaterialTheme.typography.titleLarge)
            Button(onClick = { importing = true }) { Text("Import") }
        }
        Spacer(Modifier.height(12.dp))
        if (profiles.isEmpty()) {
            Text("No profiles yet. Tap Import and paste an entro:// link.", color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        LazyColumn(verticalArrangement = Arrangement.spacedBy(8.dp)) {
            items(profiles, key = { it.name }) { p ->
                Card {
                    Row(Modifier.fillMaxWidth().padding(14.dp), verticalAlignment = Alignment.CenterVertically) {
                        Column(Modifier.weight(1f)) {
                            Text(p.name, style = MaterialTheme.typography.titleMedium)
                            Text("${p.servers.size} server(s)", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                        }
                        IconButton(onClick = { store.remove(p.name); onChange() }) {
                            Icon(Icons.Filled.Delete, "delete")
                        }
                    }
                }
            }
        }
    }
    if (importing) {
        ImportDialog(onClose = { importing = false }, onImport = { link ->
            MainActivity.decodeEntroLink(link)?.let { store.upsert(it); onChange() }
            importing = false
        })
    }
}

@Composable
private fun ImportDialog(onClose: () -> Unit, onImport: (String) -> Unit) {
    var text by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onClose,
        title = { Text("Import profile") },
        text = {
            Column {
                Text("Paste an entro:// link (from the server admin / desktop Export).", style = MaterialTheme.typography.bodySmall)
                Spacer(Modifier.height(8.dp))
                OutlinedTextField(value = text, onValueChange = { text = it }, placeholder = { Text("entro://…") }, modifier = Modifier.fillMaxWidth())
            }
        },
        confirmButton = { TextButton(onClick = { onImport(text) }, enabled = text.isNotBlank()) { Text("Import") } },
        dismissButton = { TextButton(onClick = onClose) { Text("Cancel") } },
    )
}

@Composable
private fun LogsTab(logs: List<String>) {
    LazyColumn(Modifier.fillMaxSize().padding(12.dp), reverseLayout = true) {
        items(logs.reversed()) { line ->
            Text(line, fontFamily = FontFamily.Monospace, style = MaterialTheme.typography.bodySmall, maxLines = 4, overflow = TextOverflow.Ellipsis)
        }
        if (logs.isEmpty()) item { Text("No logs yet. Connect to see engine activity.", color = MaterialTheme.colorScheme.onSurfaceVariant) }
    }
}

/** Minimal labelled dropdown (a button that opens a menu). */
@Composable
private fun Dropdown(label: String, value: String, options: List<String>, enabled: Boolean, onPick: (String) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Column {
        if (label.isNotEmpty()) Text(label, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Box {
            OutlinedButton(onClick = { open = true }, enabled = enabled, modifier = Modifier.fillMaxWidth()) {
                Text(value.ifEmpty { "—" }, Modifier.weight(1f), maxLines = 1, overflow = TextOverflow.Ellipsis)
                Icon(Icons.Filled.ArrowDropDown, null)
            }
            DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
                options.forEach { o ->
                    DropdownMenuItem(text = { Text(o) }, onClick = { onPick(o); open = false })
                }
            }
        }
    }
}
