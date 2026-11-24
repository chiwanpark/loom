package com.chiwanpark.loom.ui

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Color
import android.graphics.ImageDecoder
import android.graphics.pdf.PdfRenderer
import android.os.ParcelFileDescriptor
import android.util.LruCache
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.calculateCentroid
import androidx.compose.foundation.gestures.calculatePan
import androidx.compose.foundation.gestures.calculateZoom
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.requiredWidth
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import com.chiwanpark.loom.AgentLink
import com.chiwanpark.loom.LoomApplication
import java.io.File
import java.security.MessageDigest
import kotlin.math.ceil
import kotlin.math.max
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext

/** What a file that the media viewer shows holds. */
enum class MediaKind(val label: String) { Image("Image"), Pdf("PDF") }

/** The largest file the viewer copies to the phone. */
private const val MAX_MEDIA_BYTES: ULong = 67_108_864u

/** The longest side of a decoded image or a rendered page, in pixels. */
private const val MAX_BITMAP_SIDE = 4096

/** How far a pinch zooms in. */
private const val MAX_ZOOM = 6f

/** The kind of media a workspace path holds, by its extension; null for files shown as text. */
fun mediaKindOf(path: String): MediaKind? = when (path.substringAfterLast('/').substringAfterLast('.', "").lowercase()) {
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "heic", "heif", "avif" -> MediaKind.Image
    "pdf" -> MediaKind.Pdf
    else -> null
}

/** A place in the app's cache for a copy of `path` on this agent. */
private fun cacheFile(context: Context, link: AgentLink, path: String): File {
    val digest = MessageDigest.getInstance("SHA-256")
        .digest("${link.host}\n${link.instance}\n$path".toByteArray())
        .joinToString("") { "%02x".format(it) }
        .take(32)
    val dir = File(context.cacheDir, "media").apply { mkdirs() }
    return File(dir, "$digest.${path.substringAfterLast('.', "bin")}")
}

/** An image read from a file, made smaller when it has more pixels than a bitmap should. */
private class LoadedImage(val bitmap: Bitmap, val width: Int, val height: Int, val size: ULong)

/** A PDF file's pages: their sizes in points, and a renderer that draws them one at a time. */
private class PdfDocument(file: File) : AutoCloseable {
    private val descriptor = ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY)
    private val renderer = PdfRenderer(descriptor)
    private val lock = Mutex()
    private val cache = object : LruCache<Pair<Int, Int>, Bitmap>(48 * 1024 * 1024) {
        override fun sizeOf(key: Pair<Int, Int>, value: Bitmap) = value.byteCount
    }
    val size: ULong = file.length().toULong()

    /** Each page's width and height in points. */
    val pages: List<Pair<Int, Int>> = (0 until renderer.pageCount).map { index ->
        renderer.openPage(index).use { it.width to it.height }
    }

    fun cached(page: Int, width: Int): Bitmap? = cache.get(page to width)

    /** The page drawn `width` pixels wide on white, from the cache when it was drawn before. */
    suspend fun render(page: Int, width: Int): Bitmap? = withContext(Dispatchers.IO) {
        lock.withLock {
            cache.get(page to width)?.let { return@withLock it }
            if (closed) return@withLock null
            val (pageWidth, pageHeight) = pages[page]
            val height = max(1, (width.toLong() * pageHeight / max(1, pageWidth)).toInt())
            val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
            bitmap.eraseColor(Color.WHITE)
            renderer.openPage(page).use { it.render(bitmap, null, null, PdfRenderer.Page.RENDER_MODE_FOR_DISPLAY) }
            cache.put(page to width, bitmap)
            bitmap
        }
    }

    private var closed = false

    override fun close() {
        // Waits for a page being drawn; the renderer must not close under it.
        kotlinx.coroutines.runBlocking {
            lock.withLock {
                closed = true
                renderer.close()
                descriptor.close()
            }
        }
    }
}

/** What the media screen loaded. */
private sealed interface Media {
    class Picture(val image: LoadedImage) : Media

    class Document(val pdf: PdfDocument) : Media
}

/**
 * An image or a PDF file of the agent's workspace: the file is copied to the
 * phone's cache, which the screen clears when it closes. Pinching zooms;
 * a zoomed image pans with a finger and a double tap zooms it in or out.
 */
