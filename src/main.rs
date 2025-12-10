use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use axum::{
    Json, Router,
    extract::{OriginalUri, Path as AxumPath, State},
    http::{
        HeaderMap, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE},
    },
    response::{Html, IntoResponse, Response},
    routing::get,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::{SecondsFormat, Utc};
use clap::Parser;
use mime_guess::MimeGuess;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Number as JsonNumber, Value as JsonValue};
use serde_yaml_ng::Value as YamlValue;
use thiserror::Error;
use tokio::{
    net::TcpListener,
    process::Command,
    time::{Duration, sleep},
};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

/// ---------- CLI & configuration ----------

#[derive(Parser, Debug)]
#[command(
    name = "secure-config-server",
    version,
    about = "Secure, template-aware config server (Spring Cloud Config compatible)"
)]
struct Cli {
    /// Path to configuration file (YAML)
    #[arg(short, long, value_name = "FILE", default_value = "config.yaml")]
    config: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
struct GitConfig {
    repo_url: String,
    branch: String,
    workdir: PathBuf,
    #[serde(default)]
    subpath: Option<PathBuf>,
    #[serde(default = "default_refresh_interval")]
    refresh_interval_secs: u64,
}

fn default_refresh_interval() -> u64 {
    30
}

#[derive(Debug, Clone, Deserialize)]
struct HttpConfig {
    bind_addr: String,
    #[serde(default = "default_base_path")]
    base_path: String,
}

fn default_base_path() -> String {
    "/".to_string()
}

/// Root configuration supports:
/// - single instance: `git` + optional global env
/// - multi-tenant: `environments` + optional global env
#[derive(Debug, Clone, Deserialize)]
struct RootConfig {
    http: HttpConfig,

    /// Load process environment into template variables
    #[serde(default)]
    env_from_process: bool,

    /// Optional global env file (KEY=VALUE per line)
    #[serde(default)]
    env_file: Option<String>,

    /// Single-instance mode
    #[serde(default)]
    git: Option<GitConfig>,

    /// Multi-tenant mode
    #[serde(default)]
    environments: HashMap<String, EnvDefinition>,
}

#[derive(Debug, Clone, Deserialize)]
struct EnvDefinition {
    git: GitConfig,
    #[serde(default)]
    env_file: Option<String>,
}

#[derive(Debug, Clone)]
struct EnvState {
    name: String,
    git: GitConfig,
    env_map: Arc<HashMap<String, String>>,
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
                info!("[auth] Basic auth enabled");
                Self {
                    required: true,
                    username: u,
                    password: p,
                }
            }
            _ => {
                warn!("[auth] Basic auth disabled (env AUTH_USERNAME / AUTH_PASSWORD not set)");
                Self {
                    required: false,
                    username: String::new(),
                    password: String::new(),
                }
            }
        }
    }
}

struct AppState {
    http: HttpConfig,
    envs: HashMap<String, EnvState>,
    auth: AuthConfig,
}

/// ---------- Errors ----------

#[derive(Error, Debug)]
enum ServerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("Git error: {0}")]
    Git(String),
    #[error("Not found")]
    NotFound,
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Other error: {0}")]
    #[allow(dead_code)]
    Other(String),
}

/// ---------- Global template regex & UI template ----------

static TEMPLATE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\{\{\s*([A-Za-z_][A-Za-z0-9_]*)\s*\}\}"#).unwrap());

static UI_TEMPLATE: &str = include_str!("../templates/ui.html");

