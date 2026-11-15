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
    primaryContainer = Color(0xFF2C3A22),
    onPrimaryContainer = Lichen,
    secondaryContainer = Color(0xFF213431),
    onSecondaryContainer = Moss,
    tertiary = Moss,
    surfaceContainerLowest = Ink,
    surfaceContainerLow = Color(0xFF14181B),
    surfaceContainer = Slate,
    surfaceContainerHigh = Color(0xFF20262A),
    surfaceContainerHighest = Color(0xFF283033),
    outline = Color(0xFF39423F),
    outlineVariant = Color(0xFF2A3230),
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
    primaryContainer = Color(0xFFD9EDC4),
    onPrimaryContainer = Color(0xFF1E3D0E),
    secondaryContainer = Color(0xFFD3EAE4),
    onSecondaryContainer = Color(0xFF173F38),
    tertiary = Color(0xFF2F7F70),
    surfaceContainerLowest = Color(0xFFFFFFFF),
    surfaceContainerLow = Color(0xFFF1EEE7),
    surfaceContainer = Color(0xFFEDEAE2),
    surfaceContainerHigh = Color(0xFFE4E1D8),
    surfaceContainerHighest = Color(0xFFDDDAD0),
    outline = Color(0xFFC9C5BA),
    outlineVariant = Color(0xFFDDD9CE),
)

/** Big figures with digits of one width, so that amounts do not jump. */
val Figures = TextStyle(
    fontSize = 56.sp,
    lineHeight = 60.sp,
    fontWeight = FontWeight.SemiBold,
    letterSpacing = (-1).sp,
    fontFeatureSettings = "tnum",
)

/** The amount a screen is about, under its title: smaller than Home's. */
val Heading = TextStyle(
    fontSize = 36.sp,
    lineHeight = 44.sp,
    fontWeight = FontWeight.SemiBold,
    letterSpacing = (-0.5).sp,
    fontFeatureSettings = "tnum",
)

/** An amount or a value in a row: digits of one width, to line up down a list. */
val Amounts = TextStyle(
    fontSize = 16.sp,
    lineHeight = 24.sp,
    letterSpacing = 0.5.sp,
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
