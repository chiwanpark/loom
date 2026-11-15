package com.chiwanpark.loom.ui

import android.content.Context
import android.graphics.Typeface
import androidx.compose.material3.Typography
import androidx.compose.runtime.Composable
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.googlefonts.GoogleFont
import androidx.compose.ui.text.googlefonts.Font as GoogleFontOf
import com.chiwanpark.loom.R
import com.chiwanpark.loom.Settings

/**
 * A font to choose from. `family` is null for the system's default font;
 * fonts from Google Fonts are `downloaded` by Google Play services when first
 * used, and the system's font shows until they arrive or if they cannot.
 * `scripts` are the scripts other than Latin it covers, as the catalog names
 * them, and `source` is where its files come from.
 */
class FontChoice(
    val id: String,
    val name: String,
    val family: FontFamily?,
    val source: FontSource,
    val downloaded: Boolean = false,
    val scripts: List<String> = emptyList(),
)

/** Where the files of a font come from, to build a typeface with a fallback font from them. */
sealed interface FontSource {
    /** A font of the system, such as its default font. */
    class System(val typeface: Typeface) : FontSource

    /** Fonts the app bundles, by their resource ids and weights. */
    class Bundled(val fonts: List<Pair<Int, Int>>) : FontSource

    /** A family of Google Fonts, in its upright weights. */
    class Google(val name: String, val weights: List<Int>) : FontSource
}

/** A family of Google Fonts, as scripts/fonts/generate.py lists it. */
class CatalogFont(
    val name: String,
    val category: String,
    val weights: List<Int>,
    val scripts: List<String>,
    val monospaced: Boolean,
)

/**
 * The families of Google Fonts that Google Play services can download, the
 * most popular first, from the catalog the app bundles
 * (assets/google-fonts.tsv).
 */
class FontCatalog private constructor(val fonts: List<CatalogFont>) {
    private val byName = fonts.associateBy { it.name }
    private val choices = HashMap<String, FontChoice>()

    fun find(name: String): CatalogFont? = byName[name]

    /** The family as a choice, made once, so its font family stays the same object. */
    fun choice(font: CatalogFont): FontChoice = choices.getOrPut(font.name) {
        FontChoice(
            id = GOOGLE_PREFIX + font.name,
            name = font.name,
            family = FontFamily(
                font.weights.map { weight -> GoogleFontOf(GoogleFont(font.name), googleFonts, FontWeight(weight)) },
            ),
            source = FontSource.Google(font.name, font.weights),
            downloaded = true,
            scripts = font.scripts,
        )
    }

    /** The choice for a family by name; one missing from the catalog gets the usual weights. */
    fun choice(name: String): FontChoice =
        choice(find(name) ?: CatalogFont(name, "sans-serif", listOf(400, 700), emptyList(), monospaced = false))

    companion object {
        const val GOOGLE_PREFIX = "google:"

        const val PROVIDER_AUTHORITY = "com.google.android.gms.fonts"
        const val PROVIDER_PACKAGE = "com.google.android.gms"

        private val googleFonts = GoogleFont.Provider(
            providerAuthority = PROVIDER_AUTHORITY,
            providerPackage = PROVIDER_PACKAGE,
            certificates = R.array.com_google_android_gms_fonts_certs,
        )

        @Volatile
        private var loaded: FontCatalog? = null

        /** The catalog, read from the app's assets the first time. */
        fun get(context: Context): FontCatalog = loaded ?: synchronized(this) {
            loaded ?: read(context).also { loaded = it }
        }

        private fun read(context: Context): FontCatalog {
            val fonts = runCatching {
                context.assets.open("google-fonts.tsv").bufferedReader().useLines { lines ->
                    lines.mapNotNull { line ->
                        val fields = line.split('\t')
                        if (fields.size < 4) return@mapNotNull null
                        CatalogFont(
                            name = fields[0],
                            category = fields[1],
                            weights = fields[2].split(',').mapNotNull { it.toIntOrNull() }.ifEmpty { listOf(400) },
                            monospaced = 'm' in fields[3],
                            scripts = fields.getOrNull(4).orEmpty().split(',').filter { it.isNotEmpty() },
                        )
                    }.toList()
                }
            }.getOrDefault(emptyList())
            return FontCatalog(fonts)
        }
    }
}

/** Scripts to filter fonts by, as the catalog names them, with their labels. */
val FONT_SCRIPTS = listOf(
    "korean" to "Korean",
    "japanese" to "Japanese",
    "chinese" to "Chinese",
    "cyrillic" to "Cyrillic",
    "greek" to "Greek",
    "arabic" to "Arabic",
    "hebrew" to "Hebrew",
    "devanagari" to "Devanagari",
    "thai" to "Thai",
)

/** The fonts of the app's text and of code, which the user picks with the text size. */
object LoomFonts {
    private val jetBrainsMonoFiles = listOf(
        R.font.jetbrains_mono_nl_regular to 400,
        R.font.jetbrains_mono_nl_bold to 700,
    )

