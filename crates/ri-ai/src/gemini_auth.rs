// Gemini OAuth, credential management, and project discovery.
//
// Extracted from gemini.rs to keep the provider focused on request
// building and stream interpretation.

use std::path::PathBuf;
use serde_json::{json, Value};

use crate::creds::{self, Credentials};
use super::gemini::GeminiVariant;

// -- Credential storage --

pub fn creds_path(variant: GeminiVariant) -> eyre::Result<PathBuf> {
    let name = match variant {
        GeminiVariant::Cli => "gemini_cli_auth.json",
        GeminiVariant::Antigravity => "gemini_antigravity_auth.json",
    };
    Ok(creds::ri_dir()?.join(name))
}

pub fn load_creds(variant: GeminiVariant) -> Option<Credentials> {
    creds::load(&creds_path(variant).ok()?)
}

pub fn save_creds(variant: GeminiVariant, creds: &Credentials) -> eyre::Result<()> {
    creds::save(&creds_path(variant)?, creds)
}

// -- OAuth constants --

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

pub struct VariantConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: &'static str,
    pub scopes: &'static [&'static str],
    pub port: u16,
    pub callback_path: &'static str,
}

pub fn config_for(variant: GeminiVariant) -> VariantConfig {
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

// -- Auth URL construction --

pub fn build_auth_url(variant: GeminiVariant, challenge: &str, login_state: &str) -> String {
    let cfg = config_for(variant);
    let scopes = cfg.scopes.join(" ");
    format!(
        "{}?client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&access_type=offline&prompt=consent",
        AUTH_URL,
        urlencoding::encode(&cfg.client_id),
        urlencoding::encode(cfg.redirect_uri),
        urlencoding::encode(&scopes),
        urlencoding::encode(challenge),
        urlencoding::encode(login_state),
    )
}

// -- Token exchange and refresh --

pub async fn exchange_code(
    variant: GeminiVariant,
    code: &str,
    verifier: &str,
) -> eyre::Result<Credentials> {
    let cfg = config_for(variant);
    let client = reqwest::Client::new();
    let response = client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", cfg.redirect_uri),
            ("code_verifier", verifier),
        ])
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(eyre::eyre!("Token exchange failed ({}): {}", status, body));
    }

    let data: Value = response.json().await?;
    let access = data["access_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token"))?.to_string();
    let refresh = data["refresh_token"].as_str()
        .ok_or_else(|| eyre::eyre!("No refresh token"))?.to_string();
    let expires_in = data["expires_in"].as_u64().unwrap_or(3600);
    let expires = Credentials::compute_expiry(expires_in);

    let email = get_user_email(&client, &access).await;
    let project_id = discover_project(&client, &access, variant).await?;

    Ok(Credentials {
        refresh_token: refresh, access_token: access, expires,
        project_id: Some(project_id), email,
    })
}

pub async fn refresh_token(credentials: &Credentials, variant: GeminiVariant) -> eyre::Result<Credentials> {
    let cfg = config_for(variant);
    let client = reqwest::Client::new();

    let response = client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
            ("refresh_token", credentials.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(eyre::eyre!("Google token refresh failed ({}): {}", status, body));
    }

    let data: Value = response.json().await?;
    let access = data["access_token"].as_str()
        .ok_or_else(|| eyre::eyre!("Missing access_token"))?.to_string();
    let refresh = data["refresh_token"].as_str()
        .unwrap_or(&credentials.refresh_token).to_string();
    let expires_in = data["expires_in"].as_u64().unwrap_or(3600);
    let expires = Credentials::compute_expiry(expires_in);

    Ok(Credentials {
        refresh_token: refresh, access_token: access, expires,
        project_id: credentials.project_id.clone(),
        email: credentials.email.clone(),
    })
}

// -- Project discovery --

/// API endpoints used for project discovery and streaming.
pub const GEMINI_CLI_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
pub const ANTIGRAVITY_DAILY_ENDPOINT: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";

async fn discover_project(
    client: &reqwest::Client,
    access_token: &str,
    variant: GeminiVariant,
) -> eyre::Result<String> {
    if let Ok(id) = std::env::var("GOOGLE_CLOUD_PROJECT") { return Ok(id); }
    if let Ok(id) = std::env::var("GOOGLE_CLOUD_PROJECT_ID") { return Ok(id); }

    let endpoints = match variant {
        GeminiVariant::Antigravity => vec![GEMINI_CLI_ENDPOINT, ANTIGRAVITY_DAILY_ENDPOINT],
        GeminiVariant::Cli => vec![GEMINI_CLI_ENDPOINT],
    };

    for endpoint in &endpoints {
        let url = format!("{}/v1internal:loadCodeAssist", endpoint);
        let resp = client.post(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .json(&json!({
                "metadata": { "ideType": "IDE_UNSPECIFIED", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI" }
            }))
            .send().await;

        let Ok(resp) = resp else { continue; };
        if !resp.status().is_success() { continue; }
        let Ok(data) = resp.json::<Value>().await else { continue; };

        let project = data["cloudaicompanionProject"].as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| data["cloudaicompanionProject"]["id"].as_str().filter(|s| !s.is_empty()));
        if let Some(id) = project {
            return Ok(id.to_string());
        }
    }

    if variant == GeminiVariant::Antigravity {
        return Ok(DEFAULT_PROJECT_ID.to_string());
    }

    let url = format!("{}/v1internal:onboardUser", GEMINI_CLI_ENDPOINT);
    let resp = client.post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .json(&json!({
            "tierId": "free-tier",
            "metadata": { "ideType": "IDE_UNSPECIFIED", "platform": "PLATFORM_UNSPECIFIED", "pluginType": "GEMINI" }
        }))
        .send().await?;

    if resp.status().is_success() {
        let data: Value = resp.json().await?;
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
        let data: Value = resp.json().await.ok()?;
        data["email"].as_str().map(|s| s.to_string())
    } else { None }
}
