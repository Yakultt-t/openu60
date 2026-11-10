import SwiftUI

@main
struct U60ToolkitApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
                .frame(minWidth: 760, minHeight: 680)
        }
        .windowResizability(.contentMinSize)
    }
}
