import AVFoundation
import SwiftUI
import UIKit

/// The back camera, until it sees a QR code: its text, once. Pinching or the
/// slider zooms. The light shows if the camera has one.
struct Scanner: View {
    let onText: (String) -> Void
    @State private var granted = AVCaptureDevice.authorizationStatus(for: .video) == .authorized
    @State private var capture = Capture()
    @State private var camera: AVCaptureDevice?
    // 0 is the widest the camera sees, 1 its closest.
    @State private var zoom: Double = 0
    @State private var light = false
    // Why the camera cannot be used, if it cannot: none, or it failed.
    @State private var broken: String?

    init(onText: @escaping (String) -> Void) {
        self.onText = onText
    }

    var body: some View {
        SwiftUI.Group {
            if !granted {
                Text("Allow the camera to scan codes, or paste one.")
                    .textStyle(Theme.type.bodyMedium)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if let broken {
                Text(broken)
                    .textStyle(Theme.type.bodyMedium)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ZStack(alignment: .bottom) {
                    PreviewView(session: capture.session, pinched: pinch)
                    HStack(alignment: .center, spacing: 0) {
                        Slider(value: Binding(get: { zoom }, set: { to in
                            zoom = to
                            if let camera { zoomTo(device: camera, ratio: ratio(device: camera, linear: to)) }
                        }))
                        .tint(Theme.color.primary)
                        .accessibilityIdentifier("zoom")
                        if camera?.hasTorch == true {
                            TextButton(onClick: {
                                light.toggle()
                                if let camera { torch(device: camera, on: light) }
                            }) {
                                Text(light ? "Light off" : "Light").foregroundStyle(Color.white)
                            }
                        }
                    }
                    .frame(minHeight: 48)
                    .padding(.horizontal, 12)
                    .frame(maxWidth: .infinity)
                    .background(Color.black.opacity(0.35))
                }
                .onAppear { start() }
                // Never waiting on the main thread, nor failing there: a
                // camera that never started has nothing to stop.
                .onDisappear { capture.stop() }
            }
        }
        .task { if !granted { granted = await AVCaptureDevice.requestAccess(for: .video) } }
    }

    private func start() {
        capture.start(
            onText: onText,
            ready: { camera = $0 },
            failed: { e in
                AppLog.warn(line: "starting the camera", e: e)
                broken = "The camera is not available: paste a code instead."
            }
        )
    }

    /// A pinch spread by `factor` since last told: the camera zoomed as much.
    private func pinch(factor: CGFloat) {
        guard let camera else { return }
        zoomTo(device: camera, ratio: camera.videoZoomFactor * factor)
        zoom = linear(device: camera)
    }
}

/// The camera's session, started and stopped on a queue of its own, as both
/// wait on the camera. Left before the camera was ready, its stop, queued
/// after its start, stops it then.
private final class Capture: NSObject, AVCaptureMetadataOutputObjectsDelegate {
    private(set) lazy var session = AVCaptureSession()
    private let queue = DispatchQueue(label: "org.licash.wallet.scanner")
    private var seen = false
    private var onText: (String) -> Void = { _ in }

    /// The back camera started, then `ready` told its device, or `failed`
    /// why it did not start. `onText`, the first code it sees.
    func start(
        onText: @escaping (String) -> Void,
        ready: @escaping (AVCaptureDevice) -> Void,
        failed: @escaping (Error) -> Void
    ) {
        self.onText = onText
        queue.async { [self] in
            do {
                guard let device = AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: .back) else {
                    throw AVError(.deviceNotConnected)
                }
                let input = try AVCaptureDeviceInput(device: device)
                let output = AVCaptureMetadataOutput()
                session.beginConfiguration()
                session.inputs.forEach(session.removeInput)
                session.outputs.forEach(session.removeOutput)
                // Dense codes need more than 640x480 to read.
                if session.canSetSessionPreset(.hd1280x720) { session.sessionPreset = .hd1280x720 }
                guard session.canAddInput(input), session.canAddOutput(output) else {
                    session.commitConfiguration()
                    throw AVError(.unknown)
                }
                session.addInput(input)
                session.addOutput(output)
                session.commitConfiguration()
                guard output.availableMetadataObjectTypes.contains(.qr) else { throw AVError(.unknown) }
                output.setMetadataObjectsDelegate(self, queue: .main)
                output.metadataObjectTypes = [.qr]
                session.startRunning()
                DispatchQueue.main.async { ready(device) }
            } catch {
                DispatchQueue.main.async { failed(error) }
            }
        }
    }

