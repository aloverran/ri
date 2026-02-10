use super::{pkce, epoch_ms, OAuthCredentials};
use crate::GeminiVariant;

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
const CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
const ANTIGRAVITY_DAILY_ENDPOINT: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";
const DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";

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
        GeminiVariant::Cli => VariantConfig {
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

pub async fn login(
    variant: GeminiVariant,
    on_url: impl FnOnce(&str),
) -> eyre::Result<OAuthCredentials> {
    let cfg = config_for(variant);
    let verifier = pkce::generate_verifier();
    let challenge = pkce::challenge(&verifier);
    let state = pkce::generate_verifier();

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

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", cfg.port)).await
        .map_err(|e| eyre::eyre!("Failed to bind OAuth callback on port {}: {}", cfg.port, e))?;

    on_url(&auth_url);

    let (code, returned_state) = wait_for_callback(&listener, cfg.callback_path).await?;
    if returned_state != state {
        return Err(eyre::eyre!("OAuth state mismatch"));
    }

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

    let data: serde_json::Value = token_response.json().await?;
    let access = data["access_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token"))?.to_string();
    let refresh = data["refresh_token"].as_str()
        .ok_or_else(|| eyre::eyre!("No refresh token"))?.to_string();
    let expires_in = data["expires_in"].as_u64().unwrap_or(3600);
    let expires = epoch_ms() + (expires_in * 1000).saturating_sub(300_000);

    let email = get_user_email(&client, &access).await;
    let project_id = discover_project(&client, &access, variant).await?;

    Ok(OAuthCredentials { refresh, access, expires, project_id: Some(project_id), email })
}

pub async fn refresh_token(
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
    let access = data["access_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token"))?.to_string();
    let refresh = data["refresh_token"].as_str()
        .unwrap_or(&credentials.refresh).to_string();
    let expires_in = data["expires_in"].as_u64().unwrap_or(3600);
    let expires = epoch_ms() + (expires_in * 1000).saturating_sub(300_000);

    Ok(OAuthCredentials {
        refresh, access, expires,
        project_id: credentials.project_id.clone(),
        email: credentials.email.clone(),
    })
}

// Build the JSON-encoded API key that the Gemini provider expects.
pub fn build_api_key(credentials: &OAuthCredentials) -> (String, String) {
    (
        credentials.access.clone(),
        credentials.project_id.clone().unwrap_or_default(),
    )
}

// -- HTTP callback server --

async fn wait_for_callback(
    listener: &tokio::net::TcpListener,
    expected_path: &str,
) -> eyre::Result<(String, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(300);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(eyre::eyre!("OAuth callback timed out"));
        }

        let (mut stream, _) = tokio::time::timeout(remaining, listener.accept())
            .await
            .map_err(|_| eyre::eyre!("OAuth callback timed out"))?
            .map_err(|e| eyre::eyre!("Accept failed: {}", e))?;

        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 1024];
        let request_str = loop {
            if buf.len() >= 8192 { break String::from_utf8_lossy(&buf).to_string(); }
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                stream.read(&mut tmp),
            ).await {
                Ok(Ok(0)) => break String::from_utf8_lossy(&buf).to_string(),
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.contains(&b'\n') { break String::from_utf8_lossy(&buf).to_string(); }
                }
                _ => break String::from_utf8_lossy(&buf).to_string(),
            }
        };

        let first_line = request_str.lines().next().unwrap_or("");
        let path_and_query = first_line.strip_prefix("GET ")
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("");

        let query_start = path_and_query.find('?').unwrap_or(path_and_query.len());
        let path = &path_and_query[..query_start];
        let query = if query_start < path_and_query.len() { &path_and_query[query_start + 1..] } else { "" };

        if path != expected_path {
            let resp = "HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes()).await;
            continue;
        }

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
            let resp = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n<h1>Failed</h1>";
            let _ = stream.write_all(resp.as_bytes()).await;
            return Err(eyre::eyre!("OAuth error: {}", err));
        }

        let code = code.ok_or_else(|| eyre::eyre!("No authorization code"))?;
        let returned_state = returned_state.ok_or_else(|| eyre::eyre!("No state"))?;

        let resp = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<h1>Success</h1><p>You can close this window.</p>";
        let _ = stream.write_all(resp.as_bytes()).await;

        return Ok((code, returned_state));
    }
}

// -- Project discovery --

async fn discover_project(
    client: &reqwest::Client,
    access_token: &str,
    variant: GeminiVariant,
) -> eyre::Result<String> {
    if let Ok(id) = std::env::var("GOOGLE_CLOUD_PROJECT") { return Ok(id); }
    if let Ok(id) = std::env::var("GOOGLE_CLOUD_PROJECT_ID") { return Ok(id); }

    let endpoints = match variant {
        GeminiVariant::Antigravity => vec![CODE_ASSIST_ENDPOINT, ANTIGRAVITY_DAILY_ENDPOINT],
        GeminiVariant::Cli => vec![CODE_ASSIST_ENDPOINT],
    };

    for endpoint in &endpoints {
        let url = format!("{}/v1internal:loadCodeAssist", endpoint);
        let resp = client.post(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "metadata": { "ideType": "IDE_UNSPECIFIED", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI" }
            }))
            .send().await;

        if let Ok(resp) = resp {
            if resp.status().is_success() {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    if let Some(id) = data["cloudaicompanionProject"].as_str() {
                        if !id.is_empty() { return Ok(id.to_string()); }
                    }
                    if let Some(id) = data["cloudaicompanionProject"]["id"].as_str() {
                        if !id.is_empty() { return Ok(id.to_string()); }
                    }
                }
            }
        }
    }

    if variant == GeminiVariant::Antigravity {
        return Ok(DEFAULT_PROJECT_ID.to_string());
    }

    let url = format!("{}/v1internal:onboardUser", CODE_ASSIST_ENDPOINT);
    let resp = client.post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "tierId": "free-tier",
            "metadata": { "ideType": "IDE_UNSPECIFIED", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI" }
        }))
        .send().await?;

    if resp.status().is_success() {
        let data: serde_json::Value = resp.json().await?;
        if let Some(id) = data["response"]["cloudaicompanionProject"]["id"].as_str() {
            return Ok(id.to_string());
        }
    }

    Err(eyre::eyre!("Could not discover Google Cloud project. Set GOOGLE_CLOUD_PROJECT env var."))
}

async fn get_user_email(client: &reqwest::Client, access_token: &str) -> Option<String> {
    let resp = client.get("https://www.googleapis.com/oauth2/v1/userinfo?alt=json")
        .header("Authorization", format!("Bearer {}", access_token))
        .send().await.ok()?;
    if resp.status().is_success() {
        let data: serde_json::Value = resp.json().await.ok()?;
        data["email"].as_str().map(|s| s.to_string())
    } else { None }
}

// -- URL encoding --

fn urlencoded(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{:02X}", b),
    }).collect()
}

fn urldecoded(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().unwrap_or(b'0');
            let lo = chars.next().unwrap_or(b'0');
            if let Ok(byte) = u8::from_str_radix(&format!("{}{}", hi as char, lo as char), 16) {
                result.push(byte as char);
            }
        } else if b == b'+' { result.push(' '); }
        else { result.push(b as char); }
    }
    result
}
