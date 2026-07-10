use ankurah::{Model, Ref};
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

/// A role granted to a user (e.g. "Moderator", "Admin"). Kept separate from
/// `User` because ankurah policy rules are collection-level: profile fields
/// stay self-editable while grants require the `manage_roles` privilege.
/// Roles are read at token mint time (server) — changing a grant takes effect
/// on the user's next session.
#[derive(Model, Debug, Serialize, Deserialize)]
pub struct RoleGrant {
    #[active_type(LWW)]
    pub user: Ref<User>,
    pub role: String,
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
