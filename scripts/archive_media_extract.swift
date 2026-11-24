// Native offline readers invoked by archive_media_extract.py's bounded worker.
import Foundation
import ImageIO
import PDFKit
import Vision

let args = CommandLine.arguments
let url = URL(fileURLWithPath: args[2])
let maxBytes = 256 << 10
var output = Data()

func append(_ text: String) {
    guard output.count < maxBytes else { return }
    let chunk = text.utf8.prefix(maxBytes - output.count - 1)
    // Do not cut a UTF-8 code point in half at the output ceiling.
    let complete = String(decoding: chunk, as: UTF8.self)
    let end = complete.utf8.count <= chunk.count ? complete.endIndex : complete.index(before: complete.endIndex)
    output.append(contentsOf: complete[..<end].utf8)
    output.append(10)
}

switch args[1] {
case "image":
    let noCache = [kCGImageSourceShouldCache: false] as CFDictionary
    guard let source = CGImageSourceCreateWithURL(url as CFURL, noCache),
          let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, noCache) as? [CFString: Any],
          let width = properties[kCGImagePropertyPixelWidth] as? Int,
          let height = properties[kCGImagePropertyPixelHeight] as? Int,
          width > 0, height > 0 else { exit(2) }
    guard width <= 40_000_000 / height else { exit(4) }
    let options = [kCGImageSourceCreateThumbnailFromImageAlways: true,
                   kCGImageSourceCreateThumbnailWithTransform: true,
                   kCGImageSourceThumbnailMaxPixelSize: 3500] as CFDictionary
    guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options) else { exit(2) }
    let request = VNRecognizeTextRequest()
    request.recognitionLevel = .accurate
    if let supported = try? request.supportedRecognitionLanguages() {
        request.recognitionLanguages = ["zh-Hans", "en-US"].filter { supported.contains($0) }
    }
    do {
        try VNImageRequestHandler(cgImage: image).perform([request])
    } catch {
        exit(2)
    }
    for observation in request.results ?? [] where output.count < maxBytes {
        if let line = observation.topCandidates(1).first?.string { append(line) }
    }
case "pdf":
    guard let pdf = PDFDocument(url: url), !pdf.isLocked else { exit(2) }
    for index in 0..<min(pdf.pageCount, 256) where output.count < maxBytes {
        autoreleasepool {
            if let text = pdf.page(at: index)?.string { append(text) }
        }
    }
default:
    exit(2)
}
FileHandle.standardOutput.write(output)