/// ---------- Main ----------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let cli = Cli::parse();
    info!("[main] Loading config from {}", cli.config.display());

    let root_cfg = load_root_config(&cli.config)?;

    // Build global env map
    let mut global_env: HashMap<String, String> = HashMap::new();

    if root_cfg.env_from_process {
        for (k, v) in std::env::vars() {
            global_env.insert(k, v);
        }
    }

    if let Some(ref env_file) = root_cfg.env_file {
        merge_env_file_into(env_file, &mut global_env);
    }

    // Build environments map
    let mut envs: HashMap<String, EnvState> = HashMap::new();

    if !root_cfg.environments.is_empty() {
        // Multi-tenant
        for (name, env_def) in &root_cfg.environments {
            let mut env_map = global_env.clone();
            if let Some(ref path) = env_def.env_file {
                merge_env_file_into(path, &mut env_map);
            }

            envs.insert(
                name.clone(),
                EnvState {
                    name: name.clone(),
                    git: env_def.git.clone(),
                    env_map: Arc::new(env_map),
                },
            );
        }
    } else if let Some(ref git) = root_cfg.git {
        // Single-instance, exposed as logical env "default"
        envs.insert(
            "default".to_string(),
            EnvState {
                name: "default".to_string(),
                git: git.clone(),
                env_map: Arc::new(global_env.clone()),
            },
        );
    } else {
        return Err("config.yaml must contain either `git` or `environments`".into());
    }

    let auth = AuthConfig::from_env();

    // Initial sync for all envs
    for env in envs.values() {
        sync_git_repo(&env.git).await?;
    }

    // Background refresh loops
    for env in envs.values() {
        let git = env.git.clone();
        tokio::spawn(async move {
            git_sync_loop(git).await;
        });
    }

    let state = Arc::new(AppState {
        http: root_cfg.http.clone(),
        envs,
        auth,
    });

    let app = build_router(state.clone());

    let addr: SocketAddr = state.http.bind_addr.parse()?;
    info!("[main] Listening on http://{}", addr);

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_level(true)
        .try_init();
}

/// ---------- Config helpers ----------

fn load_root_config(path: &Path) -> Result<RootConfig, ServerError> {
    let contents = std::fs::read_to_string(path)?;
    let cfg: RootConfig = serde_yaml_ng::from_str(&contents)?;
    Ok(cfg)
}

fn merge_env_file_into(path: &str, target: &mut HashMap<String, String>) {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    target.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
        Err(e) => {
            warn!("[env] Failed to read env_file {}: {}", path, e);
        }
    }
}

fn normalize_base_path(base: &str) -> String {
    if base.is_empty() || base == "/" {
        "/".to_string()
    } else {
        let trimmed = base.trim().trim_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", trimmed)
        }
    }
}

/// ---------- Git helpers ----------

async fn sync_git_repo(git: &GitConfig) -> Result<(), ServerError> {
    std::fs::create_dir_all(&git.workdir)?;
    let git_dir = git.workdir.join(".git");

    if !git_dir.exists() {
        info!(
            "[git] Cloning {} into {} (branch {})",
            git.repo_url,
            git.workdir.display(),
            git.branch
        );
        let output = Command::new("git")
            .arg("clone")
            .arg("--branch")
            .arg(&git.branch)
            .arg("--single-branch")
            .arg(&git.repo_url)
            .arg(&git.workdir)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ServerError::Git(format!(
                "git clone failed: {}",
                stderr.trim()
            )));
        }
    } else {
        info!(
            "[git] Fetching & resetting repo in {} (branch {})",
            git.workdir.display(),
            git.branch
        );

        let fetch_out = Command::new("git")
            .arg("-C")
            .arg(&git.workdir)
            .arg("fetch")
            .arg("--all")
            .arg("--prune")
            .output()
            .await?;

        if !fetch_out.status.success() {
            let stderr = String::from_utf8_lossy(&fetch_out.stderr);
            return Err(ServerError::Git(format!(
                "git fetch failed: {}",
                stderr.trim()
            )));
        }

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
            return Err(ServerError::Git(format!(
                "git reset --hard {} failed: {}",
                reset_target,
                stderr.trim()
            )));
        }
    }

    Ok(())
}

async fn git_sync_loop(git: GitConfig) {
    let interval = if git.refresh_interval_secs == 0 {
        30
    } else {
        git.refresh_interval_secs
    };

    loop {
        sleep(Duration::from_secs(interval)).await;
        if let Err(e) = sync_git_repo(&git).await {
            warn!(
                "[git] Periodic refresh failed for {}: {:?}",
                git.workdir.display(),
                e
            );
        }
    }
}

