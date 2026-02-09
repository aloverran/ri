// Google OAuth for Cloud Code Assist (Gemini CLI + Antigravity).
//
// Uses PKCE authorization code flow with a local HTTP callback server.
// After obtaining tokens, discovers/provisions a Cloud Code Assist project.
//
// Two variants:
//   GeminiCli:    standard Gemini models, port 8085
//   Antigravity:  Gemini 3 + Claude + GPT-OSS, port 51121, extra scopes

use super::pkce;
use super::OAuthCredentials;
use crate::gemini::GeminiVariant;

// -- Credentials (base64-decoded from pi's source, same Google OAuth apps) --

fn decode_b64(s: &str) -> String {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s)
        .unwrap_or_default();
    String::from_utf8(bytes).unwrap_or_default()
}

fn gemini_cli_client_id() -> String {
    decode_b64("NjgxMjU1ODA5Mzk1LW9vOGZ0Mm9wcmRybnA5ZTNhcWY2YXYzaG1kaWIxMzVqLmFwcHMuZ29vZ2xldXNlcmNvbnRlbnQuY29t")
}

fn gemini_cli_client_secret() -> String {
    decode_b64("R09DU1BYLTR1SGdNUG0tMW83U2stZ2VWNkN1NWNsWEZzeGw=")
}

fn antigravity_client_id() -> String {
    decode_b64("MTA3MTAwNjA2MDU5MS10bWhzc2luMmgyMWxjcmUyMzV2dG9sb2poNGc0MDNlcC5hcHBzLmdvb2dsZXVzZXJjb250ZW50LmNvbQ==")
}

fn antigravity_client_secret() -> String {
    decode_b64("R09DU1BYLUs1OEZXUjQ4NkxkTEoxbUxCOHNYQzR6NnFEQWY=")
}

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

const GEMINI_CLI_REDIRECT: &str = "http://localhost:8085/oauth2callback";
const ANTIGRAVITY_REDIRECT: &str = "http://localhost:51121/oauth-callback";

const GEMINI_CLI_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

const ANTIGRAVITY_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

const CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
const ANTIGRAVITY_DAILY_ENDPOINT: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";
const DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";

struct VariantConfig {
    client_id: String,
    client_secret: String,
    redirect_uri: &'static str,
    scopes: &'static [&'static str],
    port: u16,
    callback_path: &'static str,
}

fn config_for(variant: GeminiVariant) -> VariantConfig {
    match variant {
        GeminiVariant::GeminiCli => VariantConfig {
            client_id: gemini_cli_client_id(),
            client_secret: gemini_cli_client_secret(),
            redirect_uri: GEMINI_CLI_REDIRECT,
            scopes: GEMINI_CLI_SCOPES,
            port: 8085,
            callback_path: "/oauth2callback",
        },
        GeminiVariant::Antigravity => VariantConfig {
            client_id: antigravity_client_id(),
            client_secret: antigravity_client_secret(),
            redirect_uri: ANTIGRAVITY_REDIRECT,
            scopes: ANTIGRAVITY_SCOPES,
            port: 51121,
            callback_path: "/oauth-callback",
        },
    }
}

