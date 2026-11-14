package com.chiwanpark.loom.ui

import androidx.annotation.DrawableRes
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Icon
import androidx.compose.material3.LocalContentColor
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.composables.icons.lucide.R as Lucide

/** The app's icons: Lucide, the set the desktop client uses. */
object LoomIcons {
    val Archive = Lucide.drawable.lucide_ic_archive
    val ArrowDown = Lucide.drawable.lucide_ic_arrow_down
    val Back = Lucide.drawable.lucide_ic_arrow_left
    val Ban = Lucide.drawable.lucide_ic_ban
    val Brain = Lucide.drawable.lucide_ic_brain
    val Check = Lucide.drawable.lucide_ic_check
    val CheckCheck = Lucide.drawable.lucide_ic_check_check
    val ChevronRight = Lucide.drawable.lucide_ic_chevron_right
    val ChevronsUp = Lucide.drawable.lucide_ic_chevrons_up
    val Circle = Lucide.drawable.lucide_ic_circle
    val CircleCheck = Lucide.drawable.lucide_ic_circle_check
    val CircleDot = Lucide.drawable.lucide_ic_circle_dot
    val CircleX = Lucide.drawable.lucide_ic_circle_x
    val CornerDownRight = Lucide.drawable.lucide_ic_corner_down_right
    val Image = Lucide.drawable.lucide_ic_image
    val Info = Lucide.drawable.lucide_ic_info
    val Loader = Lucide.drawable.lucide_ic_loader
    val LoaderCircle = Lucide.drawable.lucide_ic_loader_circle
    val Lock = Lucide.drawable.lucide_ic_lock
    val More = Lucide.drawable.lucide_ic_ellipsis_vertical
    val Notes = Lucide.drawable.lucide_ic_notebook_pen
    val Plus = Lucide.drawable.lucide_ic_plus
    val Refresh = Lucide.drawable.lucide_ic_refresh_cw
    val Send = Lucide.drawable.lucide_ic_send_horizontal
    val Stop = Lucide.drawable.lucide_ic_square
    val Tasks = Lucide.drawable.lucide_ic_list_todo
    val TriangleAlert = Lucide.drawable.lucide_ic_triangle_alert
    val X = Lucide.drawable.lucide_ic_x
}

/** A Lucide icon, 24 dp by default, in the current content color. */
@Composable
fun LoomIcon(
    @DrawableRes icon: Int,
    contentDescription: String?,
    modifier: Modifier = Modifier,
    size: Dp = 24.dp,
    tint: Color = LocalContentColor.current,
) {
    Icon(painterResource(icon), contentDescription, modifier.size(size), tint)
}

/** A loader icon that turns, for work in progress. */
@Composable
fun SpinningIcon(
    contentDescription: String?,
    modifier: Modifier = Modifier,
    size: Dp = 16.dp,
    tint: Color = LocalContentColor.current,
) {
    val turn by rememberInfiniteTransition(label = "spin").animateFloat(
        initialValue = 0f,
        targetValue = 360f,
        animationSpec = infiniteRepeatable(tween(1000, easing = LinearEasing), RepeatMode.Restart),
        label = "angle",
    )
    LoomIcon(LoomIcons.LoaderCircle, contentDescription, modifier.rotate(turn), size, tint)
}
