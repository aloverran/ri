// OAuth login flows for LLM providers.
//
// Each provider implements OAuthProvider: login(), refresh(), get_api_key().
// Credentials are stored externally (auth.json) by the CLI.

use serde::{Deserialize, Serialize};

pub mod anthropic_oauth;
pub mod google_oauth;
mod pkce;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    pub refresh: String,
    pub access: String,
    pub expires: u64, // ms since epoch
    /// Google Cloud project ID (only used by Google providers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// User email (optional, for display purposes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

pub struct OAuthLoginResult {
    pub url: String,
    pub instructions: Option<String>,
}

#[async_trait::async_trait]
pub trait OAuthProvider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;

    /// Start login flow. Returns a URL for the user to visit.
    fn begin_login(&self) -> eyre::Result<(OAuthLoginResult, LoginState)>;

    /// Complete login with the code the user pasted back.
    async fn complete_login(&self, code: &str, state: &LoginState) -> eyre::Result<OAuthCredentials>;

    /// Refresh an expired token.
    async fn refresh_token(&self, credentials: &OAuthCredentials) -> eyre::Result<OAuthCredentials>;

    /// Extract the API key from credentials.
    fn get_api_key(&self, credentials: &OAuthCredentials) -> String;
}

/// Opaque state carried between begin_login and complete_login.
pub struct LoginState {
    pub verifier: String,
    pub state: String,
}