    /** JetBrains Mono without ligatures, which the app bundles. */
    private val jetBrainsMono = FontFamily(jetBrainsMonoFiles.map { (id, weight) -> Font(id, FontWeight(weight)) })

    /** The text fonts that are not from Google Fonts. */
    val systemTextFonts = listOf(
        FontChoice(Settings.DEFAULT_TEXT_FONT, "System default", null, FontSource.System(Typeface.DEFAULT)),
        FontChoice("serif", "System serif", FontFamily.Serif, FontSource.System(Typeface.SERIF)),
    )

    /** The code fonts that are not from Google Fonts. */
    val systemCodeFonts = listOf(
        FontChoice(Settings.DEFAULT_CODE_FONT, "JetBrains Mono", jetBrainsMono, FontSource.Bundled(jetBrainsMonoFiles)),
        FontChoice("monospace", "System monospace", FontFamily.Monospace, FontSource.System(Typeface.MONOSPACE)),
    )

    /**
     * No fallback font of the user's: the characters a font lacks come from
     * the system's fonts.
     */
    val systemFallback = FontChoice(
        Settings.DEFAULT_FALLBACK_FONT,
        "System fonts",
        null,
        FontSource.System(Typeface.DEFAULT),
    )

    /** Google Fonts offered first for text, before searching: common ones, and some for Korean. */
    val suggestedTextFonts = listOf(
        "Inter", "Roboto", "Noto Sans KR", "IBM Plex Sans KR", "Nanum Gothic", "Nanum Myeongjo",
    )

    /** Google Fonts offered first for code. */
    val suggestedCodeFonts = listOf(
        "Nanum Gothic Coding", "Source Code Pro", "IBM Plex Mono", "Roboto Mono", "Fira Code", "Noto Sans Mono",
    )

    /** Google Fonts offered first as the fallback of text: ones for Korean, Japanese, and Chinese. */
    val suggestedTextFallbacks = listOf(
        "Noto Sans KR", "IBM Plex Sans KR", "Nanum Gothic", "Noto Serif KR", "Noto Sans JP", "Noto Sans SC",
        "Noto Sans TC",
    )

    /** Google Fonts offered first as the fallback of code: monospaced ones for Korean and Japanese first. */
    val suggestedCodeFallbacks = listOf(
        "Nanum Gothic Coding", "M PLUS 1 Code", "Noto Sans KR", "IBM Plex Sans KR", "Noto Sans JP", "Noto Sans SC",
    )

    /** The text font with this id; an unknown id is the default. */
    fun textFont(id: String, catalog: FontCatalog): FontChoice = resolve(id, systemTextFonts, catalog)

    /** The code font with this id; an unknown id is the default. */
    fun codeFont(id: String, catalog: FontCatalog): FontChoice = resolve(id, systemCodeFonts, catalog)

    /** The fallback font with this id; an unknown id is the system's fonts. */
    fun fallbackFont(id: String, catalog: FontCatalog): FontChoice = resolve(id, listOf(systemFallback), catalog)

    private fun resolve(id: String, system: List<FontChoice>, catalog: FontCatalog): FontChoice =
        system.firstOrNull { it.id == id }
            ?: id.removePrefix(FontCatalog.GOOGLE_PREFIX).takeIf { id.startsWith(FontCatalog.GOOGLE_PREFIX) }
                ?.let(catalog::choice)
            ?: system.first()

    /** The font for code, commands, and their output that the user picked. */
    val code: FontFamily
        @Composable @ReadOnlyComposable
        get() = LocalCodeFont.current
}

val LocalCodeFont = staticCompositionLocalOf<FontFamily> { FontFamily.Monospace }

/** Material's text styles in `family`, or as they are for the system's font. */
fun Typography.withFontFamily(family: FontFamily?): Typography = if (family == null) {
    this
} else {
    Typography(
        displayLarge = displayLarge.copy(fontFamily = family),
        displayMedium = displayMedium.copy(fontFamily = family),
        displaySmall = displaySmall.copy(fontFamily = family),
        headlineLarge = headlineLarge.copy(fontFamily = family),
        headlineMedium = headlineMedium.copy(fontFamily = family),
        headlineSmall = headlineSmall.copy(fontFamily = family),
        titleLarge = titleLarge.copy(fontFamily = family),
        titleMedium = titleMedium.copy(fontFamily = family),
        titleSmall = titleSmall.copy(fontFamily = family),
        bodyLarge = bodyLarge.copy(fontFamily = family),
        bodyMedium = bodyMedium.copy(fontFamily = family),
        bodySmall = bodySmall.copy(fontFamily = family),
        labelLarge = labelLarge.copy(fontFamily = family),
        labelMedium = labelMedium.copy(fontFamily = family),
        labelSmall = labelSmall.copy(fontFamily = family),
    )
}
