use super::{pkce, epoch_ms, OAuthCredentials};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference";

pub struct LoginFlow {
    pub url: String,
    pub verifier: String,
    pub state: String,
}

pub fn begin_login() -> eyre::Result<LoginFlow> {
    let verifier = pkce::generate_verifier();
    let challenge = pkce::challenge(&verifier);
    let state = pkce::generate_verifier();

    let mut url = reqwest::Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state);

    Ok(LoginFlow { url: url.to_string(), verifier, state })
}

pub async fn complete_login(code: &str, flow: &LoginFlow) -> eyre::Result<OAuthCredentials> {
    let (actual_code, returned_state) = match code.split_once('#') {
        Some((c, s)) => (c, Some(s)),
        None => (code, None),
    };

    if let Some(rs) = returned_state {
        if rs != flow.state {
            return Err(eyre::eyre!("OAuth state mismatch (possible CSRF)"));
        }
    }

    let client = reqwest::Client::new();
    let response = client
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": actual_code,
            "state": returned_state.unwrap_or(&flow.state),
            "redirect_uri": REDIRECT_URI,
            "code_verifier": flow.verifier,
        }))
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(eyre::eyre!("Token exchange failed ({}): {}", status, body));
    }

    let body: serde_json::Value = response.json().await?;
    parse_token_response(&body)
}

pub async fn refresh_token(credentials: &OAuthCredentials) -> eyre::Result<OAuthCredentials> {
    let client = reqwest::Client::new();
    let response = client
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
    let refresh = body["refresh_token"].as_str()
        .unwrap_or(&credentials.refresh)
        .to_string();

    let mut access = body["access_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token"))?
        .to_string();

    if !access.starts_with("sk-ant-oat") {
        access = format!("sk-ant-oat-{}", access);
    }

    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);
    let expires = epoch_ms() + (expires_in * 1000).saturating_sub(300_000);

    Ok(OAuthCredentials { access, refresh, expires, project_id: None, email: None })
}

fn parse_token_response(body: &serde_json::Value) -> eyre::Result<OAuthCredentials> {
    let mut access = body["access_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token"))?
        .to_string();

    if !access.starts_with("sk-ant-oat") {
        access = format!("sk-ant-oat-{}", access);
    }

    let refresh = body["refresh_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing refresh_token"))?
        .to_string();

    let expires_in = body["expires_in"].as_u64().unwrap_or(3600);
    let expires = epoch_ms() + (expires_in * 1000).saturating_sub(300_000);

    Ok(OAuthCredentials { access, refresh, expires, project_id: None, email: None })
}
