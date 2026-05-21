import Foundation

/// Convert a FTS5 snippet with `<b>...</b>` markup into an `AttributedString`
/// where the bracketed regions are rendered bold.
///
/// FTS5 controls the snippet content, so:
/// - No nested tags are possible.
/// - Tags appear in pairs.
///
/// If the input has unbalanced tags (defensive — shouldn't happen in
/// practice), the parser falls back to a plain `AttributedString` with the
/// raw text. This keeps the UI rendering a sensible string in pathological
/// cases instead of crashing.
func snippetAttributedString(_ snippet: String) -> AttributedString {
    var result = AttributedString()
    var remaining = snippet[...]
    let openTag = "<b>"
    let closeTag = "</b>"

    while !remaining.isEmpty {
        guard let openRange = remaining.range(of: openTag) else {
            // No more bold runs — append the rest as plain.
            result.append(AttributedString(String(remaining)))
            return result
        }
        // Append the plain prefix before the open tag.
        result.append(AttributedString(String(remaining[remaining.startIndex..<openRange.lowerBound])))
        // Find the matching close tag.
        let afterOpen = remaining[openRange.upperBound...]
        guard let closeRange = afterOpen.range(of: closeTag) else {
            // Unbalanced — fall back to plain rendering of the whole input.
            return AttributedString(snippet)
        }
        var boldRun = AttributedString(String(afterOpen[afterOpen.startIndex..<closeRange.lowerBound]))
        boldRun.inlinePresentationIntent = .stronglyEmphasized
        result.append(boldRun)
        remaining = afterOpen[closeRange.upperBound...]
    }
    return result
}
