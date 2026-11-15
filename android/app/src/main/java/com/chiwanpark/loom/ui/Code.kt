package com.chiwanpark.loom.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.core.Highlight
import com.chiwanpark.loom.core.Span

/** How each kind of highlighted code looks, in the light or the dark theme. */
class SyntaxColors(private val styles: Map<Highlight, SpanStyle>) {
    fun style(highlight: Highlight): SpanStyle? = styles[highlight]

    companion object {
        private fun of(colors: Map<Highlight, Long>): SyntaxColors {
            val styles = colors.mapValues { (_, argb) -> SpanStyle(color = Color(argb)) }.toMutableMap()
            styles[Highlight.HEADING] = (styles[Highlight.HEADING] ?: SpanStyle()).copy(fontWeight = FontWeight.Bold)
            styles[Highlight.STRONG] = SpanStyle(fontWeight = FontWeight.Bold)
            styles[Highlight.EMPHASIS] = SpanStyle(fontStyle = FontStyle.Italic)
            styles[Highlight.LINK] = (styles[Highlight.LINK] ?: SpanStyle()).copy(textDecoration = TextDecoration.Underline)
            return SyntaxColors(styles)
        }

        // After GitHub's light and dark themes, as the desktop's default themes are.
        val light = of(
            mapOf(
                Highlight.COMMENT to 0xFF6A737D,
                Highlight.STRING to 0xFF032F62,
                Highlight.ESCAPE to 0xFF22863A,
                Highlight.NUMBER to 0xFF005CC5,
                Highlight.CONSTANT to 0xFF005CC5,
                Highlight.KEYWORD to 0xFFD73A49,
                Highlight.OPERATOR to 0xFFD73A49,
                Highlight.TYPE to 0xFFB35900,
                Highlight.FUNCTION to 0xFF6F42C1,
                Highlight.TAG to 0xFF22863A,
                Highlight.ATTRIBUTE to 0xFF6F42C1,
                Highlight.PROPERTY to 0xFF005CC5,
                Highlight.SPECIAL to 0xFFE36209,
                Highlight.HEADING to 0xFF005CC5,
                Highlight.LINK to 0xFF032F62,
                Highlight.CODE to 0xFF032F62,
            ),
        )
        val dark = of(
            mapOf(
                Highlight.COMMENT to 0xFF8B949E,
                Highlight.STRING to 0xFFA5D6FF,
                Highlight.ESCAPE to 0xFF79C0FF,
                Highlight.NUMBER to 0xFF79C0FF,
                Highlight.CONSTANT to 0xFF79C0FF,
                Highlight.KEYWORD to 0xFFFF7B72,
                Highlight.OPERATOR to 0xFFFF7B72,
                Highlight.TYPE to 0xFFFFA657,
                Highlight.FUNCTION to 0xFFD2A8FF,
                Highlight.TAG to 0xFF7EE787,
                Highlight.ATTRIBUTE to 0xFF79C0FF,
                Highlight.PROPERTY to 0xFF79C0FF,
                Highlight.SPECIAL to 0xFFFFA657,
                Highlight.HEADING to 0xFF79C0FF,
                Highlight.LINK to 0xFFA5D6FF,
                Highlight.CODE to 0xFFA5D6FF,
            ),
        )

        val current: SyntaxColors
            @Composable @ReadOnlyComposable
            get() = if (isSystemInDarkTheme()) dark else light
    }
}

/** Spaces a tab stands for. */
private const val TAB_WIDTH = 4

/** Whether a character takes two columns of a monospace font, as CJK characters do. */
private fun isWide(codePoint: Int): Boolean =
    codePoint in 0x1100..0x115F || codePoint in 0x2E80..0xA4CF || codePoint in 0xAC00..0xD7A3 ||
        codePoint in 0xF900..0xFAFF || codePoint in 0xFE30..0xFE4F || codePoint in 0xFF00..0xFF60 ||
        codePoint in 0xFFE0..0xFFE6 || codePoint in 0x1F300..0x1FAFF || codePoint in 0x20000..0x3FFFD

