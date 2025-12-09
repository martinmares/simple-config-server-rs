use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use clap::Parser;
use mime_guess::MimeGuess;
use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value as YamlValue;
use thiserror::Error;
use tokio::{net::TcpListener, process::Command, time};
use tracing::{debug, error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

/// CLI - zatím jen cesta ke konfigu
#[derive(Parser, Debug)]
#[command(name = "secure-config-server")]
struct Cli {
    /// Path to config.yaml
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
struct GitConfig {
    repo_url: String,
    branch: String,
    workdir: PathBuf,
    #[serde(default)]
    subpath: Option<PathBuf>, // = search-paths
    refresh_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct HttpConfig {
    /// např. "127.0.0.1:8080"
    bind_addr: String,
    /// prefix path, např. "/subpath" nebo "/" nebo prázdné
    base_path: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Config {
    git: GitConfig,
    http: HttpConfig,
}

#[derive(Clone)]
struct AuthConfig {
    required: bool,
    username: String,
    password: String,
}

impl AuthConfig {
    fn from_env() -> Self {
        let user = std::env::var("AUTH_USERNAME").ok();
        let pass = std::env::var("AUTH_PASSWORD").ok();
        match (user, pass) {
            (Some(u), Some(p)) => {
                info!("[auth] Basic auth enabled (AUTH_USERNAME/AUTH_PASSWORD)");
                AuthConfig {
                    required: true,
                    username: u,
                    password: p,
                }
            }
            _ => {
                warn!("[auth] Basic auth disabled (env AUTH_USERNAME / AUTH_PASSWORD not set)");
                AuthConfig {
                    required: false,
                    username: String::new(),
                    password: String::new(),
                }
            }
        }
    }
}

#[derive(Clone)]
struct AppState {
    config: Config,
    env_map: Arc<HashMap<String, String>>,
    auth: AuthConfig,
}

#[derive(Debug, Error)]
enum ServerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),

    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error("Not found")]
    NotFound,

    #[error("Bad request: {0}")]
    BadRequest(String),
}

// Regex pro {{ VAR_NAME }} templating
static TEMPLATE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\{\{\s*([A-Z0-9_]+)\s*\}\}").unwrap());
static UI_TEMPLATE: &str = include_str!("../templates/ui.html");

fn apply_template(input: &str, env_map: &HashMap<String, String>) -> String {
    TEMPLATE_RE
        .replace_all(input, |caps: &Captures| {
            let key = &caps[1];
            env_map
                .get(key)
                .cloned()
                .unwrap_or_else(|| caps[0].to_string())
        })
        .into_owned()
}

/// Načtení configu z YAML souboru
fn load_config(path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let cfg: Config = serde_yaml_ng::from_str(&content)?;
    Ok(cfg)
}

/// Spustí git clone nebo fetch+reset
async fn sync_git_repo(git: &GitConfig) -> Result<(), Box<dyn std::error::Error>> {
    let git_dir = git.workdir.join(".git");

    if git_dir.exists() {
        // fetch + reset
        info!(
            "[git] Fetching & resetting repo in {} (branch {})",
            git.workdir.display(),
            git.branch
        );

        // FETCH
        let fetch_out = Command::new("git")
            .arg("-C")
            .arg(&git.workdir)
            .arg("fetch")
            .arg("--all")
            .output()
            .await?;
        if !fetch_out.status.success() {
            let stderr = String::from_utf8_lossy(&fetch_out.stderr);
            error!("git fetch failed: {}", stderr.trim());
            return Err(format!("git fetch failed: {}", stderr.trim()).into());
        }

        // RESET
        let reset_target = format!("origin/{}", git.branch);
        let reset_out = Command::new("git")
            .arg("-C")
            .arg(&git.workdir)
            .arg("reset")
            .arg("--hard")
            .arg(&reset_target)
            .output()
            .await?;
        if !reset_out.status.success() {
            let stderr = String::from_utf8_lossy(&reset_out.stderr);
            error!(
                "git reset --hard {} failed: {}",
                reset_target,
                stderr.trim()
            );
            return Err(format!(
                "git reset --hard {} failed: {}",
                reset_target,
                stderr.trim()
            )
            .into());
        }
    } else {
        // clone
        if let Some(parent) = git.workdir.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        info!(
            "[git] Cloning {} (branch {}) into {}",
            git.repo_url,
            git.branch,
            git.workdir.display()
        );
        let clone_out = Command::new("git")
            .arg("clone")
            .arg("--branch")
            .arg(&git.branch)
            .arg(&git.repo_url)
            .arg(&git.workdir)
            .output()
            .await?;
        if !clone_out.status.success() {
            let stderr = String::from_utf8_lossy(&clone_out.stderr);
            error!("git clone failed: {}", stderr.trim());
            return Err(format!("git clone failed: {}", stderr.trim()).into());
        }
    }

    Ok(())
}

/// Background loop pro pravidelný sync GITu
async fn git_sync_loop(git: GitConfig) {
    let min_interval = 5;
    let interval_secs = git.refresh_interval_secs.max(min_interval);
    let mut interval = time::interval(time::Duration::from_secs(interval_secs));

    loop {
        interval.tick().await;
        if let Err(e) = sync_git_repo(&git).await {
            error!("[git] periodic sync error: {e:?}");
        }
    }
}

/// Basic Auth check
fn check_basic_auth(headers: &HeaderMap, auth: &AuthConfig) -> Result<(), StatusCode> {
    if !auth.required {
        return Ok(());
    }

    let header = headers.get(AUTHORIZATION).ok_or(StatusCode::UNAUTHORIZED)?;
    let header_str = header.to_str().map_err(|_| StatusCode::UNAUTHORIZED)?;

    if !header_str.starts_with("Basic ") {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let b64 = &header_str[6..];
    let decoded = BASE64_STANDARD
        .decode(b64)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let decoded_str = String::from_utf8(decoded).map_err(|_| StatusCode::UNAUTHORIZED)?;

    let parts: Vec<&str> = decoded_str.splitn(2, ':').collect();
    if parts.len() != 2 {
        return Err(StatusCode::UNAUTHORIZED);
    }

    if parts[0] == auth.username && parts[1] == auth.password {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn unauthorized_response() -> Response {
    let mut resp = Response::new("Unauthorized".into());
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    resp.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        r#"Basic realm="SecureConfigServer""#.parse().unwrap(),
    );
    resp
}

/// Zkontroluje, že relativní path neobsahuje ".."
fn validate_rel_path(rel: &str) -> Result<PathBuf, ServerError> {
    let p = Path::new(rel);
    let mut cleaned = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(c) => cleaned.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(ServerError::BadRequest(
                    "Path cannot contain ..".to_string(),
                ));
            }
            _ => {}
        }
    }
    Ok(cleaned)
}

