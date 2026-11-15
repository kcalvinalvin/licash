package org.licash.wallet

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.consumeWindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.imePadding
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import org.licash.core.Activity
import org.licash.wallet.ui.ActivityDetails
import org.licash.wallet.ui.History
import org.licash.wallet.ui.Home
import org.licash.wallet.ui.LicashTheme
import org.licash.wallet.ui.Money
import org.licash.wallet.ui.Onboarding
import org.licash.wallet.ui.Receive
import org.licash.wallet.ui.Send
import org.licash.wallet.ui.Settings
import org.licash.wallet.ui.Wordmark

private sealed interface Screen {
    data object Home : Screen
    data object Receive : Screen

    /** Sending, or with `cashOut`, a hub, paying all the notes there out. */
    data class Send(
        val link: String?,
        val cashOut: String? = null,
        val from: Screen = Home,
        /** The address of this wallet's that `link` pays, if it does. */
        val own: String? = null,
    ) : Screen
    data object Settings : Screen

    /**
     * Where the money is, opened from `from`: all of it, or `at` one
     * place, a hub's URL or what is on Bitcoin.
     */
    data class Money(val from: Screen = Home, val at: String? = null) : Screen

    /** The history: what came in and went out. */
    data object History : Screen

    /** One line of the history, opened from `from`. */
    data class Details(val activity: Activity, val from: Screen = History) : Screen
}

/** The screen back goes to from `screen`. */
private fun parent(screen: Screen): Screen = when (screen) {
    is Screen.Send -> screen.from
    is Screen.Money -> if (screen.at != null) screen.copy(at = null) else screen.from
    is Screen.Details -> screen.from
    else -> Screen.Home
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
    // Asked as a payment goes under way, to tell of it out of sight: Android
    // asks at most twice, then no more once refused.
    val context = LocalContext.current
    val ask = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        AppLog.info(if (granted) "notifications allowed" else "notifications not allowed")
    }
    val watching = ui.watch != Watch.NONE
    LaunchedEffect(watching) {
        val missing = Build.VERSION.SDK_INT >= 33 &&
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        if (watching && missing) ask.launch(Manifest.permission.POST_NOTIFICATIONS)
    }
    LaunchedEffect(ui.wallet) { if (!ui.wallet) screen = Screen.Home }
    LaunchedEffect(link, ui.wallet) {
        if (link != null && ui.wallet) {
            screen = Screen.Send(link)
            linkShown()
        }
    }
    BackHandler(enabled = screen != Screen.Home) { screen = parent(screen) }
    Scaffold(
        containerColor = MaterialTheme.colorScheme.background,
        snackbarHost = { SnackbarHost(snackbar) },
    ) { padding ->
        // Test tags as resource ids, for the end-to-end tests to find.
        // Above the keyboard, as it shows: what a screen asks to type and its button in sight.
        Box(
            Modifier.fillMaxSize().padding(padding).consumeWindowInsets(padding).imePadding()
                .semantics { testTagsAsResourceId = true },
        ) {
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
                        money = { screen = Screen.Money(at = it) },
                        history = { screen = Screen.History },
                        open = { screen = Screen.Details(it, Screen.Home) },
                    )
                    Screen.History -> History(ui, open = { screen = Screen.Details(it) }) { screen = Screen.Home }
                    Screen.Receive -> Receive(engine) { screen = Screen.Home }
                    is Screen.Send -> Send(engine, ui, s.link, s.cashOut, s.own) { screen = s.from }
                    Screen.Settings -> Settings(engine, ui, balance = { screen = Screen.Money(Screen.Settings) }) {
                        screen = Screen.Home
                    }
                    is Screen.Money -> Money(
                        engine,
                        ui,
                        at = s.at,
                        go = { screen = s.copy(at = it) },
                        cashOut = { screen = Screen.Send(null, it, s) },
                        payOwn = { link, own -> screen = Screen.Send(link, from = s, own = own) },
                    ) { screen = s.from }
                    is Screen.Details -> ActivityDetails(engine, ui, s.activity) { screen = s.from }
                }
            }
        }
    }
}
