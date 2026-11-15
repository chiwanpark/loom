package com.chiwanpark.loom.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.PrimaryTabRow
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Tab
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.LoomApplication
import com.chiwanpark.loom.Settings
import kotlin.math.abs
import kotlin.math.roundToInt

/** What the font screen picks a font for. */
private enum class FontTarget(val title: String) { Text("Text"), Code("Code") }

/** The categories of Google Fonts, as the catalog names them, with their labels. */
private val CATEGORIES = listOf(
    "sans-serif" to "Sans serif",
    "serif" to "Serif",
    "display" to "Display",
    "handwriting" to "Handwriting",
    "monospace" to "Monospace",
)

/**
 * The text size, the font of the app's text, and the font of code, picked
 * from the system's fonts, JetBrains Mono, and Google Fonts, which can be
 * searched; and for each, a fallback font from Google Fonts that draws what
 * the font lacks, such as Hangul, before the system's fonts do. Every pick
 * applies at once and each font's name is shown in that font. On a tall
 * screen the preview and the size stay on top; on a low one they scroll
 * away, and the tabs and the search field stay.
 */
@Composable
fun FontScreen(app: LoomApplication) {
    val settings = app.settings
    val context = LocalContext.current
    val catalog = remember { FontCatalog.get(context.applicationContext) }
    var target by rememberSaveable { mutableIntStateOf(0) }
    var query by rememberSaveable { mutableStateOf("") }
    var fallback by rememberSaveable { mutableStateOf(false) }
    var script by rememberSaveable { mutableStateOf<String?>(null) }
    var category by rememberSaveable { mutableStateOf<String?>(null) }
    val forCode = FontTarget.entries[target] == FontTarget.Code
    // Code fonts are all monospaced; a fallback for code need not be.
    val categories = !forCode || fallback
    val list = rememberLazyListState()

    Scaffold(topBar = { LoomTopBar("Font", onBack = { app.navigator.pop() }) }) { padding ->
        BoxWithConstraints(Modifier.fillMaxSize().padding(padding)) {
            val pinned = maxHeight >= 600.dp
            // A new search starts at the top of its results.
            LaunchedEffect(target, fallback, query, script, category) {
                val first = if (pinned) 0 else 3
                if (list.firstVisibleItemIndex > first) list.scrollToItem(first)
            }
            Column(Modifier.fillMaxSize()) {
                if (pinned) {
                    FontPreview()
                    TextSizeSetting(settings)
                    BlockSizeSetting(settings)
                }
                LazyColumn(Modifier.weight(1f), state = list) {
                    if (!pinned) {
                        item(key = "preview") { FontPreview() }
                        item(key = "size") { TextSizeSetting(settings) }
                        item(key = "block-size") { BlockSizeSetting(settings) }
                    }
                    stickyHeader(key = "search") {
                        Surface {
                            Column {
                                PrimaryTabRow(selectedTabIndex = target) {
                                    FontTarget.entries.forEachIndexed { index, it ->
                                        Tab(
                                            selected = target == index,
                                            onClick = { target = index },
                                            text = { Text(it.title) },
                                        )
                                    }
                                }
                                FallbackSwitch(fallback, onChange = { fallback = it })
                                SearchControls(
                                    query = query,
                                    onQuery = { query = it },
                                    script = script,
                                    onScript = { script = it },
                                    category = category.takeIf { categories },
                                    onCategory = if (categories) { it -> category = it } else null,
                                )
                                HorizontalDivider()
                            }
                        }
                    }
                    val filter = FontFilter(query, script, category.takeIf { categories })
                    if (fallback) {
                        fontList(
                            filter,
                            listOf(LoomFonts.systemFallback),
                            if (forCode) LoomFonts.suggestedCodeFallbacks else LoomFonts.suggestedTextFallbacks,
                            catalog.fonts,
                            catalog,
                            selected = if (forCode) settings.codeFallbackFont else settings.textFallbackFont,
                            onPick = if (forCode) settings::updateCodeFallbackFont else settings::updateTextFallbackFont,
                        )
                    } else if (forCode) {
                        fontList(
                            filter,
                            LoomFonts.systemCodeFonts,
                            LoomFonts.suggestedCodeFonts,
                            catalog.fonts.filter { it.monospaced },
                            catalog,
                            selected = settings.codeFont,
                            onPick = settings::updateCodeFont,
                        )
                    } else {
                        fontList(
                            filter,
                            LoomFonts.systemTextFonts,
                            LoomFonts.suggestedTextFonts,
                            catalog.fonts,
                            catalog,
                            selected = settings.textFont,
                            onPick = settings::updateTextFont,
                        )
                    }
                    item(key = "note") {
                        val fallbackNote = if (fallback) {
                            "The fallback font draws the characters that the ${if (forCode) "code" else "text"} " +
                                "font does not have, such as Hangul, before the system's fonts do. "
                        } else {
                            ""
                        }
                        Text(
                            fallbackNote +
                                "Google Play services downloads a font from Google Fonts when it is first shown; " +
                                "until then, or without Google Play services, the system's font shows.",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
                        )
                    }
                }
            }
        }
    }
}

