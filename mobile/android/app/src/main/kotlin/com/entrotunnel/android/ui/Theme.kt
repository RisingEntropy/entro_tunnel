package com.entrotunnel.android.ui

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

private val Colors = darkColorScheme(
    primary = Color(0xFFE8540F),     // wall-breach orange (matches the logo)
    onPrimary = Color(0xFF160B05),
    secondary = Color(0xFFC9430B),
    background = Color(0xFF161311),
    onBackground = Color(0xFFEDE7E3),
    surface = Color(0xFF1C1815),
    onSurface = Color(0xFFEDE7E3),
    surfaceVariant = Color(0xFF2A231E),
    outline = Color(0xFF4A3F38),
)

@Composable
fun EntroTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = Colors, content = content)
}
