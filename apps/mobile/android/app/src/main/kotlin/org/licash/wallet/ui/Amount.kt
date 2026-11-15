package org.licash.wallet.ui

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.OffsetMapping
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.input.TransformedText
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.sp

/** The most digits an amount in sat takes: 21 million bitcoin is 16. */
private const val MOST_DIGITS = 16

/** What is typed, as an amount's digits: no leading zeros, at most [MOST_DIGITS]. */
internal fun amountDigits(typed: String): String = typed.filter(Char::isDigit).trimStart('0').take(MOST_DIGITS)

/**
 * `digits` as shown, thousands apart (`12,345`), and where each of its
 * offsets lands in it: `at[i]` for the offset before digit `i`, `at[n]` past
 * the last.
 */
internal class Grouped(val text: String, val at: IntArray)

internal fun grouped(digits: String): Grouped {
    val out = StringBuilder()
    val at = IntArray(digits.length + 1)
    for (i in digits.indices) {
        if (i > 0 && (digits.length - i) % 3 == 0) out.append(',')
        at[i] = out.length
        out.append(digits[i])
    }
    at[digits.length] = out.length
    return Grouped(out.toString(), at)
}

/** The digits typed shown as an amount: `12,345 sat`, the cursor among the digits. */
private object AsSat : VisualTransformation {
    override fun filter(text: AnnotatedString): TransformedText {
        val digits = text.text
        if (digits.isEmpty()) return TransformedText(text, OffsetMapping.Identity)
        val g = grouped(digits)
        val shown = g.text + " sat"
        return TransformedText(
            AnnotatedString(shown),
            object : OffsetMapping {
                override fun originalToTransformed(offset: Int) = g.at[offset.coerceIn(0, digits.length)]

                override fun transformedToOriginal(offset: Int): Int =
                    g.at.indexOfFirst { it >= offset }.let { if (it < 0) digits.length else it }
            },
        )
    }
}

/**
 * An amount in sat, typed straight in where it shows, big: focused with its
 * keyboard up as it first shows. `done`, if any, is the keyboard's action
 * (else it only hides the keyboard): never a payment, which a button makes.
 */
@Composable
fun AmountField(
    amount: Long?,
    onAmount: (Long?) -> Unit,
    modifier: Modifier = Modifier,
    action: ImeAction = ImeAction.Done,
    done: (() -> Unit)? = null,
) {
    val start = amount?.toString().orEmpty()
    var value by remember { mutableStateOf(TextFieldValue(start, TextRange(start.length))) }
    val focus = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current
    LaunchedEffect(Unit) {
        focus.requestFocus()
        keyboard?.show()
    }
    val digits = value.text
    // Smaller as it grows, to stay on one line on a narrow phone.
    val size = when (grouped(digits).text.length + 4) {
        in 0..9 -> 56
        in 10..11 -> 44
        in 12..14 -> 36
        else -> 28
    }
    val style = Figures.copy(
        fontSize = size.sp,
        lineHeight = (size + 4).sp,
        textAlign = TextAlign.Center,
        color = MaterialTheme.colorScheme.onSurface,
    )
    BasicTextField(
        value = value,
        onValueChange = { typed ->
            val d = amountDigits(typed.text)
            // The cursor kept at the end: an amount is typed, not edited inside.
            value = TextFieldValue(d, TextRange(d.length))
            onAmount(d.toLongOrNull()?.takeIf { it > 0 })
        },
        modifier = modifier.fillMaxWidth().focusRequester(focus).testTag("amount"),
        textStyle = style,
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number, imeAction = action),
        keyboardActions = if (done == null) KeyboardActions.Default else KeyboardActions(
            onDone = { done() },
            onGo = { done() },
            onNext = { done() },
        ),
        visualTransformation = AsSat,
        cursorBrush = SolidColor(MaterialTheme.colorScheme.primary),
        decorationBox = { field ->
            Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.Center) {
                if (digits.isEmpty()) {
                    Text("0 sat", style = style.copy(color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.4f)))
                }
                field()
            }
        },
    )
}
