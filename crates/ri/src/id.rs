use uuid::Uuid;

/// Generate a globally unique ID.
/// Uses UUID v4 (128-bit random), formatted as a short hex string without dashes.
pub fn gen_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Generate a session prefix from name + random suffix, used to create
/// human-readable but unique message IDs within a session.
pub fn gen_session_prefix(name: &str) -> String {
    let slug: String = name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(6)
        .collect();
    let rand = &Uuid::new_v4().simple().to_string()[..6];
    if slug.is_empty() {
        format!("s_{}", rand)
    } else {
        format!("{}_{}", slug, rand)
    }
}
