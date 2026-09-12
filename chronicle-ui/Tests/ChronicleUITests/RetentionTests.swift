import Foundation
import Testing

@testable import ChronicleUI

/// Decoding `StorageStats.retention`, the tagged value HEU-625 put on the wire
/// in place of `retention_days`.
///
/// The decoder is hand-written rather than synthesized. A synthesized one reads
/// every declared field regardless of `kind`, so an unknown variant carrying a
/// wrongly typed payload would throw and take `StatusData` with it — the
/// failure NFR-3 exists to prevent. The unknown-payload case below is the one
/// that catches a regression back to synthesis.
@Suite("Retention decoding")
struct RetentionDecodingTests {
    /// Configured like production (`DaemonConnection`), so a future variant
    /// with a two-word payload key cannot pass here and fail in the app.
    private func decode(_ json: String) throws -> Retention {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(Retention.self, from: Data(json.utf8))
    }

    /// A full status payload with `retention` spliced in, for the cases whose
    /// point is that the ENCLOSING block survives.
    private func statusJSON(retention: String) -> String {
        """
        {"type":"status","ok":true,"data":{"uptime_secs":5,"version":"0.1.0",
         "storage":{"db_size_bytes":1,"total_disk_usage_bytes":2,
          "screenshot_count":3,"audio_segment_count":4,"oldest_entry_ms":null,
          "retention":\(retention)}}}
        """
    }

    @Test("days carries its value")
    func daysDecodes() throws {
        let r = try decode(#"{"kind":"days","value":30}"#)
        #expect(r == .days(30))
    }

    @Test("disabled decodes with no payload key present")
    func disabledDecodes() throws {
        // Serde emits the unit variant as a map with only the tag: `value` is
        // absent, not null. Reading it with `decode(_:forKey:)` outside the
        // days branch would throw; `decodeIfPresent` is what absorbs it.
        let r = try decode(#"{"kind":"disabled"}"#)
        #expect(r == .disabled)
    }

    @Test("invalid decodes with no payload key present")
    func invalidDecodes() throws {
        let r = try decode(#"{"kind":"invalid"}"#)
        #expect(r == .invalid)
    }

    @Test("an unrecognized kind decodes to unknown rather than throwing")
    func unknownKindDecodes() throws {
        let r = try decode(#"{"kind":"future"}"#)
        #expect(r == .unknown)
    }

    @Test("an unrecognized kind carrying a wrongly typed payload does not throw")
    func unknownKindWithWrongTypedPayload() throws {
        // A synthesized decoder reads `value` here and throws on the string.
        let r = try decode(#"{"kind":"future","value":"forever"}"#)
        #expect(r == .unknown)
    }

    @Test("a wrongly typed payload does not take the enclosing status down")
    func unknownKindDoesNotBreakTheEnclosingBlock() throws {
        // The point of NFR-3 is not that the enum survives — it is that
        // StatusData does. A synthesized decoder fails exactly here.
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let json = statusJSON(retention: #"{"kind":"future","value":"forever"}"#)
        let resp = try decoder.decode(StatusResponse.self, from: Data(json.utf8))
        #expect(resp.data.storage?.retention == .unknown)
        #expect(resp.data.storage?.screenshotCount == 3)
        #expect(resp.data.uptimeSecs == 5)
    }

    @Test("a days object with no value is malformed, not a day count")
    func daysWithNoValue() throws {
        let r = try decode(#"{"kind":"days"}"#)
        #expect(r == .unknown)
    }

    @Test("an absent retention block leaves the rest of the status decodable")
    func absentRetentionDecodes() throws {
        // Optional on the decoder, not on the wire: a daemon too old to send
        // the field degrades one row instead of the whole poll.
        let json = """
            {"db_size_bytes":1,"total_disk_usage_bytes":2,"screenshot_count":3,
             "audio_segment_count":4,"oldest_entry_ms":null}
            """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let stats = try decoder.decode(StorageStats.self, from: Data(json.utf8))
        #expect(stats.retention == nil)
        #expect(stats.screenshotCount == 3)
    }
}

/// What Settings says about each retention state.
///
/// The copy lives in a value type because a `private struct: View` is
/// unreachable from tests, and these branches are the user-visible half of
/// HEU-625 — zero used to render as "0 days" when it means keep forever.
@Suite("Retention copy")
struct RetentionCopyTests {
    @Test("a day count reads as days")
    func daysReadsAsADayCount() {
        #expect(RetentionCopy.text(for: .days(30)) == "30 days")
    }

    @Test("one day is singular")
    func oneDayIsSingular() {
        // retention_days = 1 is a valid setting, and "1 days" in the row this
        // ticket exists to make truthful would be its own small lie.
        #expect(RetentionCopy.text(for: .days(1)) == "1 day")
    }

    @Test("zero days would be a lie, and cannot arrive from classify")
    func zeroDaysIsPinned() {
        // Unreachable from today's Rust — every production path goes through
        // classify, which maps 0 to Disabled. Days's field is public though, so
        // this pins what would render if one ever arrived.
        #expect(RetentionCopy.text(for: .days(0)) == "0 days")
    }

    @Test("disabled says data is kept, never a number")
    func disabledSaysKeepForever() {
        // The exact string is the pin; a digit check below it would be dead
        // weight, and BR-2 is about invalid settings, not this one.
        #expect(RetentionCopy.text(for: .disabled) == "Keep forever")
    }

    @Test("invalid names the problem and its consequence, with no number")
    func invalidSaysNothingIsBeingDeleted() {
        let text = RetentionCopy.text(for: .invalid)
        // BR-2: an invalid setting is never reported as a number. A digit-free
        // assertion is what pins that.
        let hasDigit = text.contains(where: \.isNumber)
        #expect(!hasDigit, "BR-2: an invalid setting must not read as a number — \(text)")
        // The a11y rule for an error message: name the field and describe what
        // follows from it, not just "Invalid".
        // Identifies the field, per docs/standards/accessibility.md: the row
        // label is a sibling Text, so VoiceOver reads this string alone.
        #expect(text.contains("Retention"))
        #expect(text.contains("invalid"))
        #expect(text.contains("deleted"))
    }

    @Test("an unknown state is unavailable, not a guess")
    func unknownIsUnavailable() {
        #expect(RetentionCopy.text(for: .unknown) == "Unavailable")
    }

    @Test("a missing block is unavailable")
    func nilIsUnavailable() {
        #expect(RetentionCopy.text(for: nil) == "Unavailable")
    }
}
