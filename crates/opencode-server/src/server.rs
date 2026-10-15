use async_trait::async_trait;
use axum::{
    extract::State,
    http::{header, header::HeaderValue, request::Parts, HeaderName, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Json,
};
use futures::StreamExt;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::sync::{broadcast, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use url::Url;

use opencode_config::load_config;
use opencode_plugin::init_global;
use opencode_plugin::subprocess::{
    PluginAuthBridge, PluginContext, PluginFetchRequest, PluginLoader,
};
use opencode_provider::{
    bootstrap_config_from_raw, create_registry_from_bootstrap_config, register_custom_fetch_proxy,
    unregister_custom_fetch_proxy, AuthInfo, AuthManager, ConfigModel as BootstrapConfigModel,
    ConfigProvider as BootstrapConfigProvider, CustomFetchProxy, CustomFetchRequest,
    CustomFetchResponse, CustomFetchStreamResponse, ProviderError, ProviderRegistry,
};
use opencode_session::SessionManager;
use opencode_storage::InputRepository;
use opencode_storage::{Database, MessageRepository, SessionRepository};

use crate::routes;

const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:4096";

fn generate_auth_token() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn is_auth_enabled() -> bool {
    std::env::var("OPENCODE_NO_AUTH").is_err()
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionRequestInfo {
    pub id: String,
    pub session_id: String,
    pub tool: String,
    pub input: serde_json::Value,
    pub message: String,
    pub permission_type: String,
    pub patterns: Vec<String>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub always: Vec<String>,
}

#[derive(Debug)]
struct PendingPermission {
    info: PermissionRequestInfo,
    responder: oneshot::Sender<PermissionResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PermissionResponse {
    Once,
    Always,
    Reject { message: Option<String> },
}

pub struct PermissionBroker {
    pending: Mutex<HashMap<String, PendingPermission>>,
    event_bus: broadcast::Sender<String>,
}

impl PermissionBroker {
    pub fn new(event_bus: broadcast::Sender<String>) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            event_bus,
        }
    }

    pub async fn ask(&self, info: PermissionRequestInfo) -> PermissionResponse {
        let (tx, rx) = oneshot::channel();
        let pending = PendingPermission {
            info: info.clone(),
            responder: tx,
        };

        {
            let mut pending_map = self.pending.lock().await;
            pending_map.insert(info.id.clone(), pending);
        }

        // Broadcast permission request event
        let _ = self.event_bus.send(
            serde_json::json!({
                "type": "permission.requested",
                "request": info
            })
            .to_string(),
        );

        // Wait for response
        match rx.await {
            Ok(response) => response,
            Err(_) => PermissionResponse::Reject {
                message: Some("Permission request cancelled".to_string()),
            },
        }
    }

    pub async fn reply(&self, id: &str, response: PermissionResponse) -> bool {
        let pending = {
            let mut pending_map = self.pending.lock().await;
            pending_map.remove(id)
        };

        if let Some(pending) = pending {
            let _ = pending.responder.send(response.clone());

            // Broadcast permission reply event
            let _ = self.event_bus.send(
                serde_json::json!({
                    "type": "permission.replied",
                    "requestID": id,
                    "sessionID": pending.info.session_id,
                    "reply": match response {
                        PermissionResponse::Once => "once",
                        PermissionResponse::Always => "always",
                        PermissionResponse::Reject { .. } => "reject",
                    },
                    "message": match response {
                        PermissionResponse::Reject { message } => message,
                        _ => None,
                    },
                })
                .to_string(),
            );
            true
        } else {
            false
        }
    }

    pub async fn list(&self) -> Vec<PermissionRequestInfo> {
        let pending_map = self.pending.lock().await;
        pending_map.values().map(|p| p.info.clone()).collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QuestionRequestInfo {
    pub id: String,
    pub session_id: String,
    pub questions: Vec<opencode_tool::QuestionDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QuestionResponse {
    Answered(Vec<Vec<String>>),
    Rejected,
}

#[derive(Debug)]
struct PendingQuestion {
    info: QuestionRequestInfo,
    responder: oneshot::Sender<QuestionResponse>,
}

pub struct QuestionBroker {
    pending: Mutex<HashMap<String, PendingQuestion>>,
    event_bus: broadcast::Sender<String>,
}

impl QuestionBroker {
    pub fn new(event_bus: broadcast::Sender<String>) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            event_bus,
        }
    }

    pub async fn ask(&self, info: QuestionRequestInfo) -> QuestionResponse {
        let (tx, rx) = oneshot::channel();
        let pending = PendingQuestion {
            info: info.clone(),
            responder: tx,
        };

        {
            let mut pending_map = self.pending.lock().await;
            pending_map.insert(info.id.clone(), pending);
        }

        tracing::info!(
            question_id = %info.id,
            session_id = %info.session_id,
            questions = info.questions.len(),
            "question requested; waiting for an answer"
        );

        // Broadcast question request event
        let _ = self.event_bus.send(
            serde_json::json!({
                "type": "question.asked",
                "request": info
            })
            .to_string(),
        );

        // Wait for an answer from the API client
        match rx.await {
            Ok(response) => response,
            Err(_) => QuestionResponse::Rejected,
        }
    }

    pub async fn reply(&self, id: &str, answers: Vec<Vec<String>>) -> bool {
        let pending = {
            let mut pending_map = self.pending.lock().await;
            pending_map.remove(id)
        };

        if let Some(pending) = pending {
            tracing::info!(
                question_id = %id,
                session_id = %pending.info.session_id,
                answered = answers.len(),
                "question answered"
            );

            let _ = pending
                .responder
                .send(QuestionResponse::Answered(answers.clone()));

            // Broadcast question reply event
            let _ = self.event_bus.send(
                serde_json::json!({
                    "type": "question.replied",
                    "requestID": id,
                    "sessionID": pending.info.session_id,
                    "answers": answers,
                })
                .to_string(),
            );
            true
        } else {
            tracing::warn!(question_id = %id, "reply for unknown question request");
            false
        }
    }

    pub async fn reject(&self, id: &str) -> bool {
        let pending = {
            let mut pending_map = self.pending.lock().await;
            pending_map.remove(id)
        };

        if let Some(pending) = pending {
            tracing::info!(
                question_id = %id,
                session_id = %pending.info.session_id,
                "question rejected"
            );

            let _ = pending.responder.send(QuestionResponse::Rejected);

            // Broadcast question rejection event
            let _ = self.event_bus.send(
                serde_json::json!({
                    "type": "question.rejected",
                    "requestID": id,
                    "sessionID": pending.info.session_id,
                })
                .to_string(),
            );
            true
        } else {
            tracing::warn!(question_id = %id, "reject for unknown question request");
            false
        }
    }

    pub async fn list(&self) -> Vec<QuestionRequestInfo> {
        let pending_map = self.pending.lock().await;
        let mut result: Vec<_> = pending_map.values().map(|p| p.info.clone()).collect();
        result.sort_by(|a, b| a.id.cmp(&b.id));
        result
    }
}

struct PluginBridgeFetchProxy {
    bridge: Arc<PluginAuthBridge>,
}

#[async_trait]
impl CustomFetchProxy for PluginBridgeFetchProxy {
    async fn fetch(
        &self,
        request: CustomFetchRequest,
    ) -> Result<CustomFetchResponse, ProviderError> {
        let response = self
            .bridge
            .fetch_proxy(PluginFetchRequest {
                url: request.url,
                method: request.method,
                headers: request.headers,
                body: request.body,
            })
            .await
            .map_err(|e| ProviderError::NetworkError(e.to_string()))?;

        Ok(CustomFetchResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }

    async fn fetch_stream(
        &self,
        request: CustomFetchRequest,
    ) -> Result<CustomFetchStreamResponse, ProviderError> {
        let response = self
            .bridge
            .fetch_proxy_stream(PluginFetchRequest {
                url: request.url,
                method: request.method,
                headers: request.headers,
                body: request.body,
            })
            .await
            .map_err(|e| ProviderError::NetworkError(e.to_string()))?;

        let stream = ReceiverStream::new(response.chunks)
            .map(|item| item.map_err(|e| ProviderError::NetworkError(e.to_string())));
        Ok(CustomFetchStreamResponse {
            status: response.status,
            headers: response.headers,
            stream: Box::pin(stream),
        })
    }
}

pub(crate) fn sync_custom_fetch_proxy(
    provider_id: &str,
    bridge: Arc<PluginAuthBridge>,
    enabled: bool,
) {
    if enabled {
        register_custom_fetch_proxy(
            provider_id.to_string(),
            Arc::new(PluginBridgeFetchProxy {
                bridge: bridge.clone(),
            }),
        );
        if provider_id == "github-copilot" {
            register_custom_fetch_proxy(
                "github-copilot-enterprise",
                Arc::new(PluginBridgeFetchProxy { bridge }),
            );
        }
    } else {
        unregister_custom_fetch_proxy(provider_id);
        if provider_id == "github-copilot" {
            unregister_custom_fetch_proxy("github-copilot-enterprise");
        }
    }
}

pub struct ServerState {
    pub sessions: Mutex<SessionManager>,
    pub providers: ProviderRegistry,
    pub auth_manager: Arc<AuthManager>,
    pub event_bus: broadcast::Sender<String>,
    pub auth_token: Option<String>,
    pub permission_broker: Arc<PermissionBroker>,
    pub question_broker: Arc<QuestionBroker>,
    session_repo: Option<SessionRepository>,
    message_repo: Option<MessageRepository>,
    pub(crate) input_repo: Option<InputRepository>,
}

impl ServerState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        let auth_token = generate_auth_token();
        let permission_broker = Arc::new(PermissionBroker::new(tx.clone()));
        let question_broker = Arc::new(QuestionBroker::new(tx.clone()));
        Self {
            sessions: Mutex::new(SessionManager::new()),
            providers: ProviderRegistry::new(),
            auth_manager: Arc::new(AuthManager::new()),
            event_bus: tx,
            auth_token: Some(auth_token),
            permission_broker,
            question_broker,
            session_repo: None,
            message_repo: None,
            input_repo: None,
        }
    }

    pub async fn new_with_storage() -> anyhow::Result<Self> {
        Self::new_with_storage_for_url(DEFAULT_SERVER_URL.to_string()).await
    }

    pub async fn new_with_storage_for_url(server_url: String) -> anyhow::Result<Self> {
        let mut state = Self::new();
        let auth_manager = Arc::new(AuthManager::load_from_file(&auth_data_dir()).await);
        state.auth_manager = auth_manager.clone();
        load_plugin_auth_store(&server_url, auth_manager.clone()).await;
        let auth_store = auth_manager.list().await;

        // Load config and convert providers to bootstrap format
        let cwd = std::env::current_dir().unwrap_or_default();
        let bootstrap_config = match load_config(&cwd) {
            Ok(config) => {
                let providers = convert_config_providers_for_bootstrap(&config);
                bootstrap_config_from_raw(
                    providers,
                    config.disabled_providers.clone(),
                    config.enabled_providers.clone(),
                    config.model.clone(),
                    config.small_model.clone(),
                )
            }
            Err(error) => {
                tracing::warn!(%error, "failed to load config for provider bootstrap, using defaults");
                opencode_provider::BootstrapConfig::default()
            }
        };

        state.providers = create_registry_from_bootstrap_config(&bootstrap_config, &auth_store);
        let db = Database::new().await?;
        let pool = db.pool().clone();
        state.session_repo = Some(SessionRepository::new(pool.clone()));
        state.message_repo = Some(MessageRepository::new(pool.clone()));
        state.input_repo = Some(InputRepository::new(pool));
        state.load_sessions_from_storage().await?;
        if let Some(input_repo) = &state.input_repo {
            input_repo.reset_processing().await?;
        }
        Ok(state)
    }

    pub fn broadcast(&self, event: &str) {
        let _ = self.event_bus.send(event.to_string());
    }

    async fn load_sessions_from_storage(&self) -> anyhow::Result<()> {
        let (Some(session_repo), Some(message_repo)) = (&self.session_repo, &self.message_repo)
        else {
            return Ok(());
        };

        let stored_sessions = session_repo.list(None, 100_000).await?;
        let mut manager = self.sessions.lock().await;

        for stored in stored_sessions {
            let stored_messages: Vec<opencode_session::SessionMessage> = serde_json::from_value(
                serde_json::to_value(message_repo.list_for_session(&stored.id.0).await?)?,
            )?;
            let session = opencode_session::Session {
                id: stored.id.0,
                slug: stored.slug,
                project_id: stored.project_id.0,
                directory: stored.directory,
                parent_id: stored.parent_id.map(|id| id.0),
                title: stored.title,
                version: stored.version,
                time: opencode_session::SessionTime {
                    created: stored.time.created as i64,
                    updated: stored.time.updated as i64,
                    compacting: stored.time.compacting.map(|t| t as i64),
                    archived: stored.time.archived.map(|t| t as i64),
                },
                messages: stored_messages,
                summary: None,
                share: stored
                    .share
                    .map(|s| opencode_session::SessionShare { url: s.url }),
                revert: stored.revert.map(|r| opencode_session::SessionRevert {
                    message_id: r.message_id,
                    part_id: r.part_id,
                    snapshot: r.snapshot,
                    diff: r.diff,
                }),
                permission: stored
                    .permission
                    .map(|p| opencode_session::PermissionRuleset {
                        allow: p.allow,
                        deny: p.deny,
                        mode: p.mode,
                    }),
                usage: stored.usage.map(|u| opencode_session::SessionUsage {
                    input_tokens: u.input as u64,
                    output_tokens: u.output as u64,
                    reasoning_tokens: u.reasoning as u64,
                    cache_write_tokens: u.cache.write as u64,
                    cache_read_tokens: u.cache.read as u64,
                    total_cost: u.input + u.output,
                }),
                status: match stored.status {
                    opencode_types::SessionStatus::Active => {
                        opencode_session::SessionStatus::Active
                    }
                    opencode_types::SessionStatus::Completed => {
                        opencode_session::SessionStatus::Completed
                    }
                    opencode_types::SessionStatus::Archived => {
                        opencode_session::SessionStatus::Archived
                    }
                    opencode_types::SessionStatus::Compacting => {
                        opencode_session::SessionStatus::Compacting
                    }
                },
                metadata: HashMap::new(),
                created_at: stored.created_at,
                updated_at: stored.updated_at,
            };
            manager.update(session);
        }

        Ok(())
    }

    pub async fn sync_sessions_to_storage(&self) -> anyhow::Result<()> {
        let (Some(session_repo), Some(message_repo)) = (&self.session_repo, &self.message_repo)
        else {
            return Ok(());
        };

        let snapshot: Vec<opencode_session::Session> = {
            let manager = self.sessions.lock().await;
            manager.list().into_iter().cloned().collect()
        };

        let snapshot_ids: HashSet<String> = snapshot.iter().map(|s| s.id.clone()).collect();
        let persisted = session_repo.list(None, 100_000).await?;

        for stale in persisted {
            if !snapshot_ids.contains(&stale.id.0) {
                message_repo.delete_for_session(&stale.id.0).await?;
                session_repo.delete(&stale.id.0).await?;
            }
        }

        for session in snapshot {
            let stored_session: opencode_types::Session =
                serde_json::from_value(serde_json::to_value(&session)?)?;

            // Convert messages to opencode_types::SessionMessage
            let messages: Vec<opencode_types::SessionMessage> =
                serde_json::from_value(serde_json::to_value(&session.messages)?)?;

            if session_repo.get(&stored_session.id.0).await?.is_some() {
                session_repo.update(&stored_session).await?;
            } else {
                session_repo.create(&stored_session).await?;
            }

            message_repo
                .delete_for_session(&stored_session.id.0)
                .await?;
            for message in messages {
                message_repo.create(&message).await?;
            }
        }

        Ok(())
    }
}

/// Convert opencode_config::ProviderConfig map to bootstrap ConfigProvider map.
fn convert_config_providers_for_bootstrap(
    config: &opencode_config::Config,
) -> std::collections::HashMap<String, BootstrapConfigProvider> {
    let Some(ref providers) = config.provider else {
        return std::collections::HashMap::new();
    };

    providers
        .iter()
        .map(|(id, provider)| (id.clone(), provider_to_bootstrap(provider)))
        .collect()
}

fn provider_to_bootstrap(provider: &opencode_config::ProviderConfig) -> BootstrapConfigProvider {
    let mut options = provider.options.clone().unwrap_or_default();
    if let Some(api_key) = &provider.api_key {
        options
            .entry("apiKey".to_string())
            .or_insert_with(|| serde_json::Value::String(api_key.clone()));
    }
    if let Some(base_url) = &provider.base_url {
        options
            .entry("baseURL".to_string())
            .or_insert_with(|| serde_json::Value::String(base_url.clone()));
    }

    let models = provider.models.as_ref().map(|models| {
        models
            .iter()
            .map(|(id, model)| (id.clone(), model_to_bootstrap(id, model)))
            .collect()
    });

    BootstrapConfigProvider {
        name: provider.name.clone(),
        api: provider.base_url.clone(),
        npm: provider.npm.clone(),
        options: (!options.is_empty()).then_some(options),
        models,
        blacklist: (!provider.blacklist.is_empty()).then_some(provider.blacklist.clone()),
        whitelist: (!provider.whitelist.is_empty()).then_some(provider.whitelist.clone()),
        ..Default::default()
    }
}

fn model_to_bootstrap(id: &str, model: &opencode_config::ModelConfig) -> BootstrapConfigModel {
    let mut options = HashMap::new();
    if let Some(api_key) = &model.api_key {
        options.insert(
            "apiKey".to_string(),
            serde_json::Value::String(api_key.clone()),
        );
    }

    let variants = model.variants.as_ref().map(|variants| {
        variants
            .iter()
            .map(|(name, variant)| (name.clone(), variant_to_bootstrap(variant)))
            .collect()
    });

    BootstrapConfigModel {
        id: model.model.clone().or_else(|| Some(id.to_string())),
        name: model.name.clone(),
        provider: model.base_url.as_ref().map(|url| {
            opencode_provider::bootstrap::ConfigModelProvider {
                api: Some(url.clone()),
                npm: None,
            }
        }),
        options: (!options.is_empty()).then_some(options),
        variants,
        ..Default::default()
    }
}

fn variant_to_bootstrap(
    variant: &opencode_config::ModelVariantConfig,
) -> HashMap<String, serde_json::Value> {
    let mut values = variant.extra.clone();
    if let Some(disabled) = variant.disabled {
        values.insert("disabled".to_string(), serde_json::Value::Bool(disabled));
    }
    values
}

fn auth_data_dir() -> PathBuf {
    if let Ok(path) = std::env::var("OPENCODE_DATA_DIR") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("opencode")
        .join("data")
}

async fn load_plugin_auth_store(server_url: &str, auth_manager: Arc<AuthManager>) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            tracing::warn!(%error, "failed to get current directory for plugin bootstrap");
            return;
        }
    };

    let config = match load_config(&cwd) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(%error, "failed to load config for plugin bootstrap");
            return;
        }
    };

    let loader = match PluginLoader::new() {
        Ok(loader) => Arc::new(loader),
        Err(error) => {
            tracing::warn!(%error, "failed to initialize plugin loader");
            return;
        }
    };
    init_global(loader.hook_system());

    let directory = cwd.to_string_lossy().to_string();
    let context = PluginContext {
        worktree: directory.clone(),
        directory,
        server_url: server_url.to_string(),
    };

    if let Err(error) = loader.load_builtins(&context).await {
        tracing::warn!(%error, "failed to load builtin auth plugins");
    }

    if !config.plugin.is_empty() {
        if let Err(error) = loader.load_all(&config.plugin, &context).await {
            tracing::warn!(%error, "failed to load configured plugins");
            return;
        }
    }

    routes::set_plugin_loader(loader.clone());

    let bridges = loader.auth_bridges().await;
    for (provider_id, bridge) in bridges {
        match bridge.load().await {
            Ok(result) => {
                sync_custom_fetch_proxy(&provider_id, bridge.clone(), result.has_custom_fetch);

                if let Some(api_key) = result.api_key {
                    auth_manager
                        .set(
                            &provider_id,
                            AuthInfo::Api {
                                key: api_key.clone(),
                            },
                        )
                        .await;
                    // TS parity: copilot auth can power both standard and enterprise providers.
                    if provider_id == "github-copilot" {
                        auth_manager
                            .set("github-copilot-enterprise", AuthInfo::Api { key: api_key })
                            .await;
                    }
                }
            }
            Err(error) => {
                sync_custom_fetch_proxy(&provider_id, bridge.clone(), false);
                tracing::warn!(provider = provider_id, %error, "failed to load plugin auth");
            }
        }
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

static EXTRA_CORS_WHITELIST: Lazy<RwLock<HashSet<String>>> =
    Lazy::new(|| RwLock::new(HashSet::new()));

fn normalize_origin(origin: &str) -> Option<String> {
    let trimmed = origin.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn set_cors_whitelist(origins: Vec<String>) {
    let mut next = HashSet::new();
    for origin in origins {
        if let Some(normalized) = normalize_origin(&origin) {
            next.insert(normalized);
        }
    }

    match EXTRA_CORS_WHITELIST.write() {
        Ok(mut guard) => *guard = next,
        Err(poisoned) => *poisoned.into_inner() = next,
    }
}

fn is_allowed_origin(origin: &str) -> bool {
    let parsed = Url::parse(origin);
    let Ok(parsed) = parsed else {
        return false;
    };

    let scheme = parsed.scheme();
    let host = parsed.host_str().unwrap_or("");
    let port = parsed.port();

    // Allow localhost with any port
    let is_localhost = matches!(host, "localhost" | "127.0.0.1" | "[::1]")
        && matches!(scheme, "http" | "https")
        && port.is_some();

    // Allow tauri
    let is_tauri = matches!(
        origin,
        "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost"
    );

    // Allow exact opencode.ai subdomains (no wildcard suffix)
    let is_opencode_ai = scheme == "https" && matches!(host, "opencode.ai" | "www.opencode.ai");

    // Check extra whitelist with exact match
    let is_extra = is_extra_allowed_origin_exact(origin);

    is_localhost || is_tauri || is_opencode_ai || is_extra
}

fn is_extra_allowed_origin_exact(origin: &str) -> bool {
    let normalized = normalize_origin(origin).unwrap_or_else(|| origin.to_string());
    match EXTRA_CORS_WHITELIST.read() {
        Ok(guard) => guard.contains(&normalized),
        Err(poisoned) => poisoned.into_inner().contains(&normalized),
    }
}

fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(
            |origin: &HeaderValue, _parts: &Parts| {
                origin.to_str().map(is_allowed_origin).unwrap_or(false)
            },
        ))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT])
}

