package com.chiwanpark.loom.ui

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import android.text.InputType
import android.util.TypedValue
import android.view.GestureDetector
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import com.chiwanpark.loom.core.KeyModifiers
import com.chiwanpark.loom.core.TerminalColor
import com.chiwanpark.loom.core.TerminalCursorShape
import com.chiwanpark.loom.core.TerminalFrame
import com.chiwanpark.loom.core.TerminalKey
import kotlin.math.abs
import kotlin.math.floor
import kotlin.math.max

/** The colors a terminal is drawn in: the theme's text and background, and the 16 colors programs name. */
data class TerminalPalette(val foreground: Int, val background: Int, val colors: IntArray) {
    fun resolve(color: TerminalColor): Int = when (color) {
        TerminalColor.Foreground -> foreground
        TerminalColor.Background -> background
        is TerminalColor.Palette -> colors[color.index.toInt() and 15]
        is TerminalColor.Rgb -> (0xFF000000 or color.rgb.toLong()).toInt()
    }

    override fun equals(other: Any?): Boolean = other is TerminalPalette && foreground == other.foreground &&
        background == other.background && colors.contentEquals(other.colors)

    override fun hashCode(): Int = (foreground * 31 + background) * 31 + colors.contentHashCode()

    companion object {
        private fun argb(vararg rgb: Long): IntArray = rgb.map { (0xFF000000 or it).toInt() }.toIntArray()

        /** After One Dark, readable on a dark surface. */
        val DARK = argb(
            0x3F4451, 0xE06C75, 0x98C379, 0xE5C07B, 0x61AFEF, 0xC678DD, 0x56B6C2, 0xABB2BF,
            0x5C6370, 0xFF7B86, 0xB5E890, 0xFFD68A, 0x8CC8FF, 0xDFA0F0, 0x7FD4DE, 0xFFFFFF,
        )

        /** After GitHub's light theme, readable on a light surface. */
        val LIGHT = argb(
            0x24292F, 0xCF222E, 0x116329, 0x7D4E00, 0x0969DA, 0x8250DF, 0x1B7C83, 0x6E7781,
            0x57606A, 0xA40E26, 0x1A7F37, 0x633C01, 0x218BFF, 0xA475F9, 0x3192AA, 0x8C959F,
        )
    }
}

/**
 * Draws a terminal's frames and takes the keyboard's input for it: text the
 * keyboard commits, including Hangul it composes (shown at the cursor until
 * then), and keys. Tapping shows the keyboard, dragging scrolls, and pinching
 * changes the text size.
 */
@SuppressLint("ViewConstructor")
class TerminalView(context: Context) : View(context) {
    /** What the view sends to the terminal; set by the screen. */
    interface Listener {
        fun onText(text: String)

        fun onKey(key: TerminalKey, modifiers: KeyModifiers)

        fun onResize(cols: Int, rows: Int)

        fun onScroll(lines: Int)

        fun onTextSize(sp: Float)

        /** The modifiers armed on the screen's extra keys, used up by the next key. */
        fun takeModifiers(): KeyModifiers
    }

    var listener: Listener? = null

    var frame: TerminalFrame? = null
        set(value) {
            field = value
            invalidate()
        }

    var palette = TerminalPalette(0xFF000000.toInt(), 0xFFFFFFFF.toInt(), TerminalPalette.LIGHT)
        set(value) {
            if (field == value) return
            field = value
            invalidate()
        }

    private var typeface: Typeface = Typeface.MONOSPACE
    private var boldTypeface: Typeface = Typeface.create(Typeface.MONOSPACE, Typeface.BOLD)
    private var textSizeSp = 12f
    private val paint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val fill = Paint()
    private var cellWidth = 1f
    private var cellHeight = 1f
    private var baseline = 0f
    private var cols = 0
    private var rows = 0

