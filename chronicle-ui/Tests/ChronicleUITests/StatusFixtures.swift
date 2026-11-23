@testable import ChronicleUI

func statusWithCapture(state: String, paused: Bool) -> StatusResponse {
    StatusResponse(type: "status", ok: true, data: StatusData(
        uptimeSecs: 0, version: "t",
        capture: CaptureStats(
            state: state, activeDisplays: 1, framesCaptured: 0, framesDropped: 0,
            framesProcessed: 0, framesFailed: 0, paused: paused),
        ocr: nil, audio: nil, storage: nil, transcription: nil))
}

func statusWithoutCapture() -> StatusResponse {
    StatusResponse(type: "status", ok: true, data: StatusData(
        uptimeSecs: 0, version: "t", capture: nil, ocr: nil, audio: nil,
        storage: nil, transcription: nil))
}

func statusWithMic(_ micState: MicState, paused: Bool = false) -> StatusResponse {
    StatusResponse(type: "status", ok: true, data: StatusData(
        uptimeSecs: 0, version: "t",
        capture: CaptureStats(
            state: paused ? "paused" : "running", activeDisplays: 1, framesCaptured: 0,
            framesDropped: 0, framesProcessed: 0, framesFailed: 0, paused: paused),
        ocr: nil, audio: AudioStats(segmentsPersisted: 0, micState: micState),
        storage: nil, transcription: nil))
}

func statusDenied(screenRecording: PermissionState) -> StatusResponse {
    StatusResponse(type: "status", ok: true, data: StatusData(
        uptimeSecs: 0, version: "t",
        capture: CaptureStats(
            state: "permission_denied", activeDisplays: 0, framesCaptured: 0, framesDropped: 0,
            framesProcessed: 0, framesFailed: 0, paused: false),
        ocr: nil, audio: nil, storage: nil, transcription: nil,
        permissions: Permissions(screenRecording: screenRecording, screenRecordingCheckedAtMs: 1,
                                 microphone: .authorized, microphoneCheckedAtMs: 1)))
}