/// Run the full Google OAuth login flow.
/// Returns credentials with access token, refresh token, project ID, and email.
pub async fn login_google(
    variant: GeminiVariant,
    on_url: impl FnOnce(&str),
) -> eyre::Result<OAuthCredentials> {
    let cfg = config_for(variant);
    let verifier = pkce::generate_verifier();
    let challenge = pkce::challenge(&verifier);
    let state = pkce::generate_verifier(); // random CSRF token

    // Build authorization URL
    let scopes = cfg.scopes.join(" ");
    let auth_url = format!(
        "{}?client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&access_type=offline&prompt=consent",
        AUTH_URL,
        urlencoded(&cfg.client_id),
        urlencoded(cfg.redirect_uri),
        urlencoded(&scopes),
        urlencoded(&challenge),
        urlencoded(&state),
    );

    // Start local callback server
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", cfg.port)).await
        .map_err(|e| eyre::eyre!("Failed to start OAuth callback server on port {}: {}", cfg.port, e))?;

    // Show URL to user
    on_url(&auth_url);

    // Wait for the callback
    let (code, returned_state) = wait_for_callback(&listener, cfg.callback_path).await?;

    // Verify state
    if returned_state != state {
        return Err(eyre::eyre!("OAuth state mismatch (possible CSRF attack)"));
    }

    // Exchange code for tokens
    let client = reqwest::Client::new();
    let token_response = client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("code", code.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", cfg.redirect_uri),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await?;

    let status = token_response.status();
    if !status.is_success() {
        let body = token_response.text().await.unwrap_or_default();
        return Err(eyre::eyre!("Token exchange failed ({}): {}", status, body));
    }

    let token_data: serde_json::Value = token_response.json().await?;

    let access_token = token_data["access_token"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token"))?
        .to_string();

    let refresh_token = token_data["refresh_token"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("No refresh token received. Try again."))?
        .to_string();

    let expires_in = token_data["expires_in"].as_u64().unwrap_or(3600);
    let now_ms = epoch_ms();
    let expires = now_ms + (expires_in * 1000).saturating_sub(300_000);

    // Get user email
    let email = get_user_email(&client, &access_token).await;

    // Discover project
    let project_id = discover_project(&client, &access_token, variant).await?;

    Ok(OAuthCredentials {
        refresh: refresh_token,
        access: access_token,
        expires,
        project_id: Some(project_id),
        email,
    })
}

/// Refresh a Google OAuth token.
pub async fn refresh_google_token(
    credentials: &OAuthCredentials,
    variant: GeminiVariant,
) -> eyre::Result<OAuthCredentials> {
    let cfg = config_for(variant);
    let client = reqwest::Client::new();

    let response = client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("refresh_token", credentials.refresh.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(eyre::eyre!("Google token refresh failed ({}): {}", status, body));
    }

    let data: serde_json::Value = response.json().await?;

    let access = data["access_token"]
        .as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token in refresh response"))?
        .to_string();

    let refresh = data["refresh_token"]
        .as_str()
        .unwrap_or(&credentials.refresh)
        .to_string();

    let expires_in = data["expires_in"].as_u64().unwrap_or(3600);
    let now_ms = epoch_ms();
    let expires = now_ms + (expires_in * 1000).saturating_sub(300_000);

    Ok(OAuthCredentials {
        refresh,
        access,
        expires,
        project_id: credentials.project_id.clone(),
        email: credentials.email.clone(),
    })
}

/// Build the JSON API key string for the Gemini provider.
pub fn build_api_key(credentials: &OAuthCredentials) -> String {
    serde_json::json!({
        "token": credentials.access,
        "projectId": credentials.project_id.as_deref().unwrap_or("")
    })
    .to_string()
}

// -- HTTP callback server --

async fn wait_for_callback(
    listener: &tokio::net::TcpListener,
    expected_path: &str,
) -> eyre::Result<(String, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(300);

    // Accept connections in a loop. Browsers may send /favicon.ico, prefetch
    // requests, or other stray requests before the actual OAuth callback.
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(eyre::eyre!("OAuth callback timed out (5 minutes)"));
        }

        let (mut stream, _addr) = tokio::time::timeout(remaining, listener.accept())
            .await
            .map_err(|_| eyre::eyre!("OAuth callback timed out (5 minutes)"))?
            .map_err(|e| eyre::eyre!("Failed to accept connection: {}", e))?;

        // Read until we have a complete first line (ending with \n).
        // Cap at 8KB to prevent abuse.
        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 1024];
        let request_str = loop {
            if buf.len() >= 8192 {
                break String::from_utf8_lossy(&buf).to_string();
            }
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                stream.read(&mut tmp),
            )
            .await
            {
                Ok(Ok(0)) => break String::from_utf8_lossy(&buf).to_string(),
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&tmp[..n]);
                    // We have enough once we see the end of the first line
                    if buf.windows(1).any(|w| w[0] == b'\n') {
                        break String::from_utf8_lossy(&buf).to_string();
                    }
                }
                Ok(Err(_)) => break String::from_utf8_lossy(&buf).to_string(),
                Err(_) => break String::from_utf8_lossy(&buf).to_string(),
            }
        };

        // Parse GET line: "GET /path?params HTTP/1.1"
        let first_line = request_str.lines().next().unwrap_or("");
        let path_and_query = first_line
            .strip_prefix("GET ")
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");

        let query_start = path_and_query.find('?').unwrap_or(path_and_query.len());
        let path = &path_and_query[..query_start];
        let query = if query_start < path_and_query.len() {
            &path_and_query[query_start + 1..]
        } else {
            ""
        };

        // If this isn't our expected callback path, respond 404 and keep listening
        if path != expected_path {
            let response = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(response.as_bytes()).await;
            continue;
        }

        // Parse query params
        let mut code = None;
        let mut returned_state = None;
        let mut error = None;

        for param in query.split('&') {
            if let Some((key, value)) = param.split_once('=') {
                let value = urldecoded(value);
                match key {
                    "code" => code = Some(value),
                    "state" => returned_state = Some(value),
                    "error" => error = Some(value),
                    _ => {}
                }
            }
        }

        if let Some(err) = error {
            let response = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n<h1>Authentication Failed</h1><p>An error occurred during login. Please try again.</p>";
            let _ = stream.write_all(response.as_bytes()).await;
            return Err(eyre::eyre!("OAuth error: {}", err));
        }

        let code = code.ok_or_else(|| eyre::eyre!("No authorization code in callback"))?;
        let returned_state = returned_state.ok_or_else(|| eyre::eyre!("No state in callback"))?;

        // Send success response
        let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<html><body><h1>Authentication Successful</h1><p>You can close this window and return to the terminal.</p></body></html>";
        let _ = stream.write_all(response.as_bytes()).await;

        return Ok((code, returned_state));
    }
}