    /** Text the keyboard is composing, shown at the cursor until it commits it. */
    private var composing = ""

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        measure()
    }

    fun setFont(typeface: Typeface, sizeSp: Float) {
        if (typeface == this.typeface && sizeSp == textSizeSp) return
        this.typeface = typeface
        boldTypeface = Typeface.create(typeface, Typeface.BOLD)
        textSizeSp = sizeSp
        measure()
        fit()
        invalidate()
    }

    private fun measure() {
        paint.typeface = typeface
        paint.textSize = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_SP, textSizeSp, resources.displayMetrics)
        cellWidth = max(1f, paint.measureText("M"))
        val metrics = paint.fontMetrics
        cellHeight = max(1f, metrics.descent - metrics.ascent)
        baseline = -metrics.ascent
    }

    /** Tells the terminal how many cells fit. */
    private fun fit() {
        if (width == 0 || height == 0) return
        val newCols = max(2, floor(width / cellWidth).toInt())
        val newRows = max(1, floor(height / cellHeight).toInt())
        if (newCols == cols && newRows == rows) return
        cols = newCols
        rows = newRows
        listener?.onResize(cols, rows)
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        super.onSizeChanged(w, h, oldw, oldh)
        fit()
    }

    /** Asks for the size again, as when the screen attached a new listener. */
    fun refit() {
        cols = 0
        rows = 0
        fit()
    }

    override fun onDraw(canvas: Canvas) {
        canvas.drawColor(palette.background)
        val frame = frame ?: return
        for ((row, runs) in frame.lines.withIndex()) {
            val top = row * cellHeight
            for (run in runs) {
                val left = run.column.toInt() * cellWidth
                val right = left + run.cells.toInt() * cellWidth
                if (run.bg != TerminalColor.Background) {
                    fill.color = palette.resolve(run.bg)
                    canvas.drawRect(left, top, right, top + cellHeight, fill)
                }
                if (run.text.isBlank() && !run.underline && !run.strikeout) continue
                var color = palette.resolve(run.fg)
                if (run.dim) color = (color and 0x00FFFFFF) or (0x99 shl 24)
                drawText(canvas, run.text, left, right - left, top, color, run.bold, run.italic, run.underline, run.strikeout)
            }
        }
        drawCursor(canvas, frame)
    }

    private fun drawText(
        canvas: Canvas,
        text: String,
        left: Float,
        room: Float,
        top: Float,
        color: Int,
        bold: Boolean = false,
        italic: Boolean = false,
        underline: Boolean = false,
        strikeout: Boolean = false,
    ) {
        paint.color = color
        paint.typeface = if (bold) boldTypeface else typeface
        paint.textSkewX = if (italic) -0.2f else 0f
        paint.isUnderlineText = underline
        paint.isStrikeThruText = strikeout
        // A character the font draws wider than its cells, as from a fallback
        // font, is squeezed into them so the grid stays aligned.
        val width = paint.measureText(text)
        paint.textScaleX = if (width > room * 1.02f) room / width else 1f
        canvas.drawText(text, left, top + baseline, paint)
        paint.textScaleX = 1f
    }

    private fun drawCursor(canvas: Canvas, frame: TerminalFrame) {
        val cursor = frame.cursor ?: return
        if (frame.exitStatus != null || frame.ended != null) return
        val left = cursor.column.toInt() * cellWidth
        val top = cursor.row.toInt() * cellHeight
        if (composing.isNotEmpty()) {
            // The text being composed sits at the cursor, underlined, until it is committed.
            val width = max(cellWidth, paint.apply { typeface = this@TerminalView.typeface }.measureText(composing))
            fill.color = palette.background
            canvas.drawRect(left, top, left + width, top + cellHeight, fill)
            drawText(canvas, composing, left, width, top, palette.foreground, underline = true)
            return
        }
        fill.color = palette.foreground
        when {
            !hasFocus() -> {
                fill.style = Paint.Style.STROKE
                fill.strokeWidth = max(1f, cellWidth / 8)
                canvas.drawRect(left, top, left + cellWidth, top + cellHeight, fill)
                fill.style = Paint.Style.FILL
            }
            cursor.shape == TerminalCursorShape.BLOCK -> {
                canvas.drawRect(left, top, left + cellWidth, top + cellHeight, fill)
                val under = frame.lines.getOrNull(cursor.row.toInt())?.firstOrNull { run ->
                    cursor.column >= run.column && cursor.column < (run.column + run.cells).toUShort()
                }
                if (under != null) {
                    val offset = (cursor.column - under.column).toInt()
                    val char = if (under.cells.toInt() == under.text.length) {
                        under.text.substring(offset, offset + 1)
                    } else {
                        under.text
                    }
                    if (char.isNotBlank()) drawText(canvas, char, left, cellWidth * under.cells.toInt(), top, palette.background)
                }
            }
            cursor.shape == TerminalCursorShape.UNDERLINE ->
                canvas.drawRect(left, top + cellHeight - max(2f, cellHeight / 10), left + cellWidth, top + cellHeight, fill)
            else -> canvas.drawRect(left, top, left + max(2f, cellWidth / 6), top + cellHeight, fill)
        }
    }

    override fun onFocusChanged(gainFocus: Boolean, direction: Int, previouslyFocusedRect: android.graphics.Rect?) {
        super.onFocusChanged(gainFocus, direction, previouslyFocusedRect)
        invalidate()
    }

    fun showKeyboard() {
        requestFocus()
        context.getSystemService(InputMethodManager::class.java)?.showSoftInput(this, 0)
    }

    // Touch: tap for the keyboard, drag to scroll, pinch for the text size.

    private var scrollRest = 0f
    private var scaling = false

    private val gestures = GestureDetector(
        context,
        object : GestureDetector.SimpleOnGestureListener() {
            override fun onDown(e: MotionEvent): Boolean {
                scrollRest = 0f
                return true
            }

            override fun onSingleTapUp(e: MotionEvent): Boolean {
                showKeyboard()
                return true
            }

            override fun onScroll(e1: MotionEvent?, e2: MotionEvent, distanceX: Float, distanceY: Float): Boolean {
                if (scaling) return true
                // Dragging down shows earlier lines.
                scrollRest -= distanceY
                val lines = (scrollRest / cellHeight).toInt()
                if (lines != 0) {
                    scrollRest -= lines * cellHeight
                    listener?.onScroll(lines)
                }
                return true
            }
        },
    )

    private val scaleGestures = ScaleGestureDetector(
        context,
        object : ScaleGestureDetector.SimpleOnScaleGestureListener() {
            /** The size when the pinch began; the detector's factor counts from then. */
            private var start = 12f
            private var sent = 12f

            override fun onScaleBegin(detector: ScaleGestureDetector): Boolean {
                scaling = true
                start = textSizeSp
                sent = textSizeSp
                return true
            }

            override fun onScale(detector: ScaleGestureDetector): Boolean {
                val size = start * detector.scaleFactor
                if (abs(size - sent) >= 0.5f) {
                    listener?.onTextSize(size)
                    sent = size
                }
                // False keeps the factor counting from the start of the pinch.
                return false
            }

            override fun onScaleEnd(detector: ScaleGestureDetector) {
                scaling = false
            }
        },
    )

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(event: MotionEvent): Boolean {
        scaleGestures.onTouchEvent(event)
        gestures.onTouchEvent(event)
        return true
    }

    // Keyboard input.

    override fun onCheckIsTextEditor(): Boolean = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_ACTION_NONE
        return TerminalInputConnection()
    }

    /** Sends text from the keyboard: line breaks as Enter, one character with armed modifiers as a key. */
    private fun commit(text: String) {
        if (text.isEmpty()) return
        val listener = listener ?: return
        val modifiers = listener.takeModifiers()
        if ((modifiers.ctrl || modifiers.alt) && text.codePointCount(0, text.length) == 1) {
            listener.onKey(TerminalKey.Char(text), modifiers)
            return
        }
        val lines = text.split('\n')
        lines.forEachIndexed { index, line ->
            if (line.isNotEmpty()) listener.onText(line)
            if (index < lines.lastIndex) listener.onKey(TerminalKey.Enter, KeyModifiers(false, false, false))
        }
    }

    private fun setComposing(text: String) {
        composing = text
        invalidate()
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        val listener = listener ?: return super.onKeyDown(keyCode, event)
        if (keyCode == KeyEvent.KEYCODE_BACK) return super.onKeyDown(keyCode, event)
        val key = terminalKey(keyCode)
        val unicode = event.getUnicodeChar(event.metaState and (KeyEvent.META_CTRL_MASK or KeyEvent.META_ALT_MASK).inv())
        // Keys that are not for the terminal, as the volume keys, leave the armed modifiers be.
        if (key == null && (unicode == 0 || unicode and KeyCharacterMapCombining != 0)) {
            return super.onKeyDown(keyCode, event)
        }
        val armed = listener.takeModifiers()
        val modifiers = KeyModifiers(
            ctrl = event.isCtrlPressed || armed.ctrl,
            alt = event.isAltPressed || armed.alt,
            shift = event.isShiftPressed,
        )
        if (key != null) {
            listener.onKey(key, modifiers)
            return true
        }
        val text = String(Character.toChars(unicode))
        if (modifiers.ctrl || modifiers.alt) {
            listener.onKey(TerminalKey.Char(text), modifiers)
        } else {
            listener.onText(text)
        }
        return true
    }

    private inner class TerminalInputConnection : BaseInputConnection(this, true) {
        override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
            setComposing("")
            commit(text?.toString().orEmpty())
            editable?.clear()
            return true
        }

        override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean {
            setComposing(text?.toString().orEmpty())
            return true
        }

        override fun finishComposingText(): Boolean {
            val text = composing
            setComposing("")
            commit(text)
            editable?.clear()
            return true
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            val listener = listener ?: return true
            val none = KeyModifiers(false, false, false)
            repeat(beforeLength) { listener.onKey(TerminalKey.Backspace, none) }
            repeat(afterLength) { listener.onKey(TerminalKey.Delete, none) }
            return true
        }

        override fun sendKeyEvent(event: KeyEvent): Boolean {
            if (event.action == KeyEvent.ACTION_DOWN) onKeyDown(event.keyCode, event)
            return true
        }
    }

    private companion object {
        /** The flag `getUnicodeChar` sets on a dead key, which combines with the next one. */
        const val KeyCharacterMapCombining = android.view.KeyCharacterMap.COMBINING_ACCENT

        fun terminalKey(keyCode: Int): TerminalKey? = when (keyCode) {
            KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> TerminalKey.Enter
            KeyEvent.KEYCODE_DEL -> TerminalKey.Backspace
            KeyEvent.KEYCODE_FORWARD_DEL -> TerminalKey.Delete
            KeyEvent.KEYCODE_TAB -> TerminalKey.Tab
            KeyEvent.KEYCODE_ESCAPE -> TerminalKey.Escape
            KeyEvent.KEYCODE_DPAD_UP -> TerminalKey.Up
            KeyEvent.KEYCODE_DPAD_DOWN -> TerminalKey.Down
            KeyEvent.KEYCODE_DPAD_LEFT -> TerminalKey.Left
            KeyEvent.KEYCODE_DPAD_RIGHT -> TerminalKey.Right
            KeyEvent.KEYCODE_MOVE_HOME -> TerminalKey.Home
            KeyEvent.KEYCODE_MOVE_END -> TerminalKey.End
            KeyEvent.KEYCODE_PAGE_UP -> TerminalKey.PageUp
            KeyEvent.KEYCODE_PAGE_DOWN -> TerminalKey.PageDown
            KeyEvent.KEYCODE_INSERT -> TerminalKey.Insert
            in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 ->
                TerminalKey.Function((keyCode - KeyEvent.KEYCODE_F1 + 1).toUByte())
            else -> null
        }
    }
}
