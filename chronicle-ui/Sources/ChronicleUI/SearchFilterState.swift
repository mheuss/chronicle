import Observation

/// The search popover's source filter. Owned by `ChronicleApp`, not the
/// popover view, so the selection survives the popover closing.
@Observable
final class SearchFilterState {
    var filter: SearchFilter = .all
}