// -- Project discovery --

async fn discover_project(
    client: &reqwest::Client,
    access_token: &str,
    variant: GeminiVariant,
) -> eyre::Result<String> {
    // Check env var first
    if let Ok(project_id) = std::env::var("GOOGLE_CLOUD_PROJECT") {
        return Ok(project_id);
    }
    if let Ok(project_id) = std::env::var("GOOGLE_CLOUD_PROJECT_ID") {
        return Ok(project_id);
    }

    let endpoints = match variant {
        GeminiVariant::Antigravity => vec![CODE_ASSIST_ENDPOINT, ANTIGRAVITY_DAILY_ENDPOINT],
        GeminiVariant::GeminiCli => vec![CODE_ASSIST_ENDPOINT],
    };

    for endpoint in &endpoints {
        let url = format!("{}/v1internal:loadCodeAssist", endpoint);
        let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "metadata": {
                    "ideType": "IDE_UNSPECIFIED",
                    "platform": "PLATFORM_UNSPECIFIED",
                    "pluginType": "GEMINI"
                }
            }))
            .send()
            .await;

        if let Ok(resp) = response {
            if resp.status().is_success() {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    // Try string format
                    if let Some(id) = data["cloudaicompanionProject"].as_str() {
                        if !id.is_empty() {
                            return Ok(id.to_string());
                        }
                    }
                    // Try object format
                    if let Some(id) = data["cloudaicompanionProject"]["id"].as_str() {
                        if !id.is_empty() {
                            return Ok(id.to_string());
                        }
                    }
                }
            }
        }
    }

    // Fallback for Antigravity
    if variant == GeminiVariant::Antigravity {
        return Ok(DEFAULT_PROJECT_ID.to_string());
    }

    // Try onboarding for Gemini CLI
    let url = format!("{}/v1internal:onboardUser", CODE_ASSIST_ENDPOINT);
    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "tierId": "free-tier",
            "metadata": {
                "ideType": "IDE_UNSPECIFIED",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI"
            }
        }))
        .send()
        .await?;

    if response.status().is_success() {
        let data: serde_json::Value = response.json().await?;
        if let Some(id) = data["response"]["cloudaicompanionProject"]["id"].as_str() {
            return Ok(id.to_string());
        }
    }

    Err(eyre::eyre!(
        "Could not discover a Google Cloud project. Set GOOGLE_CLOUD_PROJECT env var."
    ))
}

async fn get_user_email(client: &reqwest::Client, access_token: &str) -> Option<String> {
    let response = client
        .get("https://www.googleapis.com/oauth2/v1/userinfo?alt=json")
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
        .ok()?;

    if response.status().is_success() {
        let data: serde_json::Value = response.json().await.ok()?;
        data["email"].as_str().map(|s| s.to_string())
    } else {
        None
    }
}

// -- Helpers --

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn urlencoded(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

fn urldecoded(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().unwrap_or(b'0');
            let lo = chars.next().unwrap_or(b'0');
            let hex = format!("{}{}", hi as char, lo as char);
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            }
        } else if b == b'+' {
            result.push(' ');
        } else {
            result.push(b as char);
        }
    }
    result
}
