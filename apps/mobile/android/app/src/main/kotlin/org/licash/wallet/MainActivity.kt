package org.licash.wallet

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import org.licash.wallet.ui.Home
import org.licash.wallet.ui.LicashTheme
import org.licash.wallet.ui.Onboarding
import org.licash.wallet.ui.Receive
import org.licash.wallet.ui.Send
import org.licash.wallet.ui.Settings
import org.licash.wallet.ui.Wordmark

private sealed interface Screen {
    data object Home : Screen
    data object Receive : Screen
    data class Send(val link: String?) : Screen
    data object Settings : Screen
}

class MainActivity : ComponentActivity() {
    /** A `bitcoin:` link the app was opened with, not yet shown. */
    private var link by mutableStateOf<String?>(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        link = linkOf(intent)
        val engine = (application as LicashApp).engine
        setContent {
            LicashTheme { App(engine, link) { link = null } }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        linkOf(intent)?.let { link = it }
    }

    private fun linkOf(intent: Intent?): String? =
        intent?.takeIf { it.action == Intent.ACTION_VIEW }?.dataString
}

@Composable
private fun App(engine: Engine, link: String?, linkShown: () -> Unit) {
    val ui by engine.ui.collectAsStateWithLifecycle()
    var screen by remember { mutableStateOf<Screen>(Screen.Home) }
    val snackbar = remember { SnackbarHostState() }
    LaunchedEffect(Unit) { engine.events.collect { snackbar.showSnackbar(it.text) } }
    LaunchedEffect(ui.wallet) { if (!ui.wallet) screen = Screen.Home }
    LaunchedEffect(link, ui.wallet) {
        if (link != null && ui.wallet) {
            screen = Screen.Send(link)
            linkShown()
        }
    }
    BackHandler(enabled = screen != Screen.Home) { screen = Screen.Home }
    Scaffold(
        containerColor = MaterialTheme.colorScheme.background,
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        // Test tags as resource ids, for the end-to-end tests to find.
        Box(Modifier.fillMaxSize().padding(padding).semantics { testTagsAsResourceId = true }) {
            when {
                !ui.loaded -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                    Wordmark(size = 40)
                }
                !ui.wallet -> Onboarding(engine, ui)
                else -> when (val s = screen) {
                    Screen.Home -> Home(
                        engine,
                        ui,
                        receive = { screen = Screen.Receive },
                        send = { screen = Screen.Send(null) },
                        settings = { screen = Screen.Settings },
                    )
                    Screen.Receive -> Receive(engine) { screen = Screen.Home }
                    is Screen.Send -> Send(engine, ui, s.link) { screen = Screen.Home }
                    Screen.Settings -> Settings(engine, ui) { screen = Screen.Home }
                }
            }
        }
    }
}
