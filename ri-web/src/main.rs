use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use clap::Parser;
use color_eyre::eyre::Result;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

mod agent;
mod api;
mod state;

use state::AppState;

#[derive(Parser)]
#[command(name = "ri-web", about = "ri web interface")]
struct Cli {
    #[arg(long, default_value = "3001")]
    port: u16,

    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Dev mode: skip static file serving, enable permissive CORS.
    #[arg(long)]
    dev: bool,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    thinking: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    // Resolve provider and model.
    let model_id = cli.model
        .unwrap_or_else(|| ri_ai::registry::default_model_id().to_string());
    let (provider, model) = ri_ai::registry::resolve(&model_id).await?;

    let thinking = match cli.thinking.as_deref() {
        Some("off") => ri::ThinkingLevel::Off,
        Some("low") => ri::ThinkingLevel::Low,
        Some("high") => ri::ThinkingLevel::High,
        Some("xhigh") => ri::ThinkingLevel::XHigh,
        _ => ri::ThinkingLevel::Medium,
    };

    let tools: Vec<Arc<dyn ri::Tool>> = ri_tools::all_tools()
        .into_iter()
        .map(|t| Arc::from(t))
        .collect();

    let sessions_dir = ri::SessionStore::default_dir()?;

    let app_state = Arc::new(AppState {
        provider: Arc::from(provider),
        model,
        tools,
        thinking,
        sessions_dir,
        sessions: RwLock::new(std::collections::HashMap::new()),
    });

    // Build the API router.
    let api_routes = api::router(app_state.clone());

    let app = if cli.dev {
        tracing::info!("dev mode: CORS permissive, no static file serving");
        Router::new()
            .nest("/api", api_routes)
            .layer(CorsLayer::permissive())
    } else {
        // Serve built frontend from frontend/dist/, fallback to index.html for SPA routing.
        let frontend_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("frontend/dist");
        let index = frontend_dir.join("index.html");
        let serve = ServeDir::new(&frontend_dir)
            .fallback(ServeFile::new(&index));
        Router::new()
            .nest("/api", api_routes)
            .fallback_service(serve)
    };

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    tracing::info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