@Composable
fun MediaScreen(app: LoomApplication, link: AgentLink, path: String, kind: MediaKind) {
    val context = LocalContext.current
    val snackbar = remember { SnackbarHostState() }
    var reload by remember { mutableIntStateOf(0) }
    val file = remember(link, path) { cacheFile(context, link, path) }
    val loaded = rememberLoaded(link, reload, path) {
        link.connection().saveFile(path, file.absolutePath, MAX_MEDIA_BYTES)
        withContext(Dispatchers.IO) {
            when (kind) {
                MediaKind.Image -> Media.Picture(decodeImage(file))
                MediaKind.Pdf -> Media.Document(PdfDocument(file))
            }
        }
    }
    DisposableEffect(loaded) {
        onDispose { ((loaded as? Loaded.Ready)?.value as? Media.Document)?.pdf?.close() }
    }
    DisposableEffect(file) {
        onDispose { file.delete() }
    }

    Scaffold(
        topBar = {
            LoomTopBar(
                path.substringAfterLast('/'),
                subtitle = pathSubtitle(link, path),
                subtitleIsPath = true,
                onBack = { app.navigator.pop() },
            ) {
                IconButton(onClick = { reload++ }) { LoomIcon(LoomIcons.Refresh, "Reload") }
            }
        },
        snackbarHost = { LoomSnackbarHost(snackbar) },
    ) { padding ->
        Column(Modifier.fillMaxSize().padding(padding)) {
            LinkBanner(link.state, onRetry = link::retryNow)
            LoadedContent(loaded, onRetry = { reload++ }) { media ->
                when (media) {
                    is Media.Picture -> {
                        InfoRow("${kind.label} · ${media.image.width} × ${media.image.height} · ${formatSize(media.image.size)}")
                        ZoomableImage(media.image.bitmap)
                    }
                    is Media.Document -> PdfPages(media.pdf)
                }
            }
        }
    }
}

@Composable
private fun InfoRow(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.bodySmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 2.dp),
    )
}

/** Decodes an image, scaled down so its longest side has at most `MAX_BITMAP_SIDE` pixels. */
private fun decodeImage(file: File): LoadedImage {
    var width = 0
    var height = 0
    val bitmap = ImageDecoder.decodeBitmap(ImageDecoder.createSource(file)) { decoder, info, _ ->
        width = info.size.width
        height = info.size.height
        val longest = max(width, height)
        if (longest > MAX_BITMAP_SIDE) decoder.setTargetSampleSize(ceil(longest / MAX_BITMAP_SIDE.toFloat()).toInt())
    }
    return LoadedImage(bitmap, width, height, file.length().toULong())
}

/** An image that fits the screen, zoomed by pinching or double tapping, and panned when zoomed. */
@Composable
private fun ZoomableImage(bitmap: Bitmap) {
    val image = remember(bitmap) { bitmap.asImageBitmap() }
    var size by remember { mutableStateOf(IntSize.Zero) }
    var scale by remember { mutableFloatStateOf(1f) }
    var offset by remember { mutableStateOf(Offset.Zero) }

    fun zoomTo(target: Float, centroid: Offset, pan: Offset = Offset.Zero) {
        val next = target.coerceIn(1f, MAX_ZOOM)
        // The point under the fingers stays under them.
        val moved = centroid - (centroid - offset) * (next / scale) + pan
        scale = next
        offset = Offset(
            moved.x.coerceIn(size.width * (1 - next), 0f),
            moved.y.coerceIn(size.height * (1 - next), 0f),
        )
    }

    Box(
        Modifier
            .fillMaxSize()
            .clipToBounds()
            .onSizeChanged { size = it }
            .pointerInput(Unit) {
                detectTapGestures(onDoubleTap = { at -> zoomTo(if (scale > 1.01f) 1f else 2.5f, at) })
            }
            .pointerInput(Unit) {
                awaitEachGesture {
                    awaitFirstDown(requireUnconsumed = false)
                    do {
                        val event = awaitPointerEvent()
                        val pinching = event.changes.count { it.pressed } >= 2
                        if (pinching || scale > 1.01f) {
                            zoomTo(scale * event.calculateZoom(), event.calculateCentroid(), event.calculatePan())
                            event.changes.forEach { if (it.positionChanged()) it.consume() }
                        }
                    } while (event.changes.any { it.pressed })
                }
            },
    ) {
        Image(
            image,
            contentDescription = null,
            contentScale = ContentScale.Fit,
            modifier = Modifier
                .fillMaxSize()
                .graphicsLayer {
                    transformOrigin = TransformOrigin(0f, 0f)
                    scaleX = scale
                    scaleY = scale
                    translationX = offset.x
                    translationY = offset.y
                },
        )
    }
}

