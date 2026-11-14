package org.licash.wallet.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp

private val Ink = Color(0xFF0F1214)
private val Slate = Color(0xFF181D20)
private val Lichen = Color(0xFFA8E06C)
private val Moss = Color(0xFF6FB7A8)
private val Mist = Color(0xFFE7ECE9)
private val Dim = Color(0xFF8A9590)

private val dark = darkColorScheme(
    primary = Lichen,
    onPrimary = Ink,
    secondary = Moss,
    onSecondary = Ink,
    background = Ink,
    onBackground = Mist,
    surface = Ink,
    onSurface = Mist,
    surfaceVariant = Slate,
    onSurfaceVariant = Dim,
    surfaceContainer = Slate,
    surfaceContainerHigh = Color(0xFF20262A),
    outline = Color(0xFF39423F),
    error = Color(0xFFFF8A80),
)

private val light = lightColorScheme(
    primary = Color(0xFF3F7D20),
    onPrimary = Color.White,
    secondary = Color(0xFF2F7F70),
    background = Color(0xFFF6F4EE),
    onBackground = Color(0xFF151916),
    surface = Color(0xFFF6F4EE),
    onSurface = Color(0xFF151916),
    surfaceVariant = Color(0xFFEAE7DF),
    onSurfaceVariant = Color(0xFF5E665F),
    surfaceContainer = Color(0xFFEDEAE2),
    surfaceContainerHigh = Color(0xFFE4E1D8),
    outline = Color(0xFFC9C5BA),
)

/** Big figures with digits of one width, so that amounts do not jump. */
val Figures = TextStyle(
    fontSize = 56.sp,
    lineHeight = 60.sp,
    fontWeight = FontWeight.SemiBold,
    letterSpacing = (-1).sp,
    fontFeatureSettings = "tnum",
)

@Composable
fun LicashTheme(content: @Composable () -> Unit) {
    val base = Typography()
    val typography = base.copy(
        titleLarge = base.titleLarge.copy(fontWeight = FontWeight.SemiBold),
        labelLarge = base.labelLarge.copy(fontWeight = FontWeight.SemiBold, fontSize = 16.sp),
    )
    MaterialTheme(
        colorScheme = if (isSystemInDarkTheme()) dark else light,
        typography = typography,
        content = content,
    )
}
