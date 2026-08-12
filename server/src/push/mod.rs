//! Mobile push notifications: the pipeline that turns an inbox row into an
//! alert on a phone.
//!
//! FOR: the in-app inbox only reaches someone with the app open. A mention or a
//! direct message that arrives while the phone is in a pocket is the case push
//! exists for, and this generation answers it plainly — one visible alert per
//! notification-worthy event, addressed to every device the member has
//! registered.
//!
//! WHAT IS DELIBERATELY NOT HERE. There is no deduplication protocol and no
//! idle-or-present check. A member reading along on a laptop is sent the alert
//! anyway. Both were considered and deferred out of this generation on purpose,
//! so nothing in this subsystem should grow a half-version of either.
//!
//! THE PIECES, and the order events move through them:
//!
//! - [`registry`] is the door: a member's app posts its device token to
//!   `POST /push/register`, authenticated with the session token
//!   `/auth/session` minted.
//! - [`store`] keeps those tokens in a plain server-side table — NOT an ankurah
//!   collection, because a device token is a credential and a collection would
//!   mean a policy entry to get right forever.
//! - [`apns`] is the far end: a provider token, an HTTP/2 request to Apple, and
//!   what Apple's answer means for the device it was addressed to.
//! - `workers::push` is what joins them. It watches the `notification` rows the
//!   fan-out writes and, for each new one, sends one alert to every device the
//!   recipient has registered. It lives with the other reactive workers because
//!   that is what it is — a standing LiveQuery feeding a supervised consumer.

pub mod apns;
pub mod registry;
pub mod store;

pub use registry::PushRegistry;
pub use store::DeviceTokens;
