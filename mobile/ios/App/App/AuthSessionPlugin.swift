import AuthenticationServices
import Capacitor
import SafariServices
import UIKit

/// The app's front door to idp.to, and the app's way of putting an idp.to page
/// in front of the member without becoming the browser for it.
///
/// FOR: the sign-in ceremony has to run somewhere that already knows the
/// member — Safari, where the passkeys they enrolled on this device live and
/// where an idp.to session may already be standing. This app's web view knows
/// none of that: it is served from the app bundle, so it shares no cookie jar
/// and no credential store with anything. `ASWebAuthenticationSession` is the
/// system's answer — a sheet the app can open on a URL and never read the
/// inside of, which hands back only the redirect the browser was sent to.
///
/// `start` opens that sheet on an authorization URL and resolves with the whole
/// callback URL iOS caught on the app's own scheme; the page pulls `code` and
/// `state` off it and runs the exchange itself. `openExternal` puts an ordinary
/// page (account settings, the end-session URL) in the same browser, so a
/// sign-out there ends the session the sheet established.
@objc(AuthSessionPlugin)
public class AuthSessionPlugin: CAPPlugin, CAPBridgedPlugin {
    public let identifier = "AuthSessionPlugin"
    public let jsName = "AuthSession"
    public let pluginMethods: [CAPPluginMethod] = [
        CAPPluginMethod(name: "start", returnType: CAPPluginReturnPromise),
        CAPPluginMethod(name: "openExternal", returnType: CAPPluginReturnPromise)
    ]

    /// A dismissed sheet is the member changing their mind, and the page says
    /// so rather than reporting a failure. Every other ending is one.
    ///
    /// SHARED LITERALS: `shell.rs` in the leptos client matches on these exact
    /// words to tell the two apart, so a rename here is a rename there.
    static let cancelledCode = "cancelled"
    static let failedCode = "failed"

    /// The sheet in flight. `ASWebAuthenticationSession` lives only as long as
    /// something holds it — released early, the sheet closes and the call it
    /// was going to answer never settles.
    private var session: ASWebAuthenticationSession?

    @objc func start(_ call: CAPPluginCall) {
        guard let address = call.getString("url"), let url = URL(string: address) else {
            call.reject("the sign-in sheet was asked to open with no authorization URL", Self.failedCode)
            return
        }
        guard let callbackScheme = call.getString("callbackScheme"), !callbackScheme.isEmpty else {
            call.reject("the sign-in sheet was asked to open with no callback scheme", Self.failedCode)
            return
        }

        // Plugin calls arrive off the main thread; the sheet and the window it
        // hangs from are UIKit's.
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            let session = ASWebAuthenticationSession(url: url, callbackURLScheme: callbackScheme) { callbackURL, error in
                self.session = nil
                if let callbackURL {
                    call.resolve(["url": callbackURL.absoluteString])
                } else if let error = error as? ASWebAuthenticationSessionError, error.code == .canceledLogin {
                    call.reject("sign-in was cancelled", Self.cancelledCode)
                } else {
                    call.reject(error?.localizedDescription ?? "the sign-in sheet closed without a callback", Self.failedCode)
                }
            }
            session.presentationContextProvider = self
            // FALSE, deliberately. An ephemeral sheet gets a blank browser
            // profile: no passkeys, no standing idp.to session, a fresh
            // enrollment every sign-in. Sharing Safari's surface is the whole
            // reason for taking the ceremony out of the web view.
            session.prefersEphemeralWebBrowserSession = false
            self.session = session
            if !session.start() {
                self.session = nil
                call.reject("the sign-in sheet would not open", Self.failedCode)
            }
        }
    }

    @objc func openExternal(_ call: CAPPluginCall) {
        guard let address = call.getString("url"),
              let url = URL(string: address),
              let scheme = url.scheme?.lowercased(),
              scheme == "https" || scheme == "http" else {
            call.reject("openExternal was asked to open something that is not an http(s) page", Self.failedCode)
            return
        }

        DispatchQueue.main.async { [weak self] in
            guard let host = self?.bridge?.viewController else {
                call.reject("no view controller to present the browser from", Self.failedCode)
                return
            }
            // Safari's own view controller rather than the web view: it reads
            // the same cookie jar the sign-in sheet used, which is what lets a
            // sign-out here end the session that sheet established.
            host.present(SFSafariViewController(url: url), animated: true) {
                call.resolve()
            }
        }
    }
}

extension AuthSessionPlugin: ASWebAuthenticationPresentationContextProviding {
    public func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        // The window the bridge's own view controller is in. A detached
        // window is returned only if there is none — the sheet then fails to
        // present and `start`'s completion reports it, which is a better end
        // than a crash on a force-unwrap.
        bridge?.viewController?.view.window ?? ASPresentationAnchor()
    }
}
