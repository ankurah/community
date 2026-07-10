use ankurah::{property::Json, Model, Ref};
use serde::{Deserialize, Serialize};

#[derive(Model, Debug, Serialize, Deserialize)]
pub struct User {
    pub display_name: String,
    /// Stable subject identifier from the OIDC provider (idp.to `sub`). `None`
    /// for legacy anonymous users; `Some` once a user signs in. Users are keyed
    /// on this so repeat sign-ins resolve to the same `User` entity.
    pub oidc_sub: Option<String>,
}

// Room model - chat rooms
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct Room {
    pub name: String,
    /// Creator of the room. `None` for the seeded default rooms, which makes
    /// them moderator-managed only (see the room write scope in policy.json:
    /// `created_by = $jwt.sub` unless the caller holds `moderate`).
    pub created_by: Option<Ref<User>>,
}

/// Server-maintained display cache of a user's roles — one row per user.
///
/// Roles are NOT managed here. The source of truth is the idp.to `roles` claim
/// carried in the verified id_token and baked into the ankurah session token at
/// mint time (see `server::resolve_roles`); this row only mirrors that result so
/// the UI can render role badges without decoding the caller's JWT.
///
/// It is written exclusively by the server's privileged (Root) context. The
/// `userroles` policy entry requires a `system` write privilege that no role
/// holds, so remote JWT-bearing clients can never write it — otherwise a client
/// could spoof its own role badges.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct UserRoles {
    #[active_type(LWW)]
    pub user: Ref<User>,
    /// JSON array of stable lowercase role keys (e.g. `["member","moderator"]`),
    /// mirroring the roles minted into the user's most recent session token.
    pub roles: Json,
}

/// A moderation ban. Enforced at token mint (banned users are refused a new
/// session) and — once the guarded policy agent lands — live at the durable
/// node, so existing connections lose access as soon as the ban syncs.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct Ban {
    #[active_type(LWW)]
    pub user: Ref<User>,
    pub reason: String,
    pub created_at: i64,
    /// Bans are lifted by flipping this off (audit trail stays).
    #[active_type(LWW)]
    pub active: bool,
}

#[derive(Model, Debug, Serialize, Deserialize)]
pub struct Message {
    #[active_type(LWW)]
    pub user: Ref<User>,
    #[active_type(LWW)]
    pub room: Ref<Room>,
    pub text: String,
    pub timestamp: i64,
    #[active_type(LWW)]
    pub deleted: bool,
}