/** What the search field and the chips ask for. */
private class FontFilter(query: String, val script: String?, val category: String?) {
    private val needle = normalize(query)

    val any: Boolean = needle.isNotEmpty() || script != null || category != null

    fun matches(name: String) = needle.isEmpty() || needle in normalize(name)

    fun matches(font: CatalogFont) =
        matches(font.name) && (script == null || script in font.scripts) && (category == null || font.category == category)

    /** The system's fonts and JetBrains Mono have no category and are not counted for a script. */
    fun matches(font: FontChoice) = script == null && category == null && matches(font.name)

    private companion object {
        /** Case and spaces do not matter: "nanumgothic" finds Nanum Gothic. */
        fun normalize(text: String) = text.lowercase().filterNot { it.isWhitespace() }
    }
}

/**
 * The fonts to pick from. Without a search: the system's, the suggested
 * Google Fonts, then every other one by popularity; with one, what matches.
 */
private fun LazyListScope.fontList(
    filter: FontFilter,
    system: List<FontChoice>,
    suggested: List<String>,
    google: List<CatalogFont>,
    catalog: FontCatalog,
    selected: String,
    onPick: (String) -> Unit,
) {
    val row: @Composable (FontChoice) -> Unit = { font ->
        ChoiceRowPadded(font, selected == font.id, catalog) { onPick(font.id) }
    }
    if (!filter.any) {
        item(key = "h/system") { SectionTitle("System and bundled") }
        items(system, key = { "s/${it.id}" }) { row(it) }
        val suggestions = suggested.mapNotNull(catalog::find)
        item(key = "h/suggested") { SectionTitle("Suggested") }
        items(suggestions, key = { "g/${it.name}" }) { row(catalog.choice(it)) }
        val rest = google.filter { it.name !in suggested }
        item(key = "h/all") { SectionTitle("All Google Fonts (${rest.size})") }
        items(rest, key = { "a/${it.name}" }) { row(catalog.choice(it)) }
    } else {
        val systemMatches = system.filter(filter::matches)
        val googleMatches = google.filter(filter::matches)
        val count = systemMatches.size + googleMatches.size
        item(key = "h/results") { SectionTitle(if (count == 1) "1 font" else "$count fonts") }
        items(systemMatches, key = { "s/${it.id}" }) { row(it) }
        items(googleMatches, key = { "a/${it.name}" }) { row(catalog.choice(it)) }
    }
}

@Composable
private fun ChoiceRowPadded(font: FontChoice, selected: Boolean, catalog: FontCatalog, onClick: () -> Unit) {
    Row(Modifier.padding(horizontal = 12.dp)) {
        ChoiceRow(font.name, fontDetail(font, catalog), selected, titleFont = font.family, onClick = onClick)
    }
}

/** Where a font comes from, and for Google Fonts its category and the scripts it has besides Latin. */
private fun fontDetail(font: FontChoice, catalog: FontCatalog): String? = when {
    font.downloaded -> {
        val entry = catalog.find(font.name)
        val scripts = FONT_SCRIPTS.filter { it.first in font.scripts }.map { it.second }
        listOfNotNull(
            CATEGORIES.firstOrNull { it.first == entry?.category }?.second,
            scripts.take(MAX_SCRIPTS).joinToString(", ").ifEmpty { null }
                ?.let { if (scripts.size > MAX_SCRIPTS) "$it, …" else it },
        ).joinToString(" · ").ifEmpty { null }
    }
    font === LoomFonts.systemFallback -> "What a font lacks comes from the system's fonts"
    font.id == Settings.DEFAULT_CODE_FONT -> "Bundled with the app"
    else -> null
}

/** The most scripts a font's row names. */
private const val MAX_SCRIPTS = 3

/** Whether the screen picks the font itself or its fallback font. */
@Composable
private fun FallbackSwitch(fallback: Boolean, onChange: (Boolean) -> Unit) {
    SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth().padding(start = 12.dp, end = 12.dp, top = 8.dp)) {
        listOf(false to "Font", true to "Fallback font").forEachIndexed { index, (value, label) ->
            SegmentedButton(
                selected = fallback == value,
                onClick = { onChange(value) },
                shape = SegmentedButtonDefaults.itemShape(index, 2),
                label = { Text(label) },
            )
        }
    }
}

