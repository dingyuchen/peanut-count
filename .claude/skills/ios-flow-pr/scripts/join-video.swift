// Joins every NN-*.mp4 clip in a folder, in order, into full-flow.mp4 (passthrough, no re-encode).
// Usage: swift join-video.swift <clip-dir>
import AVFoundation
import Foundation

let folder = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
let clips = try FileManager.default.contentsOfDirectory(at: folder, includingPropertiesForKeys: nil)
    .filter { $0.lastPathComponent.range(of: #"^[0-9]{2}-.*\.mp4$"#, options: .regularExpression) != nil }
    .sorted { $0.lastPathComponent < $1.lastPathComponent }
guard !clips.isEmpty else { fatalError("No NN-*.mp4 clips in \(folder.path)") }
let composition = AVMutableComposition()
let video = composition.addMutableTrack(withMediaType: .video, preferredTrackID: kCMPersistentTrackID_Invalid)!
var position = CMTime.zero
var durations = ""
for clip in clips {
    let asset = AVURLAsset(url: clip)  // must outlive the insert below
    let track = try await asset.loadTracks(withMediaType: .video).first!
    let range = try await track.load(.timeRange)
    if position == .zero { video.preferredTransform = try await track.load(.preferredTransform) }
    try video.insertTimeRange(range, of: track, at: position)
    position = CMTimeAdd(position, range.duration)
    durations += "\(clip.lastPathComponent)\t\(Int(range.duration.seconds.rounded()))\n"
    print("\(clip.lastPathComponent)\t\(String(format: "%.1f", range.duration.seconds)) s")
}
let target = folder.appendingPathComponent("full-flow.mp4")
try? FileManager.default.removeItem(at: target)
try await AVAssetExportSession(asset: composition, presetName: AVAssetExportPresetPassthrough)!.export(to: target, as: .mp4)
durations += "full-flow.mp4\t\(Int(position.seconds.rounded()))\n"
try durations.write(to: folder.appendingPathComponent("durations.tsv"), atomically: true, encoding: .utf8)
print("full-flow.mp4\t\(String(format: "%.1f", position.seconds)) s")
