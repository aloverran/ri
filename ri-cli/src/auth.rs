use ri::auth::OAuthCredentials;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AuthStore {
    pub providers: HashMap<String, OAuthCredentials>,
}

impl AuthStore {
    pub fn path() -> PathBuf {
        dirs::home_dir().expect("No home directory").join(".ri").join("auth.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        if !path.exists() { return Self::default(); }
        let data = std::fs::read_to_string(&path).unwrap_or_default();
        serde_json::from_str(&data).unwrap_or_default()
    }

    pub fn save(&self) -> eyre::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn get(&self, provider: &str) -> Option<&OAuthCredentials> {
        self.providers.get(provider)
    }

    pub fn set(&mut self, provider: &str, creds: OAuthCredentials) {
        self.providers.insert(provider.to_string(), creds);
    }
}
