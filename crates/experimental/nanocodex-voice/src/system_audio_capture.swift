import AVFoundation
import CoreMedia
import Foundation
import ScreenCaptureKit

private final class AudioOutput: NSObject, SCStreamOutput {
    private let output = FileHandle.standardOutput

    func stream(
        _ stream: SCStream,
        didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
        of outputType: SCStreamOutputType
    ) {
        guard outputType == .audio, sampleBuffer.isValid else { return }

        do {
            try sampleBuffer.withAudioBufferList { audioBufferList, _ in
                for buffer in audioBufferList {
                    guard let bytes = buffer.mData, buffer.mDataByteSize > 0 else { continue }
                    output.write(Data(bytes: bytes, count: Int(buffer.mDataByteSize)))
                }
            }
        } catch {
            FileHandle.standardError.write(Data("system audio buffer failed: \(error)\n".utf8))
        }
    }
}

@main
private enum SystemAudioCapture {
    static func main() async throws {
        let content = try await SCShareableContent.excludingDesktopWindows(
            false,
            onScreenWindowsOnly: false
        )
        guard let display = content.displays.first else {
            throw NSError(
                domain: "nanocodex.meeting",
                code: 1,
                userInfo: [NSLocalizedDescriptionKey: "no display is available for system audio capture"]
            )
        }

        let filter = SCContentFilter(display: display, excludingWindows: [])
        let configuration = SCStreamConfiguration()
        configuration.capturesAudio = true
        configuration.excludesCurrentProcessAudio = true
        configuration.sampleRate = 24_000
        configuration.channelCount = 1
        configuration.width = 2
        configuration.height = 2
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: 1)

        let output = AudioOutput()
        let stream = SCStream(filter: filter, configuration: configuration, delegate: nil)
        try stream.addStreamOutput(
            output,
            type: .audio,
            sampleHandlerQueue: DispatchQueue(label: "nanocodex.meeting.system-audio")
        )
        try await stream.startCapture()

        await withCheckedContinuation { (_: CheckedContinuation<Void, Never>) in }
    }
}
