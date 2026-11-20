package org.licash.wallet

import android.Manifest
import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.app.job.JobInfo
import android.app.job.JobParameters
import android.app.job.JobScheduler
import android.app.job.JobService
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.SystemClock
import androidx.core.app.NotificationChannelCompat
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import org.licash.wallet.ui.Progress
import org.licash.wallet.ui.stages

/*
 * The app at work out of sight, as far as Android lets it: a foreground
 * service while a payment is under way, started while the app is in sight,
 * and a periodic job, with a network, should the service be stopped. Neither
 * runs with the phone off, in deep sleep or once the app is force stopped.
 */

/** The app's notifications: watching, payments, and deadlines. */
class Notifier(private val context: Context) {
    private val manager = NotificationManagerCompat.from(context)

    init {
        manager.createNotificationChannelsCompat(
            listOf(
                channel(WATCHING, NotificationManagerCompat.IMPORTANCE_LOW, "Working in the background",
                    "Shown while the app catches up with Bitcoin or finishes payments under way out of sight."),
                channel(PAYMENTS, NotificationManagerCompat.IMPORTANCE_DEFAULT, "Payments",
                    "Money received, and payments finished or failed while the app was out of sight."),
                channel(DEADLINES, NotificationManagerCompat.IMPORTANCE_HIGH, "Deadlines",
                    "A payment under way that needs the app open soon to finish."),
            ),
        )
    }

    private fun channel(id: String, importance: Int, name: String, about: String) =
        NotificationChannelCompat.Builder(id, importance).setName(name).setDescription(about).build()

    /**
     * The foreground service's own, while it works: finishing payments, or
     * else catching up with Bitcoin, at the step `ui` is at.
     */
    fun watching(ui: Ui? = null): Notification {
        val b = builder(WATCHING).setOngoing(true).setSilent(true).setOnlyAlertOnce(true)
        val step = ui?.takeIf { it.watch != Watch.NOW && behind(it) }
            ?.let { u -> stages(u).firstOrNull { it.state != Progress.DONE } }
        if (step == null) {
            return b.setContentTitle("Finishing payments under way")
                .setContentText("Licash keeps at it until they are done.")
                .build()
        }
        val percent = step.fraction?.let { (it * 100).toInt() }
        return b.setContentTitle("Catching up with Bitcoin")
            .setContentText("${step.name}: ${step.detail}")
            .setProgress(100, percent ?: 0, percent == null)
            .build()
    }

    /** The foreground service's notification shown anew. */
    fun working(notification: Notification) {
        if (Build.VERSION.SDK_INT >= 33 &&
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        try {
            manager.notify(WATCHING_ID, notification)
        } catch (e: SecurityException) {
            AppLog.warn("a notification", e)
        }
    }

    /** Money in, or a payment finished: `key` names it, each told once. */
    fun payment(key: String, text: String) = post(key, PAYMENT_ID, builder(PAYMENTS).setContentTitle(text))

    /** A payment needing the app open soon. */
    fun deadline(key: String, text: String) = post(
        key,
        DEADLINE_ID,
        builder(DEADLINES)
            .setContentTitle("Open Licash soon")
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setPriority(NotificationCompat.PRIORITY_HIGH),
    )

    /** Never on the lock screen: what the wallet does is for its owner only. */
    private fun builder(channel: String) = NotificationCompat.Builder(context, channel)
        .setSmallIcon(R.drawable.ic_notify)
        .setContentIntent(opening())
        .setAutoCancel(true)
        .setVisibility(NotificationCompat.VISIBILITY_SECRET)

    /** Open the app as from the launcher: its screen as left. */
    private fun opening(): PendingIntent? = context.packageManager.getLaunchIntentForPackage(context.packageName)
        ?.let { PendingIntent.getActivity(context, 0, it, PendingIntent.FLAG_IMMUTABLE) }

    private fun post(tag: String, id: Int, notification: NotificationCompat.Builder) {
        if (Build.VERSION.SDK_INT >= 33 &&
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            return
        }
        try {
            manager.notify(tag, id, notification.build())
        } catch (e: SecurityException) {
            AppLog.warn("a notification", e)
        }
    }

    companion object {
        const val WATCHING = "watching"
        const val PAYMENTS = "payments"
        const val DEADLINES = "deadlines"
        const val WATCHING_ID = 1
        private const val PAYMENT_ID = 2
        private const val DEADLINE_ID = 3
    }
}

/**
 * Keeps the engine at work while a payment is under way, or while it
 * catches up with Bitcoin, until neither: catching up for an hour at most.
 * Started only while the app is in sight, as Android requires; stopped by
 * Android after 6 hours a day since Android 15.
 */
class WatchService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private var watching: Job? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val engine = (application as LicashApp).engine
        try {
            ServiceCompat.startForeground(
                this,
                Notifier.WATCHING_ID,
                engine.notifier.watching(engine.ui.value),
                if (Build.VERSION.SDK_INT >= 29) ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC else 0,
            )
        } catch (e: Exception) {
            // Not allowed now, or out of time for today: the periodic job remains.
            AppLog.warn("watching in the foreground", e)
            stopSelf()
            return START_NOT_STICKY
        }
        if (watching == null) {
            running = true
            engine.keep(true)
            AppLog.info(if (engine.ui.value.watch == Watch.NOW) "watching payments under way" else "catching up out of sight")
            watching = scope.launch {
                val began = SystemClock.elapsedRealtime()
                var shown: String? = null
                // At most as long as Android 15 allows a day, on any Android:
                // a line stuck under way leaves it to the periodic look.
                val done = withTimeoutOrNull(MOST_MS) {
                    engine.ui.first { ui ->
                        val n = engine.notifier.watching(ui)
                        // Its text tells the step and how far: shown anew when it changes.
                        val now = n.extras.getCharSequence(Notification.EXTRA_TEXT)?.toString()
                        if (now != shown) {
                            shown = now
                            engine.notifier.working(n)
                        }
                        // A light client that cannot catch up does not keep it at work for ever.
                        val catching = behind(ui) && SystemClock.elapsedRealtime() - began < CATCH_UP_MS
                        ui.watch != Watch.NOW && !catching
                    }
                }
                AppLog.info(if (done == null) "watched for long enough" else "nothing left to watch or catch up")
                stopSelf()
            }
        }
        return START_NOT_STICKY
    }

    /** Out of the time Android gives a day: stop, as it asks. */
    override fun onTimeout(startId: Int, fgsType: Int) {
        AppLog.info("out of time watching in the foreground today")
        stopSelf()
    }

    override fun onDestroy() {
        if (watching != null) (application as LicashApp).engine.keep(false)
        scope.cancel()
        running = false
        super.onDestroy()
    }

    companion object {
        private const val MOST_MS = 6 * 3600 * 1000L
        private const val CATCH_UP_MS = 3600 * 1000L

        @Volatile
        private var running = false

        /** Watch now, if not yet: only while the app is in sight. */
        fun start(context: Context) {
            if (running) return
            runCatching { ContextCompat.startForegroundService(context, Intent(context, WatchService::class.java)) }
                .onFailure { AppLog.warn("starting to watch", it) }
        }
    }
}

