import Capacitor

/// The bridge view controller the scene puts on screen, and the one place the
/// app's own native plugins are handed to the bridge.
///
/// FOR: a plugin that lives in the app rather than in an npm package has
/// nothing to announce it. Capacitor registers a package's plugins from the
/// list its CLI generates at sync time; an app-local one is registered by hand,
/// here, in the hook the bridge calls once it exists and before the web view
/// loads the page — so the page's glue finds `AuthSession` on its first call.
class AppViewController: CAPBridgeViewController {
    override func capacitorDidLoad() {
        bridge?.registerPluginInstance(AuthSessionPlugin())
    }
}
