import SwiftUI

@main
struct ChronicleApp: App {
    @State private var connection = DaemonConnection()

    var body: some Scene {
        MenuBarExtra("Chronicle", systemImage: "record.circle") {
            VStack {
                Text("Chronicle")
                    .font(.headline)
                HStack {
                    Circle()
                        .fill(
                            connection.state == .connected ? .green :
                            connection.state == .connecting ? .yellow : .red
                        )
                        .frame(width: 8, height: 8)
                    Text(
                        connection.state == .connected ? "Daemon connected" :
                        connection.state == .connecting ? "Connecting…" : "Daemon disconnected"
                    )
                    .font(.caption)
                }
                Divider()
                Button("Quit") {
                    connection.disconnect()
                    NSApplication.shared.terminate(nil)
                }
                .keyboardShortcut("q")
            }
            .padding()
            .task {
                connection.connect()
            }
        }
    }
}
