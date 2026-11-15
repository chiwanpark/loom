package com.chiwanpark.loom.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.runtime.remember
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.isSpecified
import com.chiwanpark.loom.Settings

/**
 * Material 3 in the colors of the user's wallpaper, with text `textScale`
 * times the system's text size and text in blocks `blockScale` times that,
 * in the fonts with these ids.
 */
@Composable
fun LoomTheme(
    textScale: Float = 1f,
    blockScale: Float = 1f,
    textFont: String = Settings.DEFAULT_TEXT_FONT,
    codeFont: String = Settings.DEFAULT_CODE_FONT,
    content: @Composable () -> Unit,
) {
    val context = LocalContext.current
    val colors = if (isSystemInDarkTheme()) {
        dynamicDarkColorScheme(context)
    } else {
        dynamicLightColorScheme(context)
    }
    val density = LocalDensity.current
    val catalog = remember(context) { FontCatalog.get(context.applicationContext) }
    val typography = remember(textFont) {
        Typography().withFontFamily(LoomFonts.textFont(textFont, catalog).family)
    }
    CompositionLocalProvider(
        LocalDensity provides Density(density.density, density.fontScale * textScale),
        LocalCodeFont provides (LoomFonts.codeFont(codeFont, catalog).family ?: FontFamily.Monospace),
        LocalBlockScale provides blockScale,
    ) {
        MaterialTheme(colorScheme = colors, typography = typography, content = content)
    }
}

/** How much larger or smaller text in blocks is than the app's other text. */
val LocalBlockScale = staticCompositionLocalOf { 1f }

/**
 * `style` for the text in a block, such as a tool call's output or an
 * answer's reasoning: its size and line height scaled as the user set.
 */
@Composable
@ReadOnlyComposable
fun blockStyle(style: TextStyle): TextStyle {
    val scale = LocalBlockScale.current
    if (scale == 1f) return style
    return style.copy(
        fontSize = if (style.fontSize.isSpecified) style.fontSize * scale else style.fontSize,
        lineHeight = if (style.lineHeight.isSpecified) style.lineHeight * scale else style.lineHeight,
    )
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
