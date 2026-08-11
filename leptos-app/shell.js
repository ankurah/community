// The mobile shell's page glue: the native calls the wasm client makes, and
// nothing else.
//
// FOR: wasm cannot reach a Capacitor plugin on its own — the bridge is a
// JavaScript object the native runtime injects into the page. This file is the
// forwarding layer, and deliberately holds no decisions: every branch about
// when to sign in, what to do with a callback URL, what to show a member who
// cancels, whether to ask about notifications and where a tapped alert leads
// lives in `src/shell.rs`, `src/auth.rs` and `src/push.rs`.
//
// TWO WAYS ACROSS THE BRIDGE, both `window.Capacitor`'s (@capacitor/ios's
// native-bridge.js builds them side by side in `initEvents`). `nativePromise`
// makes one call and settles once. `addListener(plugin, event, callback)`
// hands the native side a callback it keeps, and returns `{ remove }` — that
// is the only route a plugin EVENT reaches the page by, and the push plugin
// reports its device token and every tapped alert as events.
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
    },
    // What iOS says about notifications for this install: "prompt" (never
    // asked), "granted", or "denied". Asks nobody anything.
    pushPermission: function () {
      return capacitor.nativePromise('PushNotifications', 'checkPermissions', {}).then(function (result) {
        return result.receive;
      });
    },
    // Puts the system's notification prompt on screen, and answers with the
    // same three words. iOS shows it once per install; afterwards this
    // resolves with the standing answer and nothing appears.
    requestPushPermission: function () {
      return capacitor.nativePromise('PushNotifications', 'requestPermissions', {}).then(function (result) {
        return result.receive;
      });
    },
    // Ask APNs for this install's device token, resolving with it as a hex
    // string.
    //
    // The plugin's `register` starts the request and resolves immediately; the
    // token arrives later as a `registration` event, or the refusal as
    // `registrationError`. Joining the two into one promise is mechanical, not
    // a decision — when to register, what a refusal means and where the token
    // goes are all `push.rs`'s.
    //
    // NEVER SETTLES if APNs answers neither way (no network, and the request
    // is never retried by iOS). Nothing waits on this promise: the caller
    // spawns it and the next launch asks again.
    registerForPush: function () {
      return new Promise(function (resolve, reject) {
        var registered;
        var failed;
        var settle = function (finish, value) {
          if (registered) { registered.remove(); registered = null; }
          if (failed) { failed.remove(); failed = null; }
          finish(value);
        };
        registered = capacitor.addListener('PushNotifications', 'registration', function (event) {
          settle(resolve, event.value);
        });
        failed = capacitor.addListener('PushNotifications', 'registrationError', function (event) {
          settle(reject, new Error(event.error));
        });
        capacitor.nativePromise('PushNotifications', 'register', {}).catch(function (e) {
          settle(reject, e);
        });
      });
    },
    // Call `handler` with every alert the member taps, for as long as the
    // document lives.
    //
    // A TAP THAT LAUNCHED THE APP IS DELIVERED TOO. The plugin reports the tap
    // with Capacitor's `retainUntilConsumed`, which holds the event until
    // something listens for that name — so the page may attach this whenever
    // its boot reaches it, and a cold start still hears the tap that caused
    // it. There is no separate launch-notification call to make.
    onPushOpened: function (handler) {
      capacitor.addListener('PushNotifications', 'pushNotificationActionPerformed', handler);
    }
  };
})();
