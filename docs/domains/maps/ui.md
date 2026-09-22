# Domain: UI

**Status:** Decided 2026-09-18, revised 2026-09-21
**Owns:** Everything the person sees and interacts with — the menu bar app's scenes, its views, the state that decides when an alert fires, the copy that describes a wire value, and the bundle keys that make the process a menu bar agent.
**Code:** `chronicle-ui/Sources/ChronicleUI/`, less `DaemonConnection.swift` and `ConnectionSession.swift`

## Boundary

- inside: declaring the app's scenes and constructing the objects they share — `chronicle-ui/Sources/ChronicleUI/ChronicleApp.swift` — `ChronicleApp`
- inside: what the menu bar item shows about capture health — `chronicle-ui/Sources/ChronicleUI/MenuBarIcon.swift` — `MenuBarIcon`
- inside: the search surface, its result list, and the banners above it — `chronicle-ui/Sources/ChronicleUI/SearchPopoverView.swift` — `SearchPopoverView`
- inside: rendering one search hit, including bold runs parsed out of an FTS5 snippet — `chronicle-ui/Sources/ChronicleUI/SnippetAttributedString.swift` — `snippetAttributedString`
- inside: deciding whether a one-shot alert has already fired, and on which snapshot — `chronicle-ui/Sources/ChronicleUI/TranscriptionAlertState.swift` — `TranscriptionAlertState`
- inside: what a banner or a settings row *says* about a daemon state, held apart from the view so the branches are testable — `chronicle-ui/Sources/ChronicleUI/TranscriptionBannerCopy.swift` — `TranscriptionBannerCopy`
- inside: the bundle keys that make this process a menu bar agent with no Dock icon — `chronicle-ui/Sources/ChronicleUI/Info.plist` — `LSUIElement`
- outside: the socket, the reconnect loop, and the request methods these views call — `chronicle-ui/Sources/ChronicleUI/DaemonConnection.swift` — `DaemonConnection`
- outside: declaring the wire types these views read — `chronicle-ui/Sources/ChronicleUI/DaemonConnection.swift` — `StatusData`
- outside: producing the data a view renders, and deciding what a status can report — `chronicle-daemon/crates/storage/src/models.rs` — `StorageStatus`

**Placement test:** Does the file decide what the person sees, what it says, or when it appears? Getting the value it displays belongs to whoever produces that value; carrying it across the socket belongs to IPC.
**Document comparison:** differs — no document in the corpus describes this domain. `docs/guides/` holds four files and `docs/use-cases/` eight, six of them domain entries, none about the UI, and `docs/use-cases/INDEX.md` names no UI row. Thirteen of the twenty corpus files naming `chronicle-ui` or `ChronicleUI` are under `.claude/audit/`.

## Owned Files

- chronicle-ui/Sources/ChronicleUI/ChronicleApp.swift
- chronicle-ui/Sources/ChronicleUI/Info.plist
- chronicle-ui/Sources/ChronicleUI/MenuBarIcon.swift
- chronicle-ui/Sources/ChronicleUI/ResultRow.swift
- chronicle-ui/Sources/ChronicleUI/RetentionCopy.swift
- chronicle-ui/Sources/ChronicleUI/ScreenshotDetailView.swift
- chronicle-ui/Sources/ChronicleUI/SearchPopoverView.swift
- chronicle-ui/Sources/ChronicleUI/SettingsView.swift
- chronicle-ui/Sources/ChronicleUI/SnippetAttributedString.swift
- chronicle-ui/Sources/ChronicleUI/StartupAlertState.swift
- chronicle-ui/Sources/ChronicleUI/TranscriptionAlertState.swift
- chronicle-ui/Sources/ChronicleUI/TranscriptionBannerCopy.swift

## Depends On

- IPC — `chronicle-ui/Sources/ChronicleUI/ChronicleApp.swift` — `DaemonConnection`

## Depended On By

- None

## Seams

