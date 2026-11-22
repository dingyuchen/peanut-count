// Makes small looping GIF previews (GitHub renders GIFs inline; it only plays
// videos uploaded through its web UI) and a contact sheet of the stills.
// Usage: swift previews.swift <clip-dir>
import AppKit
import AVFoundation
import Foundation
import ImageIO
import UniformTypeIdentifiers

let folder = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
let previews = folder.appendingPathComponent("previews", isDirectory: true)
try FileManager.default.createDirectory(at: previews, withIntermediateDirectories: true)
let files = try FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: nil)

// One frame per half second of source, shown for a quarter second: 2x speed.
for clip in files.filter({ $0.lastPathComponent.range(of: #"^[0-9]{2}-.*\.mp4$"#, options: .regularExpression) != nil })
    .sorted(by: { $0.path < $1.path }) {
    let asset = AVURLAsset(url: clip)
    let duration = try await asset.load(.duration).seconds
    let generator = AVAssetImageGenerator(asset: asset)
    generator.appliesPreferredTrackTransform = true
    generator.maximumSize = CGSize(width: 240, height: 520)
    generator.requestedTimeToleranceBefore = CMTime(seconds: 0.1, preferredTimescale: 600)
    generator.requestedTimeToleranceAfter = generator.requestedTimeToleranceBefore
    let times = stride(from: 0.0, to: duration, by: 0.5).map { CMTime(seconds: $0, preferredTimescale: 600) }
    let target = previews.appendingPathComponent(clip.deletingPathExtension().lastPathComponent + ".gif")
    let gif = CGImageDestinationCreateWithURL(target as CFURL, UTType.gif.identifier as CFString, times.count, nil)!
    CGImageDestinationSetProperties(gif, [kCGImagePropertyGIFDictionary: [kCGImagePropertyGIFLoopCount: 0]] as CFDictionary)
    for time in times {
        let frame = try await generator.image(at: time).image
        CGImageDestinationAddImage(gif, frame, [kCGImagePropertyGIFDictionary: [kCGImagePropertyGIFDelayTime: 0.25]] as CFDictionary)
    }
    guard CGImageDestinationFinalize(gif) else { fatalError("Could not write \(target.path)") }
    let size = (try FileManager.default.attributesOfItem(atPath: target.path)[.size] as! Int) / 1024
    print("\(target.lastPathComponent)\t\(times.count) frames\t\(size) KB")
}

// Contact sheet: four stills per row, file name under each.
let stills = try FileManager.default.contentsOfDirectory(at: folder.appendingPathComponent("stills"), includingPropertiesForKeys: nil)
    .filter { $0.pathExtension == "png" }.sorted { $0.path < $1.path }
if !stills.isEmpty {
    let tile = CGSize(width: 277, height: 620), columns = 4
    let rows = (stills.count + columns - 1) / columns
    let sheet = NSImage(size: CGSize(width: tile.width * CGFloat(columns), height: tile.height * CGFloat(rows)))
    sheet.lockFocus()
    NSColor(white: 0.9, alpha: 1).setFill()
    NSRect(origin: .zero, size: sheet.size).fill()
    let caption: [NSAttributedString.Key: Any] = [.font: NSFont.systemFont(ofSize: 13), .foregroundColor: NSColor.black]
    for (index, url) in stills.enumerated() {
        guard let image = NSImage(contentsOf: url) else { continue }
        let scale = min(252 / image.size.width, 568 / image.size.height)
        let size = CGSize(width: image.size.width * scale, height: image.size.height * scale)
        let x = CGFloat(index % columns) * tile.width
        let top = sheet.size.height - CGFloat(index / columns) * tile.height
        image.draw(in: NSRect(x: x + (tile.width - size.width) / 2, y: top - 12 - size.height, width: size.width, height: size.height))
        let name = url.deletingPathExtension().lastPathComponent as NSString
        let width = name.size(withAttributes: caption).width
        name.draw(at: NSPoint(x: x + (tile.width - width) / 2, y: top - 606), withAttributes: caption)
    }
    sheet.unlockFocus()
    let rep = NSBitmapImageRep(data: sheet.tiffRepresentation!)!
    try rep.representation(using: .png, properties: [:])!.write(to: folder.appendingPathComponent("contact-sheet.png"))
    print("contact-sheet.png\t\(stills.count) stills")
}
