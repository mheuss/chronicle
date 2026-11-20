import Foundation
import Testing
@testable import ChronicleUI

@Suite("Data root")
struct DataRootTests {
    private static let container = URL(
        fileURLWithPath: "/Users/someone/Library/Group Containers/7TJEE2YF4G.chronicle"
    )

    private static func containerURL(_ id: String) -> URL? {
        id == "7TJEE2YF4G.chronicle" ? container : nil
    }

    @Test func absoluteOverrideWins() {
        let result = DataRoot.resolve(
            environment: ["CHRONICLE_DATA_DIR": "/tmp/chronicle-dev"],
            containerURL: Self.containerURL
        )
        #expect(result == .success(URL(fileURLWithPath: "/tmp/chronicle-dev")))
    }

    @Test func relativeOverrideIsAnError() {
        let result = DataRoot.resolve(
            environment: ["CHRONICLE_DATA_DIR": "data"],
            containerURL: Self.containerURL
        )
        #expect(result == .failure(.relativeOverride("data")))
    }

    @Test func emptyOverrideUsesTheContainer() {
        let result = DataRoot.resolve(
            environment: ["CHRONICLE_DATA_DIR": ""],
            containerURL: Self.containerURL
        )
        #expect(result == .success(Self.container))
    }

    @Test func unsetOverrideUsesTheContainer() {
        let result = DataRoot.resolve(environment: [:], containerURL: Self.containerURL)
        #expect(result == .success(Self.container))
    }

    @Test func nilContainerURLIsAnError() {
        let result = DataRoot.resolve(environment: [:], containerURL: { _ in nil })
        #expect(result == .failure(.containerUnavailable))
    }

    @Test func relativeOverrideMessageNamesTheVariableAndValue() {
        #expect(
            DataRootError.relativeOverride("data").errorDescription
                == "CHRONICLE_DATA_DIR must be an absolute path, got \"data\""
        )
    }
}