/** The search field, a chip per script, and, where it helps, one per category. */
@Composable
private fun SearchControls(
    query: String,
    onQuery: (String) -> Unit,
    script: String?,
    onScript: (String?) -> Unit,
    category: String?,
    onCategory: ((String?) -> Unit)?,
) {
    Column(Modifier.padding(start = 12.dp, end = 12.dp, top = 8.dp)) {
        CompactTextField(
            value = query,
            onValueChange = onQuery,
            placeholder = "Search Google Fonts",
            maxLines = 1,
            modifier = Modifier.fillMaxWidth(),
            trailingIcon = {
                if (query.isEmpty()) {
                    LoomIcon(LoomIcons.Search, null, size = 18.dp)
                } else {
                    IconButton(onClick = { onQuery("") }, modifier = Modifier.size(40.dp)) {
                        LoomIcon(LoomIcons.X, "Clear the search", size = 18.dp)
                    }
                }
            },
        )
        Row(
            Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()),
            horizontalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            FONT_SCRIPTS.forEach { (id, label) ->
                FilterChip(
                    selected = script == id,
                    onClick = { onScript(if (script == id) null else id) },
                    label = { Text(label) },
                )
            }
            if (onCategory != null) {
                CATEGORIES.forEach { (id, label) ->
                    FilterChip(
                        selected = category == id,
                        onClick = { onCategory(if (category == id) null else id) },
                        label = { Text(label) },
                    )
                }
            }
        }
    }
}

/**
 * Text and code as the conversation shows them, in the fonts and sizes
 * picked; the code is in a block, like a tool call's output.
 */
@Composable
private fun FontPreview() {
    Column(
        Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Text(
            "The quick brown fox jumps over the lazy dog. 다람쥐 헌 쳇바퀴에 타고파.",
            style = MaterialTheme.typography.bodyLarge,
        )
        Text(
            "fn main() { println!(\"0O 1lI {}\", \"한글\"); }",
            fontFamily = LoomFonts.code,
            style = blockStyle(MaterialTheme.typography.bodySmall),
            softWrap = false,
            modifier = Modifier
                .fillMaxWidth()
                .background(MaterialTheme.colorScheme.surfaceContainerHigh, RoundedCornerShape(6.dp))
                .horizontalScroll(rememberScrollState())
                .padding(horizontal = 6.dp, vertical = 4.dp),
        )
    }
}

/** The size of the app's text, as a share of the system's. */
@Composable
private fun TextSizeSetting(settings: Settings) {
    ScaleSetting("Text", Settings.TEXT_SCALES, settings.textScale, settings::updateTextScale)
}

/**
 * The size of text in blocks, as a share of the app's text: tool calls'
 * arguments and output, and the reasoning of answers.
 */
@Composable
private fun BlockSizeSetting(settings: Settings) {
    ScaleSetting("Blocks", Settings.BLOCK_SCALES, settings.blockScale, settings::updateBlockScale)
}

/** The labels of the size sliders, which take the room of the widest so the sliders line up. */
private val SCALE_LABELS = listOf("Text", "Blocks")

/** A size from a slider that stops at each of `scales`. */
@Composable
private fun ScaleSetting(label: String, scales: List<Float>, value: Float, onChange: (Float) -> Unit) {
    Row(
        Modifier.padding(horizontal = 16.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Box {
            SCALE_LABELS.forEach {
                Text(
                    it,
                    style = MaterialTheme.typography.titleSmall,
                    color = MaterialTheme.colorScheme.primary,
                    modifier = if (it == label) Modifier else Modifier.alpha(0f).clearAndSetSemantics {},
                )
            }
        }
        Slider(
            value = value,
            onValueChange = { picked ->
                val scale = scales.minBy { abs(it - picked) }
                if (scale != value) onChange(scale)
            },
            valueRange = scales.first()..scales.last(),
            steps = scales.size - 2,
            modifier = Modifier.weight(1f),
        )
        // Room for "100%" whatever the value, so the slider keeps its length.
        Box(contentAlignment = Alignment.CenterEnd) {
            Text(
                "100%",
                style = MaterialTheme.typography.bodyMedium,
                modifier = Modifier.alpha(0f).clearAndSetSemantics {},
            )
            Text(
                "${(value * 100).roundToInt()}%",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

@Composable
private fun SectionTitle(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 12.dp, bottom = 2.dp),
    )
}
