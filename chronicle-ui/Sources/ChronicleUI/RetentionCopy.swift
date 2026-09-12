import Foundation

/// What Settings says about the configured retention, split out from the view
/// that renders it.
///
/// A value type rather than a computed property on a `private struct: View`,
/// because a private view is unreachable from tests and these branches are the
/// user-visible half of HEU-625.
///
/// Same shape and reasoning as `TranscriptionBannerCopy`. See
/// docs/use-cases/ipc-compat.md, "Wire Copy Split From the View That Renders
/// It".
enum RetentionCopy {
    /// `nonisolated` is inert today and kept deliberately. The SIGTRAP in
    /// docs/development/swiftui-concurrency.md needs main-actor isolation via
    /// `View` conformance AND a closure in the body; this enum conforms to
    /// nothing, so the first can only become true by moving this onto a view —
    /// which is when the annotation starts earning its place.
    nonisolated static func text(for retention: Retention?) -> String {
        // `nil` is a daemon that could not read the setting, or one too old to
        // send the field. Neither says what the policy is, so this must not
        // claim one.
        guard let retention else { return "Unavailable" }

        // Exhaustive with no `default:` arm — NFR-2. A variant added to
        // `Retention` later must fail to compile here rather than silently
        // inherit whichever branch happened to catch it.
        switch retention {
        case .days(1):
            return "1 day"
        case .days(let n):
            return "\(n) days"
        case .disabled:
            return "Keep forever"
        case .invalid:
            // Names the setting and the consequence. "Retention" rather than
            // the design table's bare "Setting" because the row label is a
            // sibling Text with no combined accessibility element, so VoiceOver
            // reads this string without it. docs/standards/accessibility.md
            // requires an error message to identify the field.
            return "Retention setting is invalid — nothing is being deleted"
        case .unknown:
            // A kind this build does not know. Rendering a guess would be the
            // display lie this type exists to remove.
            return "Unavailable"
        }
    }
}