/** How many columns `text` takes, with tabs to the next stop. */
fun columns(text: String): Int {
    var column = 0
    var i = 0
    while (i < text.length) {
        val codePoint = text.codePointAt(i)
        column += when {
            codePoint == '\t'.code -> TAB_WIDTH - column % TAB_WIDTH
            isWide(codePoint) -> 2
            else -> 1
        }
        i += Character.charCount(codePoint)
    }
    return column
}

/** A line with its highlighted parts in their colors and its tabs as spaces. */
fun highlighted(text: String, spans: List<Span>, colors: SyntaxColors): AnnotatedString = buildAnnotatedString {
    var column = 0
    fun appendPlain(from: Int, to: Int) {
        for (i in from until to) {
            val c = text[i]
            if (c == '\t') {
                val spaces = TAB_WIDTH - column % TAB_WIDTH
                repeat(spaces) { append(' ') }
                column += spaces
            } else {
                append(c)
                // A surrogate pair counts once, at its first half.
                if (!c.isLowSurrogate()) column += if (isWide(text.codePointAt(i))) 2 else 1
            }
        }
    }
    var at = 0
    for (span in spans) {
        val start = span.start.toInt().coerceIn(at, text.length)
        val end = span.end.toInt().coerceIn(start, text.length)
        appendPlain(at, start)
        val style = colors.style(span.highlight)
        if (style != null) {
            val from = length
            appendPlain(start, end)
            addStyle(style, from, length)
        } else {
            appendPlain(start, end)
        }
        at = end
    }
    appendPlain(at, text.length)
}

/** The style of code in viewers: the code font at the size of text in blocks. */
@Composable
@ReadOnlyComposable
fun codeStyle(): TextStyle = blockStyle(MaterialTheme.typography.bodySmall).copy(fontFamily = LoomFonts.code)

/**
 * Lines of code in a scrolling list, `count` of them, each with what
 * `gutter` puts before it, such as its number. Unwrapped lines scroll
 * sideways together; `widest` is how many columns the longest one takes,
 * and `gutterColumns` how many the gutter does. A line's text is made
 * once for each `content`, which stands for the lines.
 */
@Composable
fun CodeList(
    count: Int,
    widest: Int,
    gutterColumns: Int,
    wrap: Boolean,
    content: Any,
    modifier: Modifier = Modifier,
    state: LazyListState = rememberLazyListState(),
    background: (Int) -> Color? = { null },
    gutter: @Composable (Int) -> Unit = {},
    line: (Int) -> AnnotatedString,
) {
    val style = codeStyle()
    val measurer = rememberTextMeasurer()
    val density = LocalDensity.current
    val charWidth = remember(style, density) { measurer.measure("0", style).size.width }
    BoxWithConstraints(modifier) {
        // Padding takes some room besides the gutter and the text.
        val textWidth = with(density) { (charWidth * (widest + gutterColumns + 1)).toDp() } + 24.dp
        val rowWidth = if (textWidth > maxWidth) textWidth else maxWidth
        SelectionContainer {
            LazyColumn(
                state = state,
                modifier = Modifier
                    .fillMaxSize()
                    .then(if (wrap) Modifier else Modifier.horizontalScroll(rememberScrollState())),
            ) {
                items(count) { index ->
                    val color = background(index)
                    val text = remember(content, index) { line(index) }
                    Row(
                        (if (wrap) Modifier.fillMaxWidth() else Modifier.width(rowWidth))
                            .then(if (color != null) Modifier.background(color) else Modifier)
                            .padding(end = 8.dp),
                    ) {
                        gutter(index)
                        Text(
                            text,
                            style = style,
                            softWrap = wrap,
                            modifier = if (wrap) Modifier.weight(1f) else Modifier,
                        )
                    }
                }
            }
        }
    }
}

/** A line number in a gutter `digits` columns wide, dimmed; none leaves the room empty. */
@Composable
fun LineNumber(number: Int?, digits: Int) {
    val style = codeStyle()
    Text(
        (number?.toString() ?: "").padStart(digits),
        style = style,
        color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.6f),
        softWrap = false,
        modifier = Modifier.padding(start = 4.dp, end = 6.dp),
    )
}
