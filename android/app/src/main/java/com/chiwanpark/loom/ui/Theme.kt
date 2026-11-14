package com.chiwanpark.loom.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext

/** Material 3 in the colors of the user's wallpaper. */
@Composable
fun LoomTheme(content: @Composable () -> Unit) {
    val context = LocalContext.current
    val colors = if (isSystemInDarkTheme()) {
        dynamicDarkColorScheme(context)
    } else {
        dynamicLightColorScheme(context)
    }
    MaterialTheme(colorScheme = colors, content = content)
}

/** Colors that Material 3 has no role for: success and warning, as the desktop's themes have. */
object LoomColors {
    val success: Color
        @Composable get() = if (isSystemInDarkTheme()) Color(0xFF81C995) else Color(0xFF1E8E3E)
    val warning: Color
        @Composable get() = if (isSystemInDarkTheme()) Color(0xFFFDD663) else Color(0xFFB06000)
    val hunk: Color
        @Composable get() = if (isSystemInDarkTheme()) Color(0xFF78D9EC) else Color(0xFF007B83)
}
