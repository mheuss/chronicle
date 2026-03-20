import SwiftUI

@main
struct ChronicleApp: App {
    var body: some Scene {
        MenuBarExtra("Chronicle", systemImage: "record.circle") {
            VStack {
                Text("Chronicle")
                    .font(.headline)
                Divider()
                Button("Quit") {
                    NSApplication.shared.terminate(nil)
                }
                .keyboardShortcut("q")
            }
            .padding()
        }
    }
}
