package org.licash.wallet.ui

import android.Manifest
import android.content.pm.PackageManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import android.util.Size
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import androidx.camera.core.Camera
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.google.zxing.BinaryBitmap
import com.google.zxing.DecodeHintType
import com.google.zxing.PlanarYUVLuminanceSource
import com.google.zxing.ReaderException
import com.google.zxing.common.HybridBinarizer
import com.google.zxing.qrcode.QRCodeReader
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import org.licash.wallet.AppLog

/**
 * The back camera, until it sees a QR code: its text, once. Pinching or the
 * slider zooms; the light shows if the camera has one.
 */
@Composable
fun Scanner(modifier: Modifier = Modifier, onText: (String) -> Unit) {
    val context = LocalContext.current
    val owner = LocalLifecycleOwner.current
    val allowed = ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) ==
        PackageManager.PERMISSION_GRANTED
    var granted by remember { mutableStateOf(allowed) }
    val ask = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {
        granted = it
    }
    LaunchedEffect(Unit) { if (!granted) ask.launch(Manifest.permission.CAMERA) }
    if (!granted) {
        Box(modifier, contentAlignment = Alignment.Center) {
            Text("Allow the camera to scan codes, or paste one.", style = MaterialTheme.typography.bodyMedium)
        }
        return
    }
    val seen = remember { AtomicBoolean(false) }
    val worker = remember { Executors.newSingleThreadExecutor() }
    var camera by remember { mutableStateOf<Camera?>(null) }
    // 0 is the widest the camera sees, 1 its closest.
    var zoom by remember { mutableFloatStateOf(0f) }
    var light by remember { mutableStateOf(false) }
    // Why the camera cannot be used, if it cannot: none, or it failed.
    var broken by remember { mutableStateOf<String?>(null) }
    // Left before the camera was ready: not to bind it then.
    val gone = remember { AtomicBoolean(false) }
    DisposableEffect(Unit) {
        onDispose {
            gone.set(true)
            // Never waiting on the main thread, nor failing there: a camera
            // that never started has nothing to unbind.
            val future = ProcessCameraProvider.getInstance(context)
            if (future.isDone) runCatching { future.get().unbindAll() }
            worker.shutdown()
        }
    }
    broken?.let {
        Box(modifier, contentAlignment = Alignment.Center) {
            Text(it, style = MaterialTheme.typography.bodyMedium)
        }
        return
    }
    Box(modifier) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { ctx ->
                val view = PreviewView(ctx)
                val pinch = ScaleGestureDetector(ctx, object : ScaleGestureDetector.SimpleOnScaleGestureListener() {
                    override fun onScale(detector: ScaleGestureDetector): Boolean {
                        val cam = camera ?: return false
                        val ratio = cam.cameraInfo.zoomState.value?.zoomRatio ?: 1f
                        cam.cameraControl.setZoomRatio(ratio * detector.scaleFactor)
                        view.post { zoom = cam.cameraInfo.zoomState.value?.linearZoom ?: zoom }
                        return true
                    }
                })
                view.setOnTouchListener { touched, event ->
                    pinch.onTouchEvent(event)
                    if (event.action == MotionEvent.ACTION_UP) touched.performClick()
                    true
                }
                val future = ProcessCameraProvider.getInstance(ctx)
                future.addListener({
                    val preview = Preview.Builder().build()
                    preview.setSurfaceProvider(view.surfaceProvider)
                    // Dense codes need more than the default 640x480 to read.
                    val resolution = ResolutionSelector.Builder()
                        .setResolutionStrategy(
                            ResolutionStrategy(Size(1280, 720), ResolutionStrategy.FALLBACK_RULE_CLOSEST_HIGHER_THEN_LOWER),
                        )
                        .build()
                    val analysis = ImageAnalysis.Builder()
                        .setResolutionSelector(resolution)
                        .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                        .build()
                    val reader = QRCodeReader()
                    analysis.setAnalyzer(worker) { image ->
                        val text = image.use { decode(it, reader) }
                        if (text != null && seen.compareAndSet(false, true)) view.post { onText(text) }
                    }
                    if (gone.get()) return@addListener
                    runCatching {
                        val provider = future.get()
                        provider.unbindAll()
                        camera = provider.bindToLifecycle(owner, CameraSelector.DEFAULT_BACK_CAMERA, preview, analysis)
                    }.onFailure {
                        AppLog.warn("starting the camera", it)
                        broken = "The camera is not available: paste a code instead."
                    }
                }, ContextCompat.getMainExecutor(ctx))
                view
            },
        )
        Row(
            Modifier.align(Alignment.BottomCenter).fillMaxWidth()
                .background(Color.Black.copy(alpha = 0.35f)).padding(horizontal = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Slider(
                value = zoom,
                onValueChange = { zoom = it; camera?.cameraControl?.setLinearZoom(it) },
                modifier = Modifier.weight(1f).testTag("zoom"),
            )
            if (camera?.cameraInfo?.hasFlashUnit() == true) {
                TextButton(onClick = { light = !light; camera?.cameraControl?.enableTorch(light) }) {
                    Text(if (light) "Light off" else "Light", color = Color.White)
                }
            }
        }
    }
}

private fun decode(image: ImageProxy, reader: QRCodeReader): String? {
    val plane = image.planes[0]
    val buffer = plane.buffer
    val luma = ByteArray(buffer.remaining()).also { buffer.get(it) }
    val source = PlanarYUVLuminanceSource(
        luma, plane.rowStride, image.height, 0, 0, image.width, image.height, false,
    )
    return try {
        reader.decode(BinaryBitmap(HybridBinarizer(source)), mapOf(DecodeHintType.TRY_HARDER to true)).text
    } catch (_: ReaderException) {
        null
    } finally {
        reader.reset()
    }
}
