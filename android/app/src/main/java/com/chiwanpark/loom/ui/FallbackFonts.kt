package com.chiwanpark.loom.ui

import android.content.Context
import android.graphics.Paint
import android.graphics.Typeface
import android.graphics.fonts.Font
import android.graphics.text.TextRunShaper
import android.util.Log
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.ui.text.font.FontFamily
import androidx.core.provider.FontRequest
import androidx.core.provider.FontsContractCompat
import com.chiwanpark.loom.R
import java.util.concurrent.ConcurrentHashMap
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import android.graphics.fonts.FontFamily as NativeFontFamily
import androidx.compose.ui.text.font.Typeface as ComposeTypeface

/**
 * Typefaces that draw what their main font lacks, such as Hangul, from a
 * fallback font the user picked, and only then from the system's fonts.
 * Each is built once from the fonts' files, in the regular and bold weights.
 */
object FallbackFonts {
    private val built = ConcurrentHashMap<String, FontFamily>()

    /** The family built before for these fonts, if any. */
    fun cached(main: FontChoice, fallback: FontChoice): FontFamily? = built[key(main, fallback)]

    /**
     * The family of `main` with `fallback` before the system's `generic`
     * family, such as "sans-serif"; null when a font's files cannot be had,
     * as without Google Play services.
     */
    suspend fun load(context: Context, main: FontChoice, fallback: FontChoice, generic: String): FontFamily? {
        built[key(main, fallback)]?.let { return it }
        return withContext(Dispatchers.IO) {
            runCatching {
                val mainFamily = family(context, main.source) ?: return@runCatching null
                val fallbackFamily = family(context, fallback.source) ?: return@runCatching null
                val typeface = Typeface.CustomFallbackBuilder(mainFamily)
                    .addCustomFallback(fallbackFamily)
                    .setSystemFallback(generic)
                    .build()
                FontFamily(ComposeTypeface(typeface))
            }.onFailure { Log.w(TAG, "could not build ${main.name} with ${fallback.name}", it) }
                .getOrNull()
                ?.also { built[key(main, fallback)] = it }
        }
    }

    private fun key(main: FontChoice, fallback: FontChoice) = "${main.id}\n${fallback.id}"

    /** The regular and bold fonts of a source as one family. */
    private fun family(context: Context, source: FontSource): NativeFontFamily? {
        val fonts = WEIGHTS.mapNotNull { weight -> font(context, source, weight) }
            .distinctBy { it.style.weight }
        if (fonts.isEmpty()) return null
        val builder = NativeFontFamily.Builder(fonts.first())
        fonts.drop(1).forEach(builder::addFont)
        return builder.build()
    }

    private fun font(context: Context, source: FontSource, weight: Int): Font? = when (source) {
        is FontSource.System -> systemFont(source.typeface, weight)
        is FontSource.Bundled -> source.fonts.minByOrNull { kotlin.math.abs(it.second - weight) }?.let { (id, actual) ->
            Font.Builder(context.resources, id).setWeight(actual).build()
        }
        is FontSource.Google -> source.weights.minByOrNull { kotlin.math.abs(it - weight) }?.let { actual ->
            googleFont(context, source.name, actual)
        }
    }

    /** The font the system draws Latin letters with in `typeface` at `weight`. */
    private fun systemFont(typeface: Typeface, weight: Int): Font? {
        val paint = Paint().apply { this.typeface = Typeface.create(typeface, weight, false) }
        val glyphs = TextRunShaper.shapeTextRun("a", 0, 1, 0, 1, 0f, 0f, false, paint)
        if (glyphs.glyphCount() == 0) return null
        return Font.Builder(glyphs.getFont(0)).setWeight(weight).build()
    }

    /** A font of Google Fonts, from Google Play services, which keeps the files it downloaded. */
    private fun googleFont(context: Context, name: String, weight: Int): Font? {
        val request = FontRequest(
            FontCatalog.PROVIDER_AUTHORITY,
            FontCatalog.PROVIDER_PACKAGE,
            "name=$name&weight=$weight&italic=0&besteffort=true",
            R.array.com_google_android_gms_fonts_certs,
        )
        val result = FontsContractCompat.fetchFonts(context, null, request)
        if (result.statusCode != FontsContractCompat.FontFamilyResult.STATUS_OK) return null
        val info = result.fonts.firstOrNull { it.resultCode == FontsContractCompat.Columns.RESULT_CODE_OK } ?: return null
        return context.contentResolver.openFileDescriptor(info.uri, "r")?.use { descriptor ->
            Font.Builder(descriptor).setWeight(weight).build()
        }
    }

    private val WEIGHTS = listOf(400, 700)
    private const val TAG = "LoomFonts"
}

/**
 * The family of `main`, with `fallback` before the system's fonts once it is
 * built; until then, or when it cannot be, or without a fallback, `main`'s
 * own family.
 */
@Composable
fun rememberFallbackFamily(context: Context, main: FontChoice, fallback: FontChoice, generic: String): FontFamily? {
    if (fallback.id == LoomFonts.systemFallback.id) return main.family
    val family by produceState(FallbackFonts.cached(main, fallback) ?: main.family, main, fallback) {
        value = FallbackFonts.load(context, main, fallback, generic) ?: main.family
    }
    return family
}
