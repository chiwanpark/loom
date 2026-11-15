package com.chiwanpark.loom

import android.content.Context
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/** The user's preferences for the app, kept on the device. */
class Settings(context: Context) {
    private val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)

    /** How much larger or smaller the app's text is than the system's text size. */
    var textScale by mutableFloatStateOf(prefs.getFloat(TEXT_SCALE, 1f).takeIf { it in TEXT_SCALES } ?: 1f)
        private set

    /**
     * How much larger or smaller the text in blocks is than the app's other
     * text: tool calls' arguments and output, and the reasoning of answers.
     */
    var blockScale by mutableFloatStateOf(prefs.getFloat(BLOCK_SCALE, 1f).takeIf { it in BLOCK_SCALES } ?: 1f)
        private set

    /** The font of the app's text, by its id in `LoomFonts.textFonts`. */
    var textFont by mutableStateOf(prefs.getString(TEXT_FONT, null) ?: DEFAULT_TEXT_FONT)
        private set

    /** The font of code, commands, and their output, by its id in `LoomFonts.codeFonts`. */
    var codeFont by mutableStateOf(prefs.getString(CODE_FONT, null) ?: DEFAULT_CODE_FONT)
        private set

    /** Whether long lines of files and diffs wrap instead of scrolling sideways. */
    var wrapCode by mutableStateOf(prefs.getBoolean(WRAP_CODE, false))
        private set

    /** The text size of terminals, in sp; pinching a terminal changes it. */
    var terminalTextSize by mutableFloatStateOf(prefs.getFloat(TERMINAL_TEXT_SIZE, 12f).coerceIn(TERMINAL_TEXT_SIZES))
        private set

    fun updateTextScale(scale: Float) {
        textScale = scale
        prefs.edit().putFloat(TEXT_SCALE, scale).apply()
    }

    fun updateBlockScale(scale: Float) {
        blockScale = scale
        prefs.edit().putFloat(BLOCK_SCALE, scale).apply()
    }

    fun updateTextFont(id: String) {
        textFont = id
        prefs.edit().putString(TEXT_FONT, id).apply()
    }

    fun updateCodeFont(id: String) {
        codeFont = id
        prefs.edit().putString(CODE_FONT, id).apply()
    }

    fun updateWrapCode(wrap: Boolean) {
        wrapCode = wrap
        prefs.edit().putBoolean(WRAP_CODE, wrap).apply()
    }

    fun updateTerminalTextSize(size: Float) {
        terminalTextSize = size.coerceIn(TERMINAL_TEXT_SIZES)
        prefs.edit().putFloat(TERMINAL_TEXT_SIZE, terminalTextSize).apply()
    }

    companion object {
        private const val TEXT_SCALE = "text_scale"
        private const val BLOCK_SCALE = "block_scale"
        private const val TEXT_FONT = "text_font"
        private const val CODE_FONT = "code_font"
        private const val WRAP_CODE = "wrap_code"
        private const val TERMINAL_TEXT_SIZE = "terminal_text_size"

        const val DEFAULT_TEXT_FONT = "system"
        const val DEFAULT_CODE_FONT = "jetbrains-mono"

        /** The text sizes to choose from, as factors of the system's text size. */
        val TEXT_SCALES = listOf(0.8f, 0.9f, 1f, 1.1f, 1.2f, 1.3f)

        /** The sizes of text in blocks to choose from, as factors of the app's text size. */
        val BLOCK_SCALES = listOf(0.7f, 0.8f, 0.9f, 1f, 1.1f, 1.2f, 1.3f)

        /** The range of terminal text sizes, in sp. */
        val TERMINAL_TEXT_SIZES = 6f..24f
    }
}