/**
 * A look every 15 minutes or so, with a network, while anything is under
 * way: should the service be stopped, or the app killed. It keeps the engine
 * at work for a round or two in sync with Bitcoin, at most [ROUNDS_MS].
 */
class WatchJob : JobService() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var looking: Job? = null

    override fun onStartJob(params: JobParameters): Boolean {
        val engine = (application as LicashApp).engine
        looking = scope.launch {
            engine.round(ROUNDS_MS)
            jobFinished(params, false)
        }
        return true
    }

    override fun onStopJob(params: JobParameters): Boolean {
        looking?.cancel()
        // Again at its next period.
        return true
    }

    companion object {
        private const val ID = 1
        private const val PERIOD_MS = 15 * 60 * 1000L
        private const val ROUNDS_MS = 4 * 60 * 1000L

        /** Look now and then, across restarts of the phone, if not yet. */
        fun schedule(context: Context) {
            val jobs = context.getSystemService(JobScheduler::class.java) ?: return
            if (jobs.getPendingJob(ID) != null) return
            val job = JobInfo.Builder(ID, ComponentName(context, WatchJob::class.java))
                .setPeriodic(PERIOD_MS)
                .setRequiredNetworkType(JobInfo.NETWORK_TYPE_ANY)
                .setPersisted(true)
                .build()
            runCatching { jobs.schedule(job) }.onFailure { AppLog.warn("scheduling a look now and then", it) }
        }

        fun cancel(context: Context) {
            context.getSystemService(JobScheduler::class.java)?.cancel(ID)
        }
    }
}

/**
 * A look every [PERIOD_MS] or so while the phone rests (idle, with a
 * network), whatever is under way: a round in sync with Bitcoin, which
 * looks for the wallet's coins up to the chain's height, so that coins
 * paid to it are found without the app being opened. At most [ROUNDS_MS].
 */
class ScanJob : JobService() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var looking: Job? = null

    override fun onStartJob(params: JobParameters): Boolean {
        val engine = (application as LicashApp).engine
        looking = scope.launch {
            engine.round(ROUNDS_MS)
            jobFinished(params, false)
        }
        return true
    }

    override fun onStopJob(params: JobParameters): Boolean {
        looking?.cancel()
        return true
    }

    companion object {
        private const val ID = 2
        private const val PERIOD_MS = 4 * 3600 * 1000L
        private const val ROUNDS_MS = 10 * 60 * 1000L

        /** Look while the phone rests, across its restarts, if not yet. */
        fun schedule(context: Context) {
            val jobs = context.getSystemService(JobScheduler::class.java) ?: return
            if (jobs.getPendingJob(ID) != null) return
            val job = JobInfo.Builder(ID, ComponentName(context, ScanJob::class.java))
                .setPeriodic(PERIOD_MS)
                .setRequiresDeviceIdle(true)
                .setRequiredNetworkType(JobInfo.NETWORK_TYPE_ANY)
                .setPersisted(true)
                .build()
            runCatching { jobs.schedule(job) }.onFailure { AppLog.warn("scheduling a look while resting", it) }
        }

        fun cancel(context: Context) {
            context.getSystemService(JobScheduler::class.java)?.cancel(ID)
        }
    }
}
