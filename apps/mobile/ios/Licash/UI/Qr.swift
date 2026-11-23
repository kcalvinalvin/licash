import CoreImage
import CoreImage.CIFilterBuiltins
import SwiftUI

/// What turns codes into pixels: made once, as it is costly to make.
private let context = CIContext()

/// `text` as a QR code, one pixel a module: drawn scaled, unfiltered. None
/// if it is too long for a QR code.
func qrBitmap(text: String) -> CGImage? {
    let filter = CIFilter.qrCodeGenerator()
    filter.message = Data(text.utf8)
    filter.correctionLevel = "M"
    guard let image = filter.outputImage, let drawn = context.createCGImage(image, from: image.extent) else { return nil }
    // CoreImage sets the code in a margin of its own: cut to the code, whose
    // finder patterns reach its corners.
    let (w, h) = (drawn.width, drawn.height)
    var pixels = [UInt8](repeating: 255, count: w * h)
    let drew = pixels.withUnsafeMutableBytes { bytes in
        guard let gray = CGContext(
            data: bytes.baseAddress, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w,
            space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue
        ) else { return false }
        gray.draw(drawn, in: CGRect(x: 0, y: 0, width: w, height: h))
        return true
    }
    guard drew else { return nil }
    var (top, left, bottom, right) = (h, w, -1, -1)
    for y in 0..<h {
        for x in 0..<w where pixels[y * w + x] < 128 {
            (top, left, bottom, right) = (min(top, y), min(left, x), max(bottom, y), max(right, x))
        }
    }
    guard bottom >= 0 else { return nil }
    return drawn.cropping(to: CGRect(x: left, y: top, width: right - left + 1, height: bottom - top + 1))
}

/// A QR code on white, as scanners read best.
struct QrCode: View {
    let text: String
    let size: CGFloat

    init(text: String, size: CGFloat = 280) {
        self.text = text
        self.size = size
    }

    var body: some View {
        SwiftUI.Group {
            if let bitmap = qrBitmap(text: text) {
                Image(bitmap, scale: 1, label: Text("QR code"))
                    .interpolation(.none)
                    .resizable()
                    .aspectRatio(1, contentMode: .fit)
                    .frame(maxWidth: size)
            }
        }
        .padding(18)
        .background(Color.white, in: RoundedRectangle(cornerRadius: 24, style: .continuous))
    }
}