/// Handler pro file endpoint: GET /file/{label}/{path...}
async fn file_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath((label, rel_path)): AxumPath<(String, String)>,
) -> Response {
    if let Err(_status) = check_basic_auth(&headers, &state.auth) {
        return unauthorized_response();
    }

    match handle_file_request(&state, &label, &rel_path).await {
        Ok(resp) => resp,
        Err(ServerError::NotFound) => {
            debug!(%label, path = %rel_path, "file not found");
            StatusCode::NOT_FOUND.into_response()
        }
        Err(ServerError::BadRequest(msg)) => {
            warn!(%label, path = %rel_path, "bad file request: {msg}");
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
        Err(e) => {
            error!("[file] error: {e:?}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn handle_file_request(
    state: &AppState,
    label: &str,
    rel_path: &str,
) -> Result<Response, ServerError> {
    let safe_rel = validate_rel_path(rel_path)?;
    let bytes_opt = read_file_from_git(&state.config.git, Some(label), &safe_rel).await?;
    let bytes = match bytes_opt {
        Some(b) => b,
        None => return Err(ServerError::NotFound),
    };

    // detekce binary vs text - stejná jako teď:
    let is_binary = bytes.iter().any(|b| *b == 0) || std::str::from_utf8(&bytes).is_err();

    if is_binary {
        let mime = MimeGuess::from_path(&safe_rel)
            .first_or_octet_stream()
            .to_string();
        let mut resp = Response::new(bytes.into());
        resp.headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, mime.parse().unwrap());
        Ok(resp)
    } else {
        let text = String::from_utf8(bytes)?;
        let templated = apply_template(&text, &state.env_map);

        let mime = MimeGuess::from_path(&safe_rel)
            .first()
            .unwrap_or(mime_guess::mime::TEXT_PLAIN)
            .to_string();

        let mut resp = Response::new(templated.into());
        resp.headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, mime.parse().unwrap());
        Ok(resp)
    }
}

/// Spring-like config response
#[derive(Serialize)]
struct SpringPropertySource {
    name: String,
    source: HashMap<String, String>,
}

#[derive(Serialize)]
struct SpringEnvResponse {
    name: String,
    profiles: Vec<String>,
    // #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    version: String,
    state: String,
    #[serde(rename = "propertySources")]
    property_sources: Vec<SpringPropertySource>,
}

/// Handler: GET /{application}/{profile}/{label}
async fn spring_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath((application, profile, label)): AxumPath<(String, String, String)>,
) -> Response {
    if let Err(_status) = check_basic_auth(&headers, &state.auth) {
        return unauthorized_response();
    }

    match handle_spring_request(&state, &application, &profile, Some(&label)).await {
        Ok(env) => Json(env).into_response(),
        Err(ServerError::BadRequest(msg)) => {
            warn!(%application, %profile, %label, "bad spring request: {msg}");
            (StatusCode::BAD_REQUEST, msg).into_response()
        }
        Err(e) => {
            error!("[spring] error: {e:?}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn spring_handler_no_label(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath((application, profile)): AxumPath<(String, String)>,
) -> Response {
    if let Err(_status) = check_basic_auth(&headers, &state.auth) {
        return unauthorized_response();
    }

    match handle_spring_request(&state, &application, &profile, None).await {
        Ok(env) => Json(env).into_response(),
        Err(ServerError::BadRequest(msg)) => (StatusCode::BAD_REQUEST, msg).into_response(),
        Err(e) => {
            error!("[spring] error: {e:?}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn git_commit_date_for_label(
    git: &GitConfig,
    label_opt: Option<&str>,
) -> Result<String, ServerError> {
    let refs = git_refs_for_label(git, label_opt);

    for r in refs {
        // %cI = committer date, ISO 8601
        let output = Command::new("git")
            .arg("-C")
            .arg(&git.workdir)
            .arg("show")
            .arg("-s")
            .arg("--format=%cI")
            .arg(&r)
            .output()
            .await?;

        if output.status.success() {
            let s = String::from_utf8(output.stdout)?;
            return Ok(s.trim().to_string());
        }
    }

    Ok(String::new())
}

async fn git_version_for_label(
    git: &GitConfig,
    label_opt: Option<&str>,
) -> Result<String, ServerError> {
    let refs = git_refs_for_label(git, label_opt);

    for r in refs {
        let output = Command::new("git")
            .arg("-C")
            .arg(&git.workdir)
            .arg("rev-parse")
            .arg(&r)
            .output()
            .await?;

        if output.status.success() {
            let s = String::from_utf8(output.stdout)?;
            let trimmed = s.trim().to_string(); // odřízneme newline
            return Ok(trimmed);
        }
    }

    // když nic nevyjde, nebudeme kvůli tomu failovat celý config
    Ok(String::new())
}

fn git_refs_for_label(git: &GitConfig, label_opt: Option<&str>) -> Vec<String> {
    match label_opt {
        Some(label) if !label.is_empty() => vec![
            label.to_string(),           // zkus "release"
            format!("origin/{}", label), // pak "origin/release"
        ],
        _ => vec![
            git.branch.clone(),               // default branch, např. "main"
            format!("origin/{}", git.branch), // nebo "origin/main"
        ],
    }
}

async fn read_file_from_git(
    git: &GitConfig,
    label_opt: Option<&str>,
    rel_path: &Path,
) -> Result<Option<Vec<u8>>, ServerError> {
    // subpath (search-paths) + rel_path
    let mut full_rel = PathBuf::new();
    if let Some(sub) = &git.subpath {
        full_rel.push(sub);
    }
    full_rel.push(rel_path);

    let rel_str = full_rel
        .to_str()
        .ok_or_else(|| ServerError::BadRequest("Non-UTF8 path".to_string()))?
        .replace('\\', "/"); // jistota na Windows

    let refs = git_refs_for_label(git, label_opt);

    for r in refs {
        let spec = format!("{}:{}", r, rel_str);
        let output = Command::new("git")
            .arg("-C")
            .arg(&git.workdir)
            .arg("show")
            .arg(&spec)
            .output()
            .await?;

        if output.status.success() {
            return Ok(Some(output.stdout));
        }
    }

    Ok(None) // soubor v daném labelu neexistuje
}

async fn handle_spring_request(
    state: &AppState,
    application: &str,
    profile_str: &str,
    label_opt: Option<&str>,
) -> Result<SpringEnvResponse, ServerError> {
    // profily mohou být "dev" nebo "dev,foo"
    let profiles: Vec<String> = profile_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let (props, found_any) = read_and_merge_yaml_files(
        &state.config.git,
        application,
        &profiles,
        label_opt,
        &state.env_map,
    )
    .await?;

    let property_sources = if found_any {
        vec![SpringPropertySource {
            name: format!(
                "git:{}/{}:{}",
                state.config.git.repo_url, application, profile_str
            ),
            source: props,
        }]
    } else {
        Vec::new() // <-- přesně jako Spring: propertySources: []
    };

    // nově: zjistíme hash commitu
    let version = git_version_for_label(&state.config.git, label_opt)
        .await
        .unwrap_or_default();

    Ok(SpringEnvResponse {
        name: application.to_string(),
        profiles,
        label: label_opt.map(|s| s.to_string()),
        version,
        state: "".to_string(),
        property_sources,
    })
}

/// Načte a slije YAML soubory podle spring-like konvence
async fn read_and_merge_yaml_files(
    git: &GitConfig,
    application: &str,
    profiles: &[String],
    label_opt: Option<&str>,
    env_map: &HashMap<String, String>,
) -> Result<(HashMap<String, String>, bool), ServerError> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    // 1) application.yml / application.yaml
    candidates.push(PathBuf::from("application.yml"));
    candidates.push(PathBuf::from("application.yaml"));
    // 2) <application>.yml / <application>.yaml
    candidates.push(PathBuf::from(format!("{application}.yml")));
    candidates.push(PathBuf::from(format!("{application}.yaml")));
    // 3) profile-specific
    for p in profiles {
        candidates.push(PathBuf::from(format!("application-{p}.yml")));
        candidates.push(PathBuf::from(format!("application-{p}.yaml")));
        candidates.push(PathBuf::from(format!("{application}-{p}.yml")));
        candidates.push(PathBuf::from(format!("{application}-{p}.yaml")));
    }

    let mut result: HashMap<String, String> = HashMap::new();
    let mut found_any = false;

    for rel in candidates {
        if let Some(bytes) = read_file_from_git(git, label_opt, &rel).await? {
            found_any = true;
            let content = String::from_utf8(bytes)?;
            let templated = apply_template(&content, env_map);
            let yaml: YamlValue = serde_yaml_ng::from_str(&templated)?;
            flatten_yaml_value(None, &yaml, &mut result);
        }
    }

    // ŽÁDNÝ Err(NotFound) - vracíme i případ „nic jsme nenašli“
    Ok((result, found_any))
}

/// Flatten YamlValue do "key -> string" stylu (spring.datasource.url ...).
fn flatten_yaml_value(prefix: Option<&str>, value: &YamlValue, out: &mut HashMap<String, String>) {
    match value {
        YamlValue::Mapping(map) => {
            for (k, v) in map {
                let key_str = match k {
                    YamlValue::String(s) => s.clone(),
                    _ => continue, // klíče musí být string
                };
                let new_prefix = match prefix {
                    Some(p) if !p.is_empty() => format!("{p}.{key_str}"),
                    _ => key_str,
                };
                flatten_yaml_value(Some(&new_prefix), v, out);
            }
        }
        YamlValue::Sequence(seq) => {
            for (idx, v) in seq.iter().enumerate() {
                let key = match prefix {
                    Some(p) if !p.is_empty() => format!("{p}[{idx}]"),
                    _ => format!("[{idx}]"),
                };
                flatten_yaml_value(Some(&key), v, out);
            }
        }
        YamlValue::Null => {
            // ignorujeme
        }
        YamlValue::Bool(b) => {
            if let Some(p) = prefix {
                out.insert(p.to_string(), b.to_string());
            }
        }
        YamlValue::Number(n) => {
            if let Some(p) = prefix {
                out.insert(p.to_string(), n.to_string());
            }
        }
        YamlValue::String(s) => {
            if let Some(p) = prefix {
                out.insert(p.to_string(), s.clone());
            }
        }
        _ => {
            // Tagged a případné další varianty prostě ignorujeme
        }
    }
}

/// Jednoduché UI na /ui
async fn ui_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(_status) = check_basic_auth(&headers, &state.auth) {
        return unauthorized_response();
    }

    let repo = &state.config.git.repo_url;
    let branch = &state.config.git.branch;
    let workdir = state.config.git.workdir.display().to_string();

    let subpath = state
        .config
        .git
        .subpath
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());

    let sha = git_version_for_label(&state.config.git, None)
        .await
        .unwrap_or_else(|e| {
            warn!(error = ?e, "cannot determine git version for UI");
            String::new()
        });

    let commit_date = git_commit_date_for_label(&state.config.git, None)
        .await
        .unwrap_or_else(|e| {
            warn!(error = ?e, "cannot determine git commit date for UI");
            String::new()
        });

    let html = UI_TEMPLATE
        .replace("__REPO_URL__", repo)
        .replace("__BRANCH__", branch)
        .replace("__WORKDIR__", &workdir)
        .replace("__SUBPATH__", &subpath)
        .replace("__SHA__", &sha)
        .replace("__COMMIT_DATE__", &commit_date);

    Html(html).into_response()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // tracing init
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(env_filter)
        .with_target(false) // (volitelné) neschová target modulu
        .compact() // kratší hezký formát
        .init();

    let cli = Cli::parse();

    info!("[main] Loading config from {}", cli.config.display());
    let config = load_config(&cli.config)?;

    // načti ENV mapu (už dešifrované hodnoty, ty dodáš ty)
    let env_map: HashMap<String, String> = std::env::vars().collect();
    let env_map = Arc::new(env_map);

    let auth = AuthConfig::from_env();

    // initial sync git
    sync_git_repo(&config.git).await?;

    // spawn background sync
    tokio::spawn(git_sync_loop(config.git.clone()));

    let state = Arc::new(AppState {
        config: config.clone(),
        env_map,
        auth,
    });

    // vnitřní router bez explicitního typu
    let inner = Router::new()
        .route("/{application}/{profile}/{label}", get(spring_handler))
        .route("/{application}/{profile}", get(spring_handler_no_label))
        .route("/file/{label}/{*path}", get(file_handler))
        .route("/ui", get(ui_handler));

    // prefix-path
    let base_path = config.http.base_path.clone();
    let app = if base_path.is_empty() || base_path == "/" {
        inner
    } else {
        // musí začínat "/"
        let bp = if base_path.starts_with('/') {
            base_path
        } else {
            format!("/{}", base_path)
        };
        Router::new().nest(&bp, inner)
    };

    let app = app.with_state(Arc::clone(&state));

    let addr: SocketAddr = config.http.bind_addr.parse()?;
    info!("[main] Listening on http://{}", addr);

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