async fn auth_middleware(
    state: Arc<ServerState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if !is_auth_enabled() {
        return next.run(request).await;
    }

    let expected_token = state.auth_token.as_deref();
    let Some(expected) = expected_token else {
        return next.run(request).await;
    };

    let headers = request.headers();
    let auth_header = headers
        .get(HeaderName::from_static("authorization"))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match auth_header {
        Some(token) if token == expected => next.run(request).await,
        _ => {
            tracing::warn!("Authentication failed: invalid or missing token");
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "unauthorized",
                    "message": "Invalid or missing authentication token"
                })),
            )
                .into_response()
        }
    }
}

pub async fn run_server(addr: SocketAddr) -> anyhow::Result<()> {
    let state = Arc::new(ServerState::new_with_storage_for_url(format!("http://{}", addr)).await?);
    routes::resume_queued_prompts(state.clone()).await?;

    if is_auth_enabled() {
        if let Some(token) = &state.auth_token {
            tracing::info!("Authentication enabled. Token: {}", token);
            tracing::info!("Set Authorization: Bearer <token> header for all requests");
        }
    } else {
        tracing::warn!("Authentication DISABLED (OPENCODE_NO_AUTH=1). Not safe for production.");
    }

    let app = routes::router()
        .layer(middleware::from_fn_with_state(
            state.clone(),
            |State(state): State<Arc<ServerState>>,
             request: Request<axum::body::Body>,
             next: Next| async move { auth_middleware(state, request, next).await },
        ))
        .layer(cors_layer())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("Server listening on {}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

pub async fn run_server_with_state(
    addr: SocketAddr,
    state: Arc<ServerState>,
) -> anyhow::Result<()> {
    routes::resume_queued_prompts(state.clone()).await?;
    if is_auth_enabled() {
        if let Some(token) = &state.auth_token {
            tracing::info!("Authentication enabled. Token: {}", token);
        }
    } else {
        tracing::warn!("Authentication DISABLED (OPENCODE_NO_AUTH=1). Not safe for production.");
    }

    let app = routes::router()
        .layer(middleware::from_fn_with_state(
            state.clone(),
            |State(state): State<Arc<ServerState>>,
             request: Request<axum::body::Body>,
             next: Next| async move { auth_middleware(state, request, next).await },
        ))
        .layer(cors_layer())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("Server listening on {}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn state_with_repos(
        session_repo: SessionRepository,
        message_repo: MessageRepository,
    ) -> ServerState {
        let mut state = ServerState::new();
        state.session_repo = Some(session_repo);
        state.message_repo = Some(message_repo);
        state
    }

    #[tokio::test]
    async fn storage_roundtrip_restores_sessions_and_messages() {
        let db = Database::in_memory()
            .await
            .expect("in-memory db should initialize");
        let pool = db.pool().clone();

        let state = state_with_repos(
            SessionRepository::new(pool.clone()),
            MessageRepository::new(pool.clone()),
        );
        let (session_id, user_created_at, assistant_created_at) = {
            let mut manager = state.sessions.lock().await;
            let session = manager.create("default", ".");
            let session_id = session.id.clone();

            let fixed_user_time = chrono::Utc
                .timestamp_millis_opt(1_700_000_000_000)
                .single()
                .expect("valid user timestamp");
            let fixed_assistant_time = chrono::Utc
                .timestamp_millis_opt(1_700_000_000_123)
                .single()
                .expect("valid assistant timestamp");

            let session = manager
                .get_mut(&session_id)
                .expect("session should be available for mutation");
            let user = session.add_user_message("hello");
            user.created_at = fixed_user_time;
            if let Some(part) = user.parts.first_mut() {
                part.created_at = fixed_user_time;
            }

            let assistant = session.add_assistant_message();
            assistant.created_at = fixed_assistant_time;
            assistant.add_text("world");
            if let Some(part) = assistant.parts.first_mut() {
                part.created_at = fixed_assistant_time;
            }

            (session_id, fixed_user_time, fixed_assistant_time)
        };

        state
            .sync_sessions_to_storage()
            .await
            .expect("session snapshot should sync to storage");

        let reloaded = state_with_repos(
            SessionRepository::new(pool.clone()),
            MessageRepository::new(pool),
        );
        reloaded
            .load_sessions_from_storage()
            .await
            .expect("sessions should reload from storage");

        let manager = reloaded.sessions.lock().await;
        let session = manager
            .get(&session_id)
            .expect("session should exist after reload");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].created_at, user_created_at);
        assert_eq!(session.messages[1].created_at, assistant_created_at);
        assert_eq!(session.messages[0].get_text(), "hello");
        assert_eq!(session.messages[1].get_text(), "world");
    }

    #[tokio::test]
    async fn sync_removes_deleted_sessions_from_storage() {
        let db = Database::in_memory()
            .await
            .expect("in-memory db should initialize");
        let pool = db.pool().clone();
        let session_repo = SessionRepository::new(pool.clone());

        let state = state_with_repos(
            SessionRepository::new(pool.clone()),
            MessageRepository::new(pool),
        );
        let session_id = {
            let mut manager = state.sessions.lock().await;
            manager.create("default", ".").id
        };

        state
            .sync_sessions_to_storage()
            .await
            .expect("initial snapshot should sync");
        assert_eq!(
            session_repo
                .list(None, 10)
                .await
                .expect("list should succeed")
                .len(),
            1
        );

        {
            let mut manager = state.sessions.lock().await;
            manager.delete(&session_id);
        }

        state
            .sync_sessions_to_storage()
            .await
            .expect("delete sync should succeed");
        assert!(session_repo
            .get(&session_id)
            .await
            .expect("get should succeed")
            .is_none());
    }

    fn question_defs() -> Vec<opencode_tool::QuestionDef> {
        vec![opencode_tool::QuestionDef {
            question: "Continue?".to_string(),
            header: Some("Confirm".to_string()),
            options: vec![opencode_tool::QuestionOption {
                label: "Yes".to_string(),
                description: None,
            }],
            multiple: false,
        }]
    }

    async fn next_question_event(rx: &mut broadcast::Receiver<String>) -> serde_json::Value {
        let event = rx.recv().await.expect("broker should broadcast an event");
        serde_json::from_str(&event).expect("broadcast payload should be valid JSON")
    }

    #[tokio::test]
    async fn question_broker_ask_resolves_with_reply_answers() {
        let (tx, mut rx) = broadcast::channel(16);
        let broker = Arc::new(QuestionBroker::new(tx));
        let info = QuestionRequestInfo {
            id: "qst_1".to_string(),
            session_id: "ses_1".to_string(),
            questions: question_defs(),
        };

        let ask_broker = broker.clone();
        let ask_task = tokio::spawn(async move { ask_broker.ask(info).await });

        let event = next_question_event(&mut rx).await;
        assert_eq!(event["type"], "question.asked");
        assert_eq!(event["request"]["id"], "qst_1");
        assert_eq!(event["request"]["session_id"], "ses_1");

        let pending = broker.list().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].questions[0].question, "Continue?");

        assert!(broker.reply("qst_1", vec![vec!["Yes".to_string()]]).await);

        match ask_task.await.expect("ask task should join") {
            QuestionResponse::Answered(answers) => {
                assert_eq!(answers, vec![vec!["Yes".to_string()]]);
            }
            other => panic!("expected answered response, got {:?}", other),
        }

        let event = next_question_event(&mut rx).await;
        assert_eq!(event["type"], "question.replied");
        assert_eq!(event["requestID"], "qst_1");
        assert_eq!(event["sessionID"], "ses_1");
        assert!(broker.list().await.is_empty());
    }

    #[tokio::test]
    async fn question_broker_reject_resolves_with_rejection() {
        let (tx, mut rx) = broadcast::channel(16);
        let broker = Arc::new(QuestionBroker::new(tx));
        let info = QuestionRequestInfo {
            id: "qst_2".to_string(),
            session_id: "ses_2".to_string(),
            questions: question_defs(),
        };

        let ask_broker = broker.clone();
        let ask_task = tokio::spawn(async move { ask_broker.ask(info).await });

        let event = next_question_event(&mut rx).await;
        assert_eq!(event["type"], "question.asked");

        assert!(broker.reject("qst_2").await);
        assert!(matches!(
            ask_task.await.expect("ask task should join"),
            QuestionResponse::Rejected
        ));

        let event = next_question_event(&mut rx).await;
        assert_eq!(event["type"], "question.rejected");
        assert_eq!(event["requestID"], "qst_2");
        assert!(broker.list().await.is_empty());
    }

    #[tokio::test]
    async fn question_broker_unknown_ids_return_false() {
        let (tx, _rx) = broadcast::channel(16);
        let broker = QuestionBroker::new(tx);

        assert!(!broker.reply("missing", Vec::new()).await);
        assert!(!broker.reject("missing").await);
        assert!(broker.list().await.is_empty());
    }
}
