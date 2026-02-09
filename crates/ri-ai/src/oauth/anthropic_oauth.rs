// Anthropic OAuth provider.
//
// Implements the PKCE Authorization Code flow against claude.ai / console.anthropic.com.
// Constants and flow match pi's implementation.

use super::pkce;
use super::{LoginState, OAuthCredentials, OAuthLoginResult, OAuthProvider};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference";
const CREATE_API_KEY_URL: &str = "https://api.anthropic.com/api/oauth/claude_cli/create_api_key";

pub struct AnthropicOAuth {
    client: reqwest::Client,
}

impl AnthropicOAuth {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl OAuthProvider for AnthropicOAuth {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn name(&self) -> &str {
        "Anthropic"
    }

    fn begin_login(&self) -> eyre::Result<(OAuthLoginResult, LoginState)> {
        let verifier = pkce::generate_verifier();
        let challenge = pkce::challenge(&verifier);
        let state = pkce::generate_verifier(); // random string for CSRF protection

        let mut url = reqwest::Url::parse(AUTHORIZE_URL)?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", SCOPES)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);

        let result = OAuthLoginResult {
            url: url.to_string(),
            instructions: Some(
                "Visit the URL above, authorize the app, then paste the code from the callback page."
                    .to_string(),
            ),
        };

        Ok((result, LoginState { verifier, state }))
    }

    async fn complete_login(
        &self,
        code: &str,
        state: &LoginState,
    ) -> eyre::Result<OAuthCredentials> {
        // The callback page shows "code#state" -- split and verify state matches.
        let (actual_code, returned_state) = match code.split_once('#') {
            Some((c, s)) => (c, Some(s)),
            None => (code, None),
        };

        if let Some(rs) = returned_state {
            if rs != state.state {
                return Err(eyre::eyre!("OAuth state mismatch (possible CSRF)"));
            }
        }

        let response = self
            .client
            .post(TOKEN_URL)
            .json(&serde_json::json!({
                "grant_type": "authorization_code",
                "client_id": CLIENT_ID,
                "code": actual_code,
                "state": returned_state.unwrap_or(&state.state),
                "redirect_uri": REDIRECT_URI,
                "code_verifier": state.verifier,
            }))
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Token exchange failed ({}): {}", status, body));
        }

        let body: serde_json::Value = response.json().await?;
        let mut creds = parse_token_response(&body)?;

        // Exchange the OAuth token for a real API key.
        // The Messages API does not accept OAuth tokens directly.
        let api_key = self.create_api_key(&creds.access).await?;
        creds.access = api_key;

        Ok(creds)
    }

    async fn refresh_token(
        &self,
        credentials: &OAuthCredentials,
    ) -> eyre::Result<OAuthCredentials> {
        let response = self
            .client
            .post(TOKEN_URL)
            .json(&serde_json::json!({
                "grant_type": "refresh_token",
                "client_id": CLIENT_ID,
                "refresh_token": credentials.refresh,
            }))
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!("Token refresh failed ({}): {}", status, body));
        }

        let body: serde_json::Value = response.json().await?;

        // Some providers omit refresh_token on refresh -- keep the old one.
        let refresh = body["refresh_token"]
            .as_str()
            .unwrap_or(&credentials.refresh)
            .to_string();

        let access = body["access_token"]
            .as_str()
            .ok_or_else(|| eyre::eyre!("Missing access_token"))?
            .to_string();

        let expires_in = body["expires_in"].as_u64().unwrap_or(3600);
        let now_ms = epoch_ms();

        // Exchange the refreshed OAuth token for a new API key.
        // If this fails, still return the new refresh token so it doesn't get lost
        // (the server may have rotated it).
        let api_key = match self.create_api_key(&access).await {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!("API key exchange failed after refresh: {e}");
                access
            }
        };

        Ok(OAuthCredentials {
            access: api_key,
            refresh,
            expires: now_ms + (expires_in * 1000),
        })
    }

    fn get_api_key(&self, credentials: &OAuthCredentials) -> String {
        credentials.access.clone()
    }
}

impl AnthropicOAuth {
    /// Exchange an OAuth access token for a real API key via the CLI endpoint.
    async fn create_api_key(&self, oauth_token: &str) -> eyre::Result<String> {
        let response = self
            .client
            .post(CREATE_API_KEY_URL)
            .header("authorization", format!("Bearer {oauth_token}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(eyre::eyre!("API key creation failed ({}): {}", status, body));
        }

        let body: serde_json::Value = response.json().await?;
        body["raw_key"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| eyre::eyre!("Missing raw_key in create_api_key response"))
    }
}

fn parse_token_response(body: &serde_json::Value) -> eyre::Result<OAuthCredentials> {
    let access = body["access_token"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token in response"))?
        .to_string();

    let refresh = body["refresh_token"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("Missing refresh_token in response"))?
        .to_string();

    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);
    let now_ms = epoch_ms();

    Ok(OAuthCredentials {
        access,
        refresh,
        expires: now_ms + (expires_in * 1000),
    })
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
