import Foundation

/// The words the transcription banner says, split out from the view that
/// renders them.
///
/// A value type rather than a pile of computed properties on a `private
/// struct: View`, because the branches here encode decisions that are easy to
/// get quietly wrong — the corrupt-model wording especially — and a private
/// view is unreachable from tests. Rendering (icons, tint, the bar) stays in
/// `TranscriptionBanner`; what to *say* lives here.
struct TranscriptionBannerCopy {
    struct Action: Equatable {
        let title: String
        let variant: String
    }

    let state: TranscriptionState
    let variant: String
    private let loadedVariant: String?
    private let errorText: String?
    private let models: [ModelEntry]

    /// `nil` transcription means a daemon too old to send the block. The
    /// banner is not shown in that case, so this is only a safe fallback.
    init(_ transcription: TranscriptionStats?) {
        state = transcription?.state ?? .missing
        variant = transcription?.variant ?? "base"
        loadedVariant = transcription?.loadedVariant
        errorText = transcription?.error
        models = transcription?.models ?? []
    }

    /// What to say when the daemon REFUSES a provision request.
    ///
    /// `ok: false` collapses three cases the wire cannot tell apart — an
    /// unknown variant, a provision already in flight, and a daemon not yet
    /// ready. The status read back with the reply is the only thing that
    /// separates them, so it leads.
    ///
    /// Static and shared because both surfaces that can request a provision
    /// render this. Two copies of one sentence drift, which is the same hazard
    /// that put the rest of this copy in one place.
    static func rejection(_ status: TranscriptionStats?) -> String {
        TranscriptionBannerCopy(status).showsProgress
            ? "Already working on it…"
            : "The daemon turned that request down. Try again in a moment."
    }

    /// The request never reached the daemon at all — a different failure from
    /// one the daemon considered and declined.
    static let unreachable = "Couldn't reach the daemon."

    var isFailure: Bool { state == .error }

    var showsProgress: Bool {
        switch state {
        case .downloading, .verifying, .loading:
            return true
        case .missing, .error, .ready, .unknown:
            return false
        }
    }

    var icon: String {
        switch state {
        case .error:
            return "exclamationmark.triangle"
        case .downloading, .verifying, .loading:
            return "arrow.down.circle"
        case .ready, .unknown:
            // Not just tidiness: `body` can re-render on a `.ready` status
            // before the `.task(id:)` that calls `evaluate` runs, so there is
            // a pass where the banner is still on screen showing this copy.
            // "Transcription is on" beside the off icon would be a lie for
            // that frame.
            return "waveform"
        case .missing:
            return "waveform.slash"
        }
    }

    var headline: String {
        switch state {
        case .missing:
            return "Audio transcription is off. Download the \(variant) model (\(sizeLabel)) to enable it."
        case .downloading:
            return "Downloading \(variant) model…"
        case .verifying, .loading:
            return "Preparing \(variant) model…"
        case .error:
            // Two different failures reach `.error`, and the design's §4.2
            // copy only described one of them. A switch that failed still has
            // an engine serving, so name it. A corrupt-but-present model at
            // boot walks Loading → Error with `loadedVariant` nil and NO
            // download involved — "Download failed" would be a lie there, so
            // the wording stays neutral and `detail` carries the daemon's own
            // reason (plan Decisions, 2026-08-01).
            if let loadedVariant {
                return "Switch to \(variant) failed — still using \(loadedVariant)."
            }
            return "Audio transcription is off."
        case .ready, .unknown:
            return "Audio transcription is on."
        }
    }

    /// The daemon's own message. It is the only thing that distinguishes a
    /// failed download from a corrupt file, so it is surfaced rather than
    /// summarised away.
    var detail: String? {
        guard state == .error, let errorText, !errorText.isEmpty else { return nil }
        return errorText
    }

    var action: Action? {
        switch state {
        case .missing:
            return Action(title: "Download", variant: variant)
        case .error:
            return Action(title: "Retry", variant: variant)
        case .downloading, .verifying, .loading, .ready, .unknown:
            return nil
        }
    }

    /// Advertised size for the variant being offered, read from the daemon's
    /// own `models` list so the copy and the button act on one source.
    var sizeLabel: String {
        guard let entry = models.first(where: { $0.variant == variant }) else {
            return "size unknown"
        }
        // clamping, not trapping: a wrong label beats a crash if a future
        // field ever carries a sentinel above Int64.max.
        return ByteCountFormatter.string(
            fromByteCount: Int64(clamping: entry.sizeBytes), countStyle: .file)
    }
}
