// The mobile shell's page glue: the two native calls the wasm client makes,
// and nothing else.
//
// FOR: wasm cannot reach a Capacitor plugin on its own — the bridge is a
// JavaScript object the native runtime injects into the page. This file is the
// forwarding layer, and deliberately holds no decisions: every branch about
// when to sign in, what to do with a callback URL, and what to show a member
// who cancels lives in `src/shell.rs` and `src/auth.rs`.
//
// Defined ONLY inside the app. `window.Capacitor` exists only where the native
// runtime injected it, so in a browser this file leaves no global behind and
// the client's shell detection — which is the presence of that global —
// answers no.
(function () {
  var capacitor = window.Capacitor;
  if (!capacitor || typeof capacitor.isNativePlatform !== 'function' || !capacitor.isNativePlatform()) {
    return;
  }

  window.__ankurahShell = {
    // Resolves with the callback URL the sign-in sheet caught, or rejects with
    // an error carrying `code`: "cancelled" when the member dismissed the
    // sheet, "failed" otherwise (AuthSessionPlugin.swift writes both words).
    startAuthSession: function (url, callbackScheme) {
      return capacitor
        .nativePromise('AuthSession', 'start', { url: url, callbackScheme: callbackScheme })
        .then(function (result) {
          return result.url;
        });
    },
    // Puts a page in the system browser, on top of the app.
    openExternal: function (url) {
      return capacitor.nativePromise('AuthSession', 'openExternal', { url: url });
    }
  };
})();