    func stop() {
        queue.async { [session] in session.stopRunning() }
    }

    func metadataOutput(
        _ output: AVCaptureMetadataOutput,
        didOutput metadataObjects: [AVMetadataObject],
        from connection: AVCaptureConnection
    ) {
        guard !seen, let text = (metadataObjects.first as? AVMetadataMachineReadableCodeObject)?.stringValue else { return }
        seen = true
        onText(text)
    }
}

/// What the camera sees, filling its place. A pinch on it tells how far it spread.
private struct PreviewView: UIViewRepresentable {
    let session: AVCaptureSession
    let pinched: (CGFloat) -> Void

    func makeUIView(context: Context) -> Shown {
        let view = Shown()
        view.preview.session = session
        view.preview.videoGravity = .resizeAspectFill
        view.addGestureRecognizer(UIPinchGestureRecognizer(target: context.coordinator, action: #selector(Pinch.scaled(recognizer:))))
        return view
    }

    func updateUIView(_ uiView: Shown, context: Context) {
        context.coordinator.pinched = pinched
    }

    func makeCoordinator() -> Pinch {
        Pinch(pinched: pinched)
    }

    final class Shown: UIView {
        override class var layerClass: AnyClass { AVCaptureVideoPreviewLayer.self }

        var preview: AVCaptureVideoPreviewLayer { layer as! AVCaptureVideoPreviewLayer }
    }

    final class Pinch: NSObject {
        var pinched: (CGFloat) -> Void

        init(pinched: @escaping (CGFloat) -> Void) {
            self.pinched = pinched
        }

        @objc func scaled(recognizer: UIPinchGestureRecognizer) {
            guard recognizer.state == .changed else { return }
            pinched(recognizer.scale)
            recognizer.scale = 1
        }
    }
}

/// `device` zoomed to `ratio`, within what it allows.
private func zoomTo(device: AVCaptureDevice, ratio: CGFloat) {
    guard (try? device.lockForConfiguration()) != nil else { return }
    device.videoZoomFactor = min(max(ratio, device.minAvailableVideoZoomFactor), device.maxAvailableVideoZoomFactor)
    device.unlockForConfiguration()
}

/// The zoom ratio `linear` stands for on `device`: from its widest (0) to its
/// closest (1), in even steps of how wide what it sees is.
private func ratio(device: AVCaptureDevice, linear: Double) -> CGFloat {
    let (widest, closest) = (device.minAvailableVideoZoomFactor, device.maxAvailableVideoZoomFactor)
    return 1 / (1 / widest - (1 / widest - 1 / closest) * linear)
}

/// Where `device`'s zoom stands, from its widest (0) to its closest (1).
private func linear(device: AVCaptureDevice) -> Double {
    let (widest, closest) = (device.minAvailableVideoZoomFactor, device.maxAvailableVideoZoomFactor)
    guard closest > widest else { return 0 }
    return (1 / widest - 1 / device.videoZoomFactor) / (1 / widest - 1 / closest)
}

/// `device`'s light turned `on`, or off.
private func torch(device: AVCaptureDevice, on: Bool) {
    guard (try? device.lockForConfiguration()) != nil else { return }
    device.torchMode = on ? .on : .off
    device.unlockForConfiguration()
}