- SEAM-daemon-connection-api — IPC — this domain holds one `DaemonConnection`, calls its request methods and reads its published state, and never touches the socket; IPC decides the method surface and the error vocabulary, this domain decides what it renders — owner: IPC — `chronicle-ui/Sources/ChronicleUI/ChronicleApp.swift` — `DaemonConnection`

## How The Existing Documents Saw This

| Document | Said | Verdict |
|---|---|---|
| `docs/project-description.md:25-26, 166-178, 207` | A "thin menu bar app" whose icon click opens a search popover, built in SwiftUI | accurate in shape — `MenuBarExtra` with `SearchPopoverView` as content is `ChronicleApp.swift:9-17`. "Thin" understates it: this domain is 12 files and 1,631 lines |
| `docs/guides/chronicle-daemon.md:261-262` | The UI "does not depend on the daemon as a library — only on the IPC protocol" | accurate — this domain's only cross-domain dependency is `DaemonConnection`, and none of its eleven Swift files matches `chronicle_|chronicle-daemon|MicrophoneStatus|StorageStatus|CompletedSegment`, a pattern that fires twice in `DaemonConnection.swift` |
| `.claude/audit/architecture-design.md:60` | "the Swift app only connects, polls status, and renders a connection indicator" | stale — true when written 2026-04-06, falsified by CHR-121 (Done 2026-05-21). `SearchPopoverView.swift` is 429 lines and `SettingsView.swift` 458 |
| `.claude/audit/documentation-drift.md:57-59` | "the UI only connects to the daemon, polls `status`, and offers a Quit action" | stale — same date and same cause as the row above |
| `.claude/audit/frontend-ui.md:13` | "The menu bar scene only renders a title, a health badge, and Quit", against `docs/project-description.md:167-178` | stale as a measurement, and the disagreement it names has since inverted — the code grew a search popover and the description is the older artifact. Recorded on CHR-79, which already proposes refreshing that file |
| `.claude/audit/frontend-ui.md:17, 27, 33` | Three findings, all located at `DaemonConnection.swift` | accurate as findings, and all three are IPC's file rather than this domain's. Open as CHR-87 |
| `docs/use-cases/ipc-compat.md:76-92` | The one-shot-latch pattern, located at `TranscriptionAlertState.swift:evaluate(status:)` | accurate — and that file is this domain's, which is why the pattern reads as a UI rule about when an alert fires rather than a wire rule |
| `docs/use-cases/ipc-compat.md:94-112` | The wire-copy pattern, located at `TranscriptionBannerCopy.swift` | accurate — `RetentionCopy.swift:9-10` names it as the model it follows, and both files are this domain's |
| `docs/use-cases/INDEX.md` | Names six use-case domains, none of them the UI | accurate as an absence. Control: a case-insensitive search for `ui\|menu\|popover\|view` matches exactly one line, `:12` — row `ipc-compat`, on "Daemon/UI version skew" and the `chronicle-ui/Sources/ChronicleUI/` path. So the search could have matched a UI row and did not |

Two facts about the walk rather than about the code, recorded because nothing
else holds them. Row 10's Roots named `SearchPopoverView.swift`,
`ResultRow.swift` and `SnippetAttributedString.swift`; that sitting merged into
`storage` and claimed none of the three, so all three arrived here. And this
row's clause subtracts three candidates by name but only two files, because
`search` and `ipc-compat` both merged without claiming anything in this
directory.

`Info.plist` is owned rather than excluded. `chronicle-ui/Package.swift` both
excludes it from compilation and links it into `__TEXT` with `-sectcreate`, so it
ships inside the built binary and meets §3.2's inventory rule. Seven of its ten
keys are `CFBundle*` identity, `LSMinimumSystemVersion` a deployment floor, and
`NSHighResolutionCapable` a rendering capability. `LSUIElement` is the one that
decides what kind of app this is — a menu bar agent with no Dock icon. CHR-75
will consume this file when the bundle pipeline is built, which changes who edits
it, not who owns it today.

## Offshoots Filed

- none found