private fun androidx.compose.ui.input.pointer.PointerInputChange.positionChanged() = position != previousPosition

/**
 * The pages of a PDF file, as wide as the screen and taller as they need,
 * one after the other. Pinching zooms; a zoomed document scrolls sideways
 * too. Pages are drawn when they come into view, sharper once a pinch ends.
 */
@Composable
private fun PdfPages(pdf: PdfDocument) {
    val list = rememberLazyListState()
    val sideways = rememberScrollState()
    val scope = rememberCoroutineScope()
    var zoom by remember { mutableFloatStateOf(1f) }
    // The zoom pages are drawn at: it follows `zoom` when a pinch ends.
    var settled by remember { mutableFloatStateOf(1f) }
    val pages = pdf.pages
    // The page at the middle of the screen, or the last one at the end.
    val page by remember {
        derivedStateOf {
            val info = list.layoutInfo
            val middle = (info.viewportStartOffset + info.viewportEndOffset) / 2
            val index = if (!list.canScrollForward && info.visibleItemsInfo.isNotEmpty()) info.visibleItemsInfo.last().index
            else info.visibleItemsInfo.firstOrNull { it.offset + it.size > middle }?.index ?: list.firstVisibleItemIndex
            index + 1
        }
    }
    InfoRow(
        listOfNotNull(
            "PDF",
            if (pages.size == 1) "1 page" else "page $page of ${pages.size}",
            formatSize(pdf.size),
            "${(zoom * 100).roundToInt()}%".takeIf { zoom > 1.01f },
        ).joinToString(" · "),
    )
    if (pages.isEmpty()) {
        CenteredMessage("This PDF file has no pages.")
        return
    }
    BoxWithConstraints(
        Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.surfaceContainerHighest)
            .pointerInput(Unit) {
                awaitEachGesture {
                    awaitFirstDown(requireUnconsumed = false, pass = PointerEventPass.Initial)
                    var pinched = false
                    do {
                        val event = awaitPointerEvent(PointerEventPass.Initial)
                        if (event.changes.count { it.pressed } >= 2) {
                            pinched = true
                            val before = zoom
                            zoom = (zoom * event.calculateZoom()).coerceIn(1f, MAX_ZOOM)
                            val centroid = event.calculateCentroid().x
                            val target = (sideways.value + centroid) * (zoom / before) - centroid
                            scope.launch { sideways.scrollTo(target.roundToInt()) }
                            event.changes.forEach { it.consume() }
                        }
                    } while (event.changes.any { it.pressed })
                    if (pinched) settled = zoom
                }
            },
    ) {
        val density = LocalDensity.current
        val width = maxWidth * zoom
        val renderWidth = with(density) { (maxWidth * settled).roundToPx() }.coerceIn(1, MAX_BITMAP_SIDE)
        Box(Modifier.fillMaxSize().horizontalScroll(sideways, enabled = zoom > 1.01f)) {
            LazyColumn(
                Modifier.requiredWidth(width).fillMaxSize(),
                state = list,
                contentPadding = PaddingValues(vertical = 8.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                items(pages.size, key = { it }) { index ->
                    val (pageWidth, pageHeight) = pages[index]
                    val bitmap by produceState(pdf.cached(index, renderWidth), index, renderWidth) {
                        value = pdf.render(index, renderWidth) ?: value
                    }
                    Box(
                        Modifier
                            .fillMaxWidth()
                            .padding(horizontal = 8.dp)
                            .aspectRatio(max(1, pageWidth).toFloat() / max(1, pageHeight))
                            .background(androidx.compose.ui.graphics.Color.White),
                        contentAlignment = Alignment.Center,
                    ) {
                        bitmap?.let {
                            Image(
                                it.asImageBitmap(),
                                contentDescription = "Page ${index + 1}",
                                contentScale = ContentScale.FillBounds,
                                modifier = Modifier.fillMaxSize(),
                            )
                        }
                    }
                }
            }
        }
    }
}
