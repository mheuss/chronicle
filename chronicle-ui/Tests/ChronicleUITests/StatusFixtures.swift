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
