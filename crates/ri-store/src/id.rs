use uuid::Uuid;

// Generate a globally unique ID.
// Uses UUID v4 (128-bit random), formatted as a short hex string without dashes.
pub fn gen_id() -> String {
    Uuid::new_v4().simple().to_string()
}