async fn git_version_for_label(
    git: &GitConfig,
    label: Option<&str>,
) -> Result<String, ServerError> {
    let rev = label.unwrap_or(&git.branch);
    let output = Command::new("git")
        .arg("-C")
        .arg(&git.workdir)
        .arg("rev-parse")
        .arg(rev)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServerError::Git(format!(
            "git rev-parse {} failed: {}",
            rev,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout.trim().to_string())
}

async fn git_commit_date_for_label(
    git: &GitConfig,
    label: Option<&str>,
) -> Result<String, ServerError> {
    let rev = label.unwrap_or(&git.branch);
    let output = Command::new("git")
        .arg("-C")
        .arg(&git.workdir)
        .arg("show")
        .arg("-s")
        .arg("--format=%cI")
        .arg(rev)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServerError::Git(format!(
            "git show {} failed: {}",
            rev,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout.trim().to_string())
}

async fn read_file_from_git(
    git: &GitConfig,
    label_opt: Option<&str>,
    rel_path: &Path,
) -> Result<Option<Vec<u8>>, ServerError> {
    let mut full_rel = PathBuf::new();
    if let Some(sub) = &git.subpath {
        full_rel.push(sub);
    }
    full_rel.push(rel_path);

    let rel_str = full_rel
        .to_str()
        .ok_or_else(|| ServerError::BadRequest("Non-UTF8 path".to_string()))?
        .replace('\\', "/");

    let rev = label_opt.unwrap_or(&git.branch);
    let spec = format!("{}:{}", rev, rel_str);

    let output = Command::new("git")
        .arg("-C")
        .arg(&git.workdir)
        .arg("show")
        .arg(&spec)
        .output()
        .await?;

    if output.status.success() {
        Ok(Some(output.stdout))
    } else {
        Ok(None)
    }
}

async fn list_files_in_git(git: &GitConfig) -> Result<Vec<String>, ServerError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(&git.workdir)
        .arg("ls-tree")
        .arg("-r")
        .arg("--name-only")
        .arg(&git.branch)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServerError::Git(format!(
            "git ls-tree failed: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8(output.stdout)?;
    let mut files = Vec::new();

    let sub = git
        .subpath
        .as_ref()
        .map(|p| p.to_string_lossy().replace('\\', "/"));

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut rel = line.to_string();
        if let Some(ref subpath) = sub {
            if let Some(stripped) = rel.strip_prefix(&(subpath.clone() + "/")) {
                rel = stripped.to_string();
            } else if rel == *subpath {
                continue;
            } else {
                continue;
            }
        }
        files.push(rel);
    }

    Ok(files)
}

/// ---------- Template & YAML helpers ----------

fn apply_template(input: &str, env: &HashMap<String, String>) -> String {
    TEMPLATE_RE
        .replace_all(input, |caps: &regex::Captures| {
            let key = &caps[1];
            env.get(key).cloned().unwrap_or_else(|| caps[0].to_string())
        })
        .into_owned()
}

fn flatten_yaml_value(
    prefix: Option<&str>,
    value: &YamlValue,
    out: &mut HashMap<String, JsonValue>,
) {
    match value {
        YamlValue::Null => {
            if let Some(key) = prefix {
                out.insert(key.to_string(), JsonValue::Null);
            }
        }
        YamlValue::Bool(b) => {
            if let Some(key) = prefix {
                out.insert(key.to_string(), JsonValue::Bool(*b));
            }
        }
        YamlValue::Number(n) => {
            if let Some(key) = prefix {
                let json_num = if let Some(i) = n.as_i64() {
                    JsonNumber::from(i)
                } else if let Some(u) = n.as_u64() {
                    JsonNumber::from(u)
                } else if let Some(f) = n.as_f64() {
                    JsonNumber::from_f64(f).unwrap_or_else(|| JsonNumber::from(0))
                } else {
                    JsonNumber::from(0)
                };
                out.insert(key.to_string(), JsonValue::Number(json_num));
            }
        }
        YamlValue::String(s) => {
            if let Some(key) = prefix {
                out.insert(key.to_string(), JsonValue::String(s.clone()));
            }
        }
        YamlValue::Sequence(seq) => {
            for (idx, v) in seq.iter().enumerate() {
                let new_prefix = match prefix {
                    Some(p) => format!("{}[{}]", p, idx),
                    None => format!("[{}]", idx),
                };
                flatten_yaml_value(Some(&new_prefix), v, out);
            }
        }
        YamlValue::Mapping(map) => {
            for (k, v) in map {
                let key_str = match k {
                    YamlValue::String(s) => s.clone(),
                    YamlValue::Number(n) => n.to_string(),
                    YamlValue::Bool(b) => b.to_string(),
                    other => format!("{:?}", other),
                };
                let new_prefix = match prefix {
                    Some(p) => format!("{}.{}", p, key_str),
                    None => key_str,
                };
                flatten_yaml_value(Some(&new_prefix), v, out);
            }
        }
        YamlValue::Tagged(inner) => {
            flatten_yaml_value(prefix, &inner.value, out);
        }
    }
}

async fn read_and_merge_yaml_files(
    git: &GitConfig,
    application: &str,
    profiles: &[String],
    label_opt: Option<&str>,
    env_map: &HashMap<String, String>,
) -> Result<(HashMap<String, JsonValue>, bool), ServerError> {
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

    let mut result: HashMap<String, JsonValue> = HashMap::new();
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

    Ok((result, found_any))
}

fn parse_profiles(profile_str: &str) -> Vec<String> {
    profile_str
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn validate_rel_path(raw: &str) -> Result<PathBuf, ServerError> {
    let path = Path::new(raw);
    let mut clean = PathBuf::new();

    for comp in path.components() {
        match comp {
            Component::Normal(seg) => clean.push(seg),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(ServerError::BadRequest(
                    "Parent '..' segments are not allowed".to_string(),
                ));
            }
            _ => {
                return Err(ServerError::BadRequest(
                    "Absolute or root-relative paths are not allowed".to_string(),
                ));
            }
        }
    }

    Ok(clean)
}

/// ---------- Spring-compatible response types ----------

#[derive(Serialize)]
struct SpringPropertySource {
    name: String,
    source: HashMap<String, JsonValue>,
}

#[derive(Serialize)]
struct SpringEnvResponse {
    name: String,
    profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    version: String,
    state: String,
    #[serde(rename = "propertySources")]
    property_sources: Vec<SpringPropertySource>,
}

async fn handle_spring_request(
    env_state: &EnvState,
    application: &str,
    profile_str: &str,
    label_opt: Option<&str>,
) -> Result<SpringEnvResponse, ServerError> {
    let profiles = parse_profiles(profile_str);
    let (props, found_any) = read_and_merge_yaml_files(
        &env_state.git,
        application,
        &profiles,
        label_opt,
        &env_state.env_map,
    )
    .await?;

    let version = match git_version_for_label(&env_state.git, label_opt).await {
        Ok(v) => v,
        Err(e) => {
            warn!("[spring] git version lookup failed: {:?}", e);
            String::new()
        }
    };

    let property_sources = if found_any {
        let ps_name = format!(
            "git:{}{}:{}",
            env_state.git.repo_url,
            env_state
                .git
                .subpath
                .as_ref()
                .map(|p| format!("/{}", p.display()))
                .unwrap_or_default(),
            profile_str
        );
        vec![SpringPropertySource {
            name: ps_name,
            source: props,
        }]
    } else {
        Vec::new()
    };

    Ok(SpringEnvResponse {
        name: application.to_string(),
        profiles,
        label: label_opt.map(|s| s.to_string()),
        version,
        state: "".to_string(),
        property_sources,
    })
}

/// ---------- HTTP helpers ----------

fn check_basic_auth(state: &AppState, headers: &HeaderMap) -> bool {
    if !state.auth.required {
        return true;
    }

    let value = match headers.get(AUTHORIZATION) {
        Some(v) => v,
        None => return false,
    };

    let value_str = match value.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };

    if !value_str.starts_with("Basic ") {
        return false;
    }

    let b64 = &value_str[6..];
    let decoded = match BASE64_STANDARD.decode(b64) {
        Ok(d) => d,
        Err(_) => return false,
    };

    let creds = String::from_utf8_lossy(&decoded);
    let mut parts = creds.splitn(2, ':');
    let user = parts.next().unwrap_or("");
    let pass = parts.next().unwrap_or("");

    user == state.auth.username && pass == state.auth.password
}

fn unauthorized_response() -> Response {
    let mut resp = Response::new("Unauthorized".into());
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    resp.headers_mut().insert(
        WWW_AUTHENTICATE,
        r#"Basic realm="SecureConfigServer""#.parse().unwrap(),
    );
    resp
}

fn spring_not_found_json(path: &str) -> Response {
    let body = serde_json::json!({
        "timestamp": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "status": 404,
        "error": "Not Found",
        "path": path,
    });
    (StatusCode::NOT_FOUND, Json(body)).into_response()
}

async fn spring_like_404(OriginalUri(uri): OriginalUri) -> Response {
    spring_not_found_json(uri.path())
}

/// ---------- HTTP handlers ----------

async fn spring_handler(
    State(state): State<Arc<AppState>>,
    AxumPath((env, application, profile, label)): AxumPath<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if !check_basic_auth(&state, &headers) {
        return unauthorized_response();
    }

    let env_state = match state.envs.get(&env) {
        Some(e) => e,
        None => {
            let path = format!("/{}/{}/{}/{}", env, application, profile, label);
            return spring_not_found_json(&path);
        }
    };

    match handle_spring_request(env_state, &application, &profile, Some(&label)).await {
        Ok(body) => Json(body).into_response(),
        Err(e) => {
            error!("[spring] error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

async fn spring_handler_no_label(
    State(state): State<Arc<AppState>>,
    AxumPath((env, application, profile)): AxumPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if !check_basic_auth(&state, &headers) {
        return unauthorized_response();
    }

    let env_state = match state.envs.get(&env) {
        Some(e) => e,
        None => {
            let path = format!("/{}/{}/{}", env, application, profile);
            return spring_not_found_json(&path);
        }
    };

    match handle_spring_request(env_state, &application, &profile, None).await {
        Ok(body) => Json(body).into_response(),
        Err(e) => {
            error!("[spring] error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

fn shell_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
}

async fn env_json_handler(
    State(state): State<Arc<AppState>>,
    AxumPath(env): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if !check_basic_auth(&state, &headers) {
        return unauthorized_response();
    }

    let env_state = match state.envs.get(&env) {
        Some(e) => e,
        None => {
            let path = format!("/{}/env", env);
            return spring_not_found_json(&path);
        }
    };

    Json(&*env_state.env_map).into_response()
}

async fn env_export_handler(
    State(state): State<Arc<AppState>>,
    AxumPath(env): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if !check_basic_auth(&state, &headers) {
        return unauthorized_response();
    }

    let env_state = match state.envs.get(&env) {
        Some(e) => e,
        None => {
            let path = format!("/{}/env/export", env);
            return spring_not_found_json(&path);
        }
    };

    let mut body = String::new();
    for (k, v) in env_state.env_map.iter() {
        body.push_str("export ");
        body.push_str(k);
        body.push_str("=\"");
        body.push_str(&shell_escape(v));
        body.push_str("\"\n");
    }

    let mut resp = Response::new(body.into());
    resp.headers_mut()
        .insert(CONTENT_TYPE, "text/plain; charset=utf-8".parse().unwrap());
    resp
}

async fn env_files_handler(
    State(state): State<Arc<AppState>>,
    AxumPath(env): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if !check_basic_auth(&state, &headers) {
        return unauthorized_response();
    }

    let env_state = match state.envs.get(&env) {
        Some(e) => e,
        None => {
            let path = format!("/{}/files", env);
            return spring_not_found_json(&path);
        }
    };

    match list_files_in_git(&env_state.git).await {
        Ok(files) => Json(serde_json::json!({ "files": files })).into_response(),
        Err(e) => {
            error!("[files] error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

async fn file_handler(
    State(state): State<Arc<AppState>>,
    AxumPath((env, label, rel_path)): AxumPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if !check_basic_auth(&state, &headers) {
        return unauthorized_response();
    }

    let env_state = match state.envs.get(&env) {
        Some(e) => e,
        None => {
            let path = format!("/{}/file/{}/{}", env, label, rel_path);
            return spring_not_found_json(&path);
        }
    };

    match handle_file_request(env_state, &label, &rel_path).await {
        Ok(resp) => resp,
        Err(ServerError::NotFound) => {
            let path = format!("/{}/file/{}/{}", env, label, rel_path);
            spring_not_found_json(&path)
        }
        Err(e) => {
            error!("[file] error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

async fn handle_file_request(
    env_state: &EnvState,
    label: &str,
    rel_path: &str,
) -> Result<Response, ServerError> {
    let safe_rel = validate_rel_path(rel_path)?;
    let bytes_opt = read_file_from_git(&env_state.git, Some(label), &safe_rel).await?;
    let bytes = match bytes_opt {
        Some(b) => b,
        None => return Err(ServerError::NotFound),
    };

    let is_binary = bytes.iter().any(|b| *b == 0) || std::str::from_utf8(&bytes).is_err();

    if is_binary {
        let mime = MimeGuess::from_path(&safe_rel)
            .first_or_octet_stream()
            .to_string();
        let mut resp = Response::new(bytes.into());
        resp.headers_mut().insert(
            CONTENT_TYPE,
            mime.parse()
                .unwrap_or_else(|_| "application/octet-stream".parse().unwrap()),
        );
        Ok(resp)
    } else {
        let text = String::from_utf8(bytes)?;
        let templated = apply_template(&text, &env_state.env_map);
        let mime = MimeGuess::from_path(&safe_rel)
            .first_or_octet_stream()
            .to_string();
        let mut resp = Response::new(templated.into());
        resp.headers_mut().insert(
            CONTENT_TYPE,
            mime.parse()
                .unwrap_or_else(|_| "text/plain; charset=utf-8".parse().unwrap()),
        );
        Ok(resp)
    }
}

/// ---------- UI handler & router ----------

async fn ui_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !check_basic_auth(&state, &headers) {
        return unauthorized_response();
    }

    #[derive(Serialize)]
    struct EnvMeta {
        name: String,
        repo_url: String,
        branch: String,
        workdir: String,
        subpath: String,
        last_commit: String,
        last_commit_date: String,
    }

    #[derive(Serialize)]
    struct UiMeta {
        base_path: String,
        environments: Vec<EnvMeta>,
        auth_enabled: bool,
    }

    let mut envs_meta = Vec::new();
    for env_state in state.envs.values() {
        let last_commit = match git_version_for_label(&env_state.git, None).await {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "[ui] failed to get git version for {}: {:?}",
                    env_state.name, e
                );
                String::new()
            }
        };
        let last_commit_date = match git_commit_date_for_label(&env_state.git, None).await {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "[ui] failed to get git date for {}: {:?}",
                    env_state.name, e
                );
                String::new()
            }
        };

        envs_meta.push(EnvMeta {
            name: env_state.name.clone(),
            repo_url: env_state.git.repo_url.clone(),
            branch: env_state.git.branch.clone(),
            workdir: env_state.git.workdir.display().to_string(),
            subpath: env_state
                .git
                .subpath
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            last_commit,
            last_commit_date,
        });
    }

    let meta = UiMeta {
        base_path: normalize_base_path(&state.http.base_path),
        environments: envs_meta,
        auth_enabled: state.auth.required,
    };

    let meta_json = match serde_json::to_string(&meta) {
        Ok(s) => s,
        Err(e) => {
            error!("[ui] failed to serialize meta: {:?}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
    };

    let html = UI_TEMPLATE.replace("__META_JSON__", &meta_json);
    Html(html).into_response()
}

fn build_router(state: Arc<AppState>) -> Router {
    let base_path = normalize_base_path(&state.http.base_path);

    let inner = Router::new()
        // Spring-compatible: /{env}/{application}/{profile}/{label}
        .route(
            "/{env}/{application}/{profile}/{label}",
            get(spring_handler),
        )
        // Spring-compatible: /{env}/{application}/{profile}
        .route(
            "/{env}/{application}/{profile}",
            get(spring_handler_no_label),
        )
        // Raw file access with templating: /{env}/file/{label}/{*path}
        .route("/{env}/file/{label}/{*path}", get(file_handler))
        // Env helpers
        .route("/{env}/env", get(env_json_handler))
        .route("/{env}/env/export", get(env_export_handler))
        .route("/{env}/files", get(env_files_handler))
        // UI
        .route("/ui", get(ui_handler));

    let app = if base_path == "/" {
        inner
    } else {
        Router::new().nest(&base_path, inner)
    };

    app.with_state(state).fallback(spring_like_404)
}
