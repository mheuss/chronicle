import AppKit
import SwiftUI

/// Hosts `OnboardingView` in one reused window. The order of the three calls
/// in `present` comes from the CHR-94 activation probe (design §9.2, AD2).
@MainActor
final class OnboardingWindowPresenter: NSObject, OnboardingPresenter, NSWindowDelegate {
    private var window: NSWindow?
    private weak var controller: OnboardingWindowController?

    func present(_ controller: OnboardingWindowController) {
        self.controller = controller
        let window = self.window ?? makeWindow(controller)
        self.window = window
        window.orderFrontRegardless()
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }

    func close() {
        window?.close()
    }

    func windowWillClose(_ notification: Notification) {
        controller?.didClose()
    }

    private func makeWindow(_ controller: OnboardingWindowController) -> NSWindow {
        let window = NSWindow(contentRect: .zero, styleMask: [.titled, .closable],
                              backing: .buffered, defer: false)
        let hosting = NSHostingView(rootView: OnboardingView(controller: controller))
        window.contentView = hosting
        window.setContentSize(hosting.fittingSize)
        window.title = OnboardingCopy.windowTitle
        window.isReleasedWhenClosed = false
        window.delegate = self
        window.center()
        return window
    }
}
