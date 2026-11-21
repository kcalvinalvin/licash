package org.licash.wallet.ui

import androidx.compose.foundation.layout.BoxScope
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.pulltorefresh.PullToRefreshBox
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import kotlinx.coroutines.launch
import org.licash.wallet.Engine

/**
 * `content`, which pulled down from its top looks for payments now rather
 * than at the next round ([Engine.lookNow]): spinning until it has.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun PullToLook(engine: Engine, modifier: Modifier = Modifier, content: @Composable BoxScope.() -> Unit) {
    val scope = rememberCoroutineScope()
    var looking by remember { mutableStateOf(false) }
    PullToRefreshBox(
        isRefreshing = looking,
        onRefresh = {
            looking = true
            scope.launch {
                try {
                    engine.lookNow()
                } finally {
                    looking = false
                }
            }
        },
        modifier = modifier.testTag("pull-to-look"),
        content = content,
    )
}
