use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Output,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use futures::lock::Mutex as AsyncMutex;
use globset::Glob;
use gpui::App;
use http_client::{AsyncBody, HttpClient, Method, Request};
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelId, LanguageModelProviderId,
    LanguageModelRegistry, LanguageModelRequest, LanguageModelRequestMessage, MessageContent, Role,
};
use parking_lot::Mutex as ParkingMutex;
use rand::Rng;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings, SettingsContent};
use settings_content::{LanguageModelSelection, ZessionRepoAccessContent};
use util::command::new_command;

const SESSION_RULES: &str = "You are working inside a Zession workspace. The session starts empty and grows as repositories are attached.\n\
Use session_search_repos to find approved repositories and session_read_repo to inspect them without attaching. Attach a repository only before editing, building, or testing it.\n\
Use the native Session tools instead of the Session CLI. Work only on the session branch in attached repositories.\n";
const OUTPUT_LIMIT: usize = 16 * 1024;
const INDEX_FILE_LIMIT: usize = 8 * 1024;
const INDEX_CONTEXT_LIMIT: usize = 80 * 1024;
const INDEX_PATH_LIMIT: usize = 2000;
const INDEX_FILE_LIST_HEADER: &str = "## File list\n";
const INDEX_FILE_CONTENTS_HEADER: &str = "\n\n## Selected file contents\n";
const READ_ONLY_PUSH_URL: &str = "DISABLED-by-session-read-only-policy";
const INDEX_PROMPT: &str = "You are indexing a git repository for Zession, a tool that helps people find the right repositories for a task.\n\
Repository: {repo} ({remote}), checked out read-only at commit {commit}.\n\n\
A specialized discovery tool will search this summary later and quote only relevant parts, so be thorough and specific: concrete names, paths, and identifiers are more useful than general descriptions. Explore the supplied files well. Do not modify anything.\n\n\
Write a detailed Markdown summary (roughly 600-1500 words, depending on the repo's size) with exactly these sections:\n\n\
## Purpose\nWhat this repo is for and who uses it, in 1-4 sentences.\n\n\
## Stack\nLanguages, frameworks, major libraries, and build, test, and lint tools, with versions where pinned.\n\n\
## Layout\nThe important directories and files, and what lives in each. Go one or two levels deep for the main areas.\n\n\
## Key files and entry points\nMain entry points, central modules, and the files someone would most likely edit for common changes, with paths.\n\n\
## Manages / deploys\nServices, infrastructure, or resources this repo defines or deploys, with paths. Write \"Nothing found.\" if there are none.\n\n\
## Configuration\nConfig files, environment variables, secrets they refer to (names only), feature flags, and per-environment settings, with paths.\n\n\
## Interfaces and dependencies\nAPIs, endpoints, CLIs, packages, or artifacts this repo exposes, and the other repos, services, and external systems it depends on or talks to.\n\n\
## Development\nHow to build, run, and test it locally (commands), and any conventions in CONTRIBUTING, CLAUDE, or AGENTS files.\n\n\
## Keywords\nA comma-separated list of terms, including synonyms and product, service, and resource names, that someone might search for when they need this repo.\n\n\
Only state what you saw in the files. Output only the Markdown summary, with no preamble.\n\n\
";

#[derive(Clone, Debug, RegisterSetting)]
pub struct ZessionSettings {
    pub repos_allow: Vec<String>,
    pub repos_access: BTreeMap<String, Access>,
    pub auto_index: bool,
    pub store_max_age_minutes: u64,
    pub index_model: Option<LanguageModelSelection>,
}

impl Settings for ZessionSettings {
    fn from_settings(settings: &SettingsContent) -> Self {
        let content = settings.zession.clone().unwrap_or_default();
        let repos = content.repos.unwrap_or_default();
        Self {
            repos_allow: repos.allow.unwrap_or_default(),
            repos_access: repos
                .access
                .unwrap_or_default()
                .into_iter()
                .map(|(pattern, access)| {
                    let access = match access {
                        ZessionRepoAccessContent::Read => Access::Read,
                        ZessionRepoAccessContent::Write => Access::Write,
                    };
                    (pattern, access)
                })
                .collect(),
            auto_index: content.auto_index.unwrap_or(true),
            store_max_age_minutes: content.store_max_age_minutes.unwrap_or(10),
            index_model: content.index_model,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Access {
    Read,
    #[default]
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TitleSource {
    Auto,
    User,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionManifest {
    pub version: u32,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_source: Option<TitleSource>,
    pub created: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_opened: Option<DateTime<Utc>>,
    #[serde(default)]
    pub repos: Vec<SessionRepo>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRepo {
    pub name: String,
    pub repo_id: String,
    pub remote: String,
    pub cache: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base_ref: String,
    pub base_commit: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub dir: PathBuf,
    pub manifest: SessionManifest,
}

impl Session {
    pub fn display_title(&self) -> &str {
        self.manifest
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&self.manifest.id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoSpec {
    pub repo_id: String,
    pub host: String,
    pub owner: String,
    pub name: String,
    pub remote: String,
    pub cache: PathBuf,
    pub local: bool,
}

impl RepoSpec {
    pub fn parse(value: &str) -> Result<Self> {
        Self::parse_at(value, &zession_home())
    }

    pub fn parse_at(value: &str, home: &Path) -> Result<Self> {
        let value = value.trim();
        ensure!(!value.is_empty(), "repository value cannot be empty");
        let expanded = expand_home(Path::new(value));
        if expanded.is_dir() {
            ensure!(
                is_git_directory(&expanded),
                "local path {} is not a Git repository",
                expanded.display()
            );
            let absolute = expanded
                .canonicalize()
                .with_context(|| format!("resolve local repository {}", expanded.display()))?;
            let name = absolute
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.strip_suffix(".git").unwrap_or(name).to_owned())
                .filter(|name| valid_cache_part(name))
                .ok_or_else(|| anyhow!("local repository path has an invalid name"))?;
            return Ok(Self {
                repo_id: format!("local/{name}"),
                host: "local".to_string(),
                owner: "local".to_string(),
                name: name.clone(),
                remote: absolute.to_string_lossy().into_owned(),
                cache: home.join("store/local").join(format!("{name}.git")),
                local: true,
            });
        }

        let (remote, host, owner, name) = parse_remote_or_shorthand(value)?;
        Ok(Self {
            repo_id: format!("{owner}/{name}"),
            cache: home
                .join("store")
                .join(&host)
                .join(&owner)
                .join(format!("{name}.git")),
            host,
            owner,
            name,
            remote,
            local: false,
        })
    }
}

pub fn zession_home() -> PathBuf {
    env::var_os("ZESSION_HOME")
        .map(PathBuf::from)
        .map(|path| expand_home(&path))
        .unwrap_or_else(|| paths::home_dir().join(".zession"))
}

pub fn workspaces_dir() -> PathBuf {
    zession_home().join("workspaces")
}

pub fn sessions_dir() -> PathBuf {
    zession_home().join("sessions")
}

pub fn store_dir() -> PathBuf {
    zession_home().join("store")
}

pub fn index_dir() -> PathBuf {
    zession_home().join("index")
}

pub fn logs_dir() -> PathBuf {
    zession_home().join("logs")
}

pub fn init(cx: &mut App) {
    ZessionSettings::register(cx);
}

pub fn is_allowed(settings: &ZessionSettings, repo_id: &str) -> Result<bool> {
    for entry in &settings.repos_allow {
        let pattern = normalize_approval_entry(entry)?;
        if Glob::new(&pattern)
            .with_context(|| format!("invalid repository approval pattern {entry:?}"))?
            .compile_matcher()
            .is_match(repo_id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn access_for(settings: &ZessionSettings, repo_id: &str) -> Result<Access> {
    let mut patterns = settings.repos_access.iter().collect::<Vec<_>>();
    patterns.sort_by(|(left, _), (right, _)| {
        let left_wildcards = left.matches(['*', '?', '[']).count();
        let right_wildcards = right.matches(['*', '?', '[']).count();
        left_wildcards
            .cmp(&right_wildcards)
            .then_with(|| right.len().cmp(&left.len()))
            .then_with(|| left.cmp(right))
    });
    for (pattern, access) in patterns {
        let matcher = Glob::new(pattern)
            .with_context(|| format!("invalid repository access pattern {pattern:?}"))?
            .compile_matcher();
        if matcher.is_match(repo_id) {
            return Ok(*access);
        }
    }
    Ok(Access::Write)
}

pub fn create_session(name: Option<&str>) -> Result<Session> {
    create_session_at(&zession_home(), name)
}

pub fn create_session_at(home: &Path, name: Option<&str>) -> Result<Session> {
    let sessions_root = home.join("sessions");
    let workspaces_root = home.join("workspaces");
    fs::create_dir_all(&sessions_root)?;
    fs::create_dir_all(&workspaces_root)?;

    let id = match name {
        Some(name) => {
            let id = sanitize_id(name);
            ensure!(
                !id.is_empty(),
                "session name must contain a letter or number"
            );
            ensure!(
                !sessions_root.join(format!("{id}.json")).exists()
                    && !workspaces_root.join(&id).exists(),
                "session {id:?} already exists"
            );
            id
        }
        None => generated_id(&sessions_root, &workspaces_root)?,
    };
    let session_dir = workspaces_root.join(&id);
    fs::create_dir(&session_dir)
        .with_context(|| format!("create session workspace {}", session_dir.display()))?;
    let manifest = SessionManifest {
        version: 1,
        id,
        title: None,
        title_source: None,
        created: Utc::now(),
        last_opened: None,
        repos: Vec::new(),
    };
    let session = Session {
        dir: session_dir,
        manifest,
    };
    if let Err(error) = (|| -> Result<()> {
        write_rules(&session)?;
        write_manifest_at(home, &session.manifest)?;
        Ok(())
    })() {
        fs::remove_dir_all(&session.dir).with_context(|| {
            format!(
                "remove incomplete session workspace {}",
                session.dir.display()
            )
        })?;
        return Err(error);
    }
    Ok(session)
}

pub fn read_manifest(id: &str) -> Result<SessionManifest> {
    read_manifest_at(&zession_home(), id)
}

pub fn read_manifest_at(home: &Path, id: &str) -> Result<SessionManifest> {
    ensure!(valid_session_id(id), "invalid session id {id:?}");
    let path = home.join("sessions").join(format!("{id}.json"));
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("read Session manifest {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parse Session manifest {}", path.display()))
}

pub fn list_sessions() -> Result<Vec<Session>> {
    let root = sessions_dir();
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", root.display())),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) => {
                log::debug!(
                    "failed to read Session manifest {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        let manifest = match serde_json::from_str::<SessionManifest>(&contents) {
            Ok(manifest) => manifest,
            Err(error) => {
                log::debug!(
                    "failed to parse Session manifest {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        sessions.push(Session {
            dir: workspaces_dir().join(&manifest.id),
            manifest,
        });
    }
    sessions.sort_by(|left, right| {
        right
            .manifest
            .last_opened
            .as_ref()
            .unwrap_or(&right.manifest.created)
            .cmp(
                left.manifest
                    .last_opened
                    .as_ref()
                    .unwrap_or(&left.manifest.created),
            )
            .then_with(|| left.manifest.id.cmp(&right.manifest.id))
    });
    Ok(sessions)
}

pub fn set_title(id: &str, title: &str, source: TitleSource) -> Result<()> {
    let mut manifest = read_manifest(id)?;
    let title = title.trim();
    if title.is_empty() {
        manifest.title = None;
        manifest.title_source = None;
    } else {
        manifest.title = Some(title.to_owned());
        manifest.title_source = Some(source);
    }
    write_manifest(&manifest)
}

pub fn touch_last_opened(id: &str) -> Result<()> {
    let mut manifest = read_manifest(id)?;
    manifest.last_opened = Some(Utc::now());
    write_manifest(&manifest)
}

fn write_manifest(manifest: &SessionManifest) -> Result<()> {
    write_manifest_at(&zession_home(), manifest)
}

fn write_manifest_at(home: &Path, manifest: &SessionManifest) -> Result<()> {
    let path = home.join("sessions").join(format!("{}.json", manifest.id));
    let mut contents = serde_json::to_vec_pretty(manifest)?;
    contents.push(b'\n');
    atomic_write(&path, &contents)
}

fn write_rules(session: &Session) -> Result<()> {
    let mut contents = String::from(SESSION_RULES);
    if !session.manifest.repos.is_empty() {
        contents.push_str("\nAttached repositories:\n");
        for repo in &session.manifest.repos {
            contents.push_str(&format!(
                "- {}: {} (branch `{}`, base `{}`)\n",
                repo.name,
                repo.path.display(),
                repo.branch,
                repo.base_ref
            ));
        }
    }
    fs::write(session.dir.join(".rules"), contents)
        .with_context(|| format!("write Session rules in {}", session.dir.display()))
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut random = rand::rng();
    let temp_path = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("manifest"),
        random.random::<u64>()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .with_context(|| format!("create temporary file {}", temp_path.display()))?;
    let write_result = file
        .write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("write temporary file {}", temp_path.display()));
    if let Err(error) = write_result {
        let remove_result = fs::remove_file(&temp_path);
        if let Err(remove_error) = remove_result {
            log::warn!("failed to remove {}: {remove_error}", temp_path.display());
        }
        return Err(error);
    }
    fs::rename(&temp_path, path).with_context(|| format!("replace manifest {}", path.display()))?;
    Ok(())
}

fn generated_id(sessions_root: &Path, workspaces_root: &Path) -> Result<String> {
    const ADJECTIVES: &[&str] = &[
        "bright", "calm", "clever", "curious", "eager", "gentle", "happy", "quiet", "swift",
        "warm", "bold", "brisk", "cosmic", "daring", "dreamy", "friendly", "golden", "jolly",
        "lucky", "merry", "nimble", "peaceful", "playful", "proud", "shiny", "silent", "sunny",
        "tidy", "vivid", "wise",
    ];
    const NOUNS: &[&str] = &[
        "badger", "comet", "fern", "harbor", "maple", "otter", "panda", "sparrow", "willow",
        "wren", "acorn", "breeze", "cedar", "cloud", "daisy", "falcon", "grove", "heron", "island",
        "juniper", "kitten", "lantern", "meadow", "nebula", "orchid", "rabbit", "pebble", "river",
        "summit", "turtle",
    ];
    let mut random = rand::rng();
    for _ in 0..100 {
        let adjective = ADJECTIVES
            .get(random.random_range(0..ADJECTIVES.len()))
            .ok_or_else(|| anyhow!("no Session adjectives configured"))?;
        let noun = NOUNS
            .get(random.random_range(0..NOUNS.len()))
            .ok_or_else(|| anyhow!("no Session nouns configured"))?;
        let id = format!("{adjective}-{noun}");
        if !sessions_root.join(format!("{id}.json")).exists() && !workspaces_root.join(&id).exists()
        {
            return Ok(id);
        }
    }
    bail!("could not generate a unique Session id")
}

fn normalize_approval_entry(value: &str) -> Result<String> {
    let value = value.trim();
    ensure!(
        !value.is_empty(),
        "repository approval entry cannot be empty"
    );
    if value == "local" {
        return Ok("local/*".to_string());
    }
    if !value.contains('/') {
        ensure!(
            valid_cache_part(value),
            "invalid repository owner {value:?}"
        );
        return Ok(format!("{value}/*"));
    }
    let (owner, name) = value
        .split_once('/')
        .ok_or_else(|| anyhow!("approval entry must be an owner or owner/repository"))?;
    ensure!(
        !owner.is_empty() && !name.is_empty() && !name.contains('/'),
        "approval entry must be an owner or owner/repository"
    );
    ensure!(
        valid_cache_part(owner),
        "invalid repository owner {owner:?}"
    );
    if !name.contains(['*', '?', '[']) {
        ensure!(valid_cache_part(name), "invalid repository name {name:?}");
    } else {
        Glob::new(name).context("invalid repository glob")?;
    }
    Ok(value.to_string())
}

fn parse_remote_or_shorthand(value: &str) -> Result<(String, String, String, String)> {
    if !value.contains("://") && !value.starts_with("git@") {
        let parts = value.trim_matches('/').split('/').collect::<Vec<_>>();
        if let [owner, name] = parts.as_slice()
            && valid_cache_part(owner)
        {
            let name = name.strip_suffix(".git").unwrap_or(name);
            ensure!(valid_cache_part(name), "invalid repository name {name:?}");
            return Ok((
                format!("https://github.com/{owner}/{name}.git"),
                "github.com".to_string(),
                (*owner).to_string(),
                name.to_string(),
            ));
        }
    }

    let (remote, host, path) = if let Some(without_prefix) = value.strip_prefix("git@") {
        let (host, path) = without_prefix
            .split_once(':')
            .ok_or_else(|| anyhow!("cannot parse repository URL {value:?}"))?;
        (value.to_string(), host.to_string(), path.to_string())
    } else {
        let url =
            url::Url::parse(value).with_context(|| format!("invalid repository URL {value:?}"))?;
        ensure!(
            matches!(url.scheme(), "https" | "http" | "ssh" | "git"),
            "unsupported repository URL scheme {:?}",
            url.scheme()
        );
        ensure!(
            url.password().is_none() && (url.scheme() == "ssh" || url.username().is_empty()),
            "repository URLs must not contain credentials"
        );
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("repository URL has no host"))?
            .to_ascii_lowercase();
        (value.to_string(), host, url.path().to_string())
    };
    ensure!(valid_cache_part(&host), "invalid repository host {host:?}");
    let (owner, name) = split_remote_path(&path)?;
    Ok((remote, host, owner.to_string(), name.to_string()))
}

fn split_remote_path(path: &str) -> Result<(&str, &str)> {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let parts = path.split('/').collect::<Vec<_>>();
    ensure!(
        parts.len() >= 2,
        "repository URL must include owner and repository"
    );
    let owner = parts
        .get(parts.len() - 2)
        .ok_or_else(|| anyhow!("repository URL must include owner and repository"))?;
    let name = parts
        .last()
        .ok_or_else(|| anyhow!("repository URL must include owner and repository"))?;
    ensure!(
        valid_cache_part(owner) && valid_cache_part(name),
        "repository URL contains an invalid owner or repository"
    );
    Ok((*owner, *name))
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(path_string) = path.to_str() else {
        return path.to_path_buf();
    };
    if path_string == "~" {
        return paths::home_dir().to_path_buf();
    }
    path_string
        .strip_prefix("~/")
        .map(|suffix| paths::home_dir().join(suffix))
        .unwrap_or_else(|| path.to_path_buf())
}

fn is_git_directory(path: &Path) -> bool {
    path.join(".git").exists()
        || (path.join("HEAD").is_file()
            && path.join("objects").is_dir()
            && path.join("refs").is_dir())
}

fn sanitize_id(value: &str) -> String {
    let mut id = String::new();
    let mut last_dash = false;
    for character in value.to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            id.push(character);
            last_dash = false;
        } else if !last_dash && !id.is_empty() {
            id.push('-');
            last_dash = true;
        }
    }
    id.trim_end_matches('-').to_string()
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
        && !id.starts_with('-')
        && !id.ends_with('-')
}

fn valid_cache_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreRepo {
    pub repo_id: String,
    pub host: String,
    pub owner: String,
    pub name: String,
    pub remote: String,
    pub cache: PathBuf,
    pub local: bool,
}

impl From<&RepoSpec> for StoreRepo {
    fn from(spec: &RepoSpec) -> Self {
        Self {
            repo_id: spec.repo_id.clone(),
            host: spec.host.clone(),
            owner: spec.owner.clone(),
            name: spec.name.clone(),
            remote: spec.remote.clone(),
            cache: spec.cache.clone(),
            local: spec.local,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ZessionError {
    #[error("repository {repo_id} is not approved")]
    NotApproved { repo_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoOperation {
    ListFiles {
        git_ref: Option<String>,
        path: Option<String>,
    },
    Show {
        path: String,
        git_ref: Option<String>,
    },
    Grep {
        pattern: String,
        git_ref: Option<String>,
        path: Option<String>,
    },
    Log {
        git_ref: Option<String>,
        path: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusJson {
    pub id: String,
    pub title: String,
    pub path: PathBuf,
    pub repos: Vec<StatusRepo>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusRepo {
    pub name: String,
    pub repo_id: String,
    pub path: PathBuf,
    pub branch: String,
    pub base_ref: String,
    pub changed: usize,
}

static STORE_LOCKS: OnceLock<ParkingMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();
static INDEX_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();

pub async fn ensure_store_fresh(
    spec: &RepoSpec,
    max_age: Duration,
    force: bool,
) -> Result<StoreRepo> {
    let locks = STORE_LOCKS.get_or_init(|| ParkingMutex::new(HashMap::new()));
    let lock = {
        let mut locks = locks.lock();
        locks
            .entry(spec.cache.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    };
    let _guard = lock.lock().await;
    if !force && store_is_fresh(&spec.cache, max_age) {
        return Ok(StoreRepo::from(spec));
    }
    ensure_store_exists(spec).await?;
    let marker = spec.cache.join("zession-fetched-at");
    fs::write(&marker, Utc::now().to_rfc3339())
        .with_context(|| format!("write cache freshness marker {}", marker.display()))?;
    Ok(StoreRepo::from(spec))
}

async fn ensure_store_exists(spec: &RepoSpec) -> Result<()> {
    let cache = &spec.cache;
    if !cache.exists() {
        let parent = cache
            .parent()
            .ok_or_else(|| anyhow!("repository cache path has no parent"))?;
        fs::create_dir_all(parent)?;
        let output = run_git(
            None,
            [
                "clone".to_string(),
                "--bare".to_string(),
                spec.remote.clone(),
                cache.to_string_lossy().into_owned(),
            ],
        )
        .await?;
        ensure_git_success(output, "clone repository cache")?;
    } else {
        let output = run_git(Some(cache), ["rev-parse", "--is-bare-repository"]).await?;
        let is_bare_repository = String::from_utf8_lossy(&output.stdout).trim() == "true";
        ensure_git_success(output, "inspect repository cache")?;
        ensure!(
            is_bare_repository,
            "{} exists but is not a bare repository",
            cache.display()
        );
    }

    let origin = run_git(Some(cache), ["remote", "get-url", "origin"]).await?;
    if origin.status.success() {
        let origin_url = String::from_utf8_lossy(&origin.stdout);
        if spec.local || origin_url.trim() != spec.remote {
            let output =
                run_git(Some(cache), ["remote", "set-url", "origin", &spec.remote]).await?;
            ensure_git_success(output, "update repository cache origin")?;
        }
    } else {
        let output = run_git(Some(cache), ["remote", "add", "origin", &spec.remote]).await?;
        ensure_git_success(output, "add repository cache origin")?;
    }

    let output = run_git(
        Some(cache),
        [
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    )
    .await?;
    ensure_git_success(output, "configure repository cache refspec")?;
    let output = run_git(Some(cache), ["fetch", "origin", "--prune"]).await?;
    ensure_git_success(output, "fetch repository cache")?;
    let head = run_git(Some(cache), ["remote", "set-head", "origin", "-a"]).await?;
    if !head.status.success() {
        log::debug!(
            "could not set repository cache origin/HEAD: {}",
            head.status
        );
    }
    Ok(())
}

fn store_is_fresh(cache: &Path, max_age: Duration) -> bool {
    let marker = cache.join("zession-fetched-at");
    let Ok(contents) = fs::read_to_string(marker) else {
        return false;
    };
    let Ok(fetched_at) = DateTime::parse_from_rfc3339(contents.trim()) else {
        return false;
    };
    Utc::now()
        .signed_duration_since(fetched_at.with_timezone(&Utc))
        .to_std()
        .is_ok_and(|age| age <= max_age)
}

pub async fn attach_repo(
    session_id: &str,
    spec: &RepoSpec,
    name: Option<&str>,
    base: Option<&str>,
    settings: &ZessionSettings,
) -> Result<SessionRepo> {
    attach_repo_at(&zession_home(), session_id, spec, name, base, settings).await
}

pub async fn attach_repo_at(
    home: &Path,
    session_id: &str,
    spec: &RepoSpec,
    name: Option<&str>,
    base: Option<&str>,
    settings: &ZessionSettings,
) -> Result<SessionRepo> {
    let mut manifest = read_manifest_at(home, session_id)?;
    let session_dir = home.join("workspaces").join(session_id);
    ensure!(session_dir.is_dir(), "session workspace does not exist");
    if !is_allowed(settings, &spec.repo_id)? {
        return Err(ZessionError::NotApproved {
            repo_id: spec.repo_id.clone(),
        }
        .into());
    }
    let name = name.unwrap_or(&spec.name);
    ensure!(valid_repo_name(name), "invalid repository name {name:?}");
    ensure!(
        !manifest.repos.iter().any(|repo| repo.name == name),
        "repository name {name:?} is already attached"
    );
    let path = session_dir.join(name);
    ensure!(
        !path.exists(),
        "repository path {} already exists",
        path.display()
    );

    let store = ensure_store_fresh(
        spec,
        Duration::from_secs(settings.store_max_age_minutes.saturating_mul(60)),
        false,
    )
    .await?;
    apply_repo_access(&store, access_for(settings, &spec.repo_id)?).await?;
    let base_ref = select_base(&store.cache, base).await?;
    let base_commit = git_stdout(
        Some(&store.cache),
        ["rev-parse".to_string(), format!("{base_ref}^{{commit}}")],
    )
    .await?;
    let base_commit = base_commit.trim().to_string();
    let primary_branch = format!("session/{session_id}");
    let primary_is_checked_out = branch_is_checked_out(&store.cache, &primary_branch).await?;
    let branch = if primary_is_checked_out {
        format!("{primary_branch}-{name}")
    } else {
        primary_branch
    };
    let branch_exists = branch_exists(&store.cache, &branch).await?;
    let mut arguments = vec!["worktree".to_string(), "add".to_string()];
    if !branch_exists {
        arguments.push("-b".to_string());
        arguments.push(branch.clone());
    }
    arguments.push(path.to_string_lossy().into_owned());
    if branch_exists {
        arguments.push(branch.clone());
    } else {
        arguments.push(base_ref.clone());
    }
    let output = run_git(Some(&store.cache), arguments).await?;
    ensure_git_success(output, "create repository worktree")?;

    let repository = SessionRepo {
        name: name.to_string(),
        repo_id: spec.repo_id.clone(),
        remote: spec.remote.clone(),
        cache: store.cache.clone(),
        path: path.clone(),
        branch,
        base_ref,
        base_commit,
    };
    manifest.repos.push(repository.clone());
    if let Err(error) = write_manifest_at(home, &manifest).and_then(|()| {
        write_rules(&Session {
            dir: session_dir.clone(),
            manifest: manifest.clone(),
        })
    }) {
        let remove_output = run_git(
            Some(&store.cache),
            ["worktree", "remove", "--force", &path.to_string_lossy()],
        )
        .await;
        if let Err(remove_error) = remove_output {
            log::warn!("failed to remove incomplete repository worktree: {remove_error:#}");
        }
        return Err(error);
    }
    Ok(repository)
}

async fn apply_repo_access(store: &StoreRepo, access: Access) -> Result<()> {
    match access {
        Access::Read => {
            let output = run_git(
                Some(&store.cache),
                ["config", "remote.origin.pushurl", READ_ONLY_PUSH_URL],
            )
            .await?;
            ensure_git_success(output, "set read-only repository access")
        }
        Access::Write => {
            let output = run_git(
                Some(&store.cache),
                ["config", "--unset-all", "remote.origin.pushurl"],
            )
            .await?;
            if output.status.success() || output.status.code() == Some(5) {
                Ok(())
            } else {
                Err(git_error(output, "clear read-only repository access"))
            }
        }
    }
}

async fn select_base(cache: &Path, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        let requested = requested.trim();
        ensure!(
            !requested.is_empty() && !requested.starts_with('-'),
            "base must be a non-option branch or commit"
        );
        let candidate = if requested.starts_with("origin/") {
            format!("refs/remotes/{requested}")
        } else {
            format!("refs/remotes/origin/{requested}")
        };
        let output = run_git(
            Some(cache),
            ["rev-parse", "--verify", &format!("{candidate}^{{commit}}")],
        )
        .await?;
        if output.status.success() {
            return Ok(candidate
                .strip_prefix("refs/remotes/")
                .unwrap_or(&candidate)
                .to_string());
        }
        let commit = run_git(
            Some(cache),
            ["rev-parse", "--verify", &format!("{requested}^{{commit}}")],
        )
        .await?;
        ensure_git_success(commit, "resolve requested base")?;
        return Ok(requested.to_string());
    }

    let head = run_git(
        Some(cache),
        ["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .await?;
    if head.status.success() {
        let head = String::from_utf8_lossy(&head.stdout);
        let head = head.trim();
        if let Some(branch) = head.strip_prefix("refs/remotes/") {
            return Ok(branch.to_string());
        }
    }
    let local_head = run_git(Some(cache), ["symbolic-ref", "--quiet", "HEAD"]).await?;
    if local_head.status.success() {
        let head = String::from_utf8_lossy(&local_head.stdout);
        if let Some(branch) = head.trim().strip_prefix("refs/heads/") {
            let candidate = format!("origin/{branch}");
            let output = run_git(
                Some(cache),
                [
                    "rev-parse",
                    "--verify",
                    &format!("refs/remotes/{candidate}^{{commit}}"),
                ],
            )
            .await?;
            if output.status.success() {
                return Ok(candidate);
            }
            let output = run_git(
                Some(cache),
                [
                    "rev-parse",
                    "--verify",
                    &format!("refs/heads/{branch}^{{commit}}"),
                ],
            )
            .await?;
            ensure_git_success(output, "resolve repository default branch")?;
            return Ok(format!("refs/heads/{branch}"));
        }
    }
    bail!("repository has no default branch")
}

async fn branch_exists(cache: &Path, branch: &str) -> Result<bool> {
    let output = run_git(
        Some(cache),
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_error(output, "inspect repository branch")),
    }
}

async fn branch_is_checked_out(cache: &Path, branch: &str) -> Result<bool> {
    let output = run_git(Some(cache), ["worktree", "list", "--porcelain"]).await?;
    let worktrees = String::from_utf8_lossy(&output.stdout).into_owned();
    ensure_git_success(output, "list repository worktrees")?;
    let expected = format!("branch refs/heads/{branch}");
    Ok(worktrees.lines().any(|line| line == expected))
}

fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && !name.contains(['/', '\\'])
        && Path::new(name).file_name().and_then(|value| value.to_str()) == Some(name)
}

pub async fn session_status(session_id: &str) -> Result<StatusJson> {
    session_status_at(&zession_home(), session_id).await
}

pub async fn session_status_at(home: &Path, session_id: &str) -> Result<StatusJson> {
    let manifest = read_manifest_at(home, session_id)?;
    let path = home.join("workspaces").join(session_id);
    let mut repos = Vec::with_capacity(manifest.repos.len());
    for repo in &manifest.repos {
        let output = run_git(Some(&repo.path), ["status", "--porcelain"]).await?;
        let status = String::from_utf8_lossy(&output.stdout).into_owned();
        ensure_git_success(output, "read repository status")?;
        let changed = status.lines().filter(|line| !line.is_empty()).count();
        repos.push(StatusRepo {
            name: repo.name.clone(),
            repo_id: repo.repo_id.clone(),
            path: repo.path.clone(),
            branch: repo.branch.clone(),
            base_ref: repo.base_ref.clone(),
            changed,
        });
    }
    Ok(StatusJson {
        id: manifest.id.clone(),
        title: manifest.title.unwrap_or_else(|| manifest.id.clone()),
        path,
        repos,
    })
}

pub async fn read_repo(
    repo_id: &str,
    operation: RepoOperation,
    settings: &ZessionSettings,
) -> Result<String> {
    read_repo_at(&zession_home(), repo_id, operation, settings).await
}

pub async fn read_repo_at(
    home: &Path,
    repo_id: &str,
    operation: RepoOperation,
    settings: &ZessionSettings,
) -> Result<String> {
    ensure!(
        !repo_id.trim().is_empty() && !repo_id.starts_with('-'),
        "repo must be a repository identifier"
    );
    ensure!(
        is_allowed(settings, repo_id)?,
        "repository {repo_id} is not approved"
    );
    let spec = repo_spec_for_id_at(repo_id, home).await?;
    let store = ensure_store_fresh(
        &spec,
        Duration::from_secs(settings.store_max_age_minutes.saturating_mul(60)),
        false,
    )
    .await?;
    let is_grep = matches!(&operation, RepoOperation::Grep { .. });
    let arguments = read_repo_git_args(operation)?;
    let output = run_git(Some(&store.cache), arguments).await?;
    if is_grep && output.status.code() == Some(1) && output.stdout.is_empty() {
        return Ok("No matches.".to_string());
    }
    if !output.status.success() {
        return Err(git_error(output, "read repository"));
    }
    Ok(truncate_output(&output.stdout))
}

fn read_repo_git_args(operation: RepoOperation) -> Result<Vec<String>> {
    let mut arguments = Vec::new();
    match operation {
        RepoOperation::ListFiles { git_ref, path } => {
            let git_ref = validate_git_ref(git_ref.as_deref())?;
            arguments.extend(["ls-tree".into(), "-r".into(), "--name-only".into(), git_ref]);
            if let Some(path) = path.as_deref() {
                arguments.extend(["--".into(), validate_path(path)?]);
            }
        }
        RepoOperation::Show { path, git_ref } => {
            let git_ref = validate_git_ref(git_ref.as_deref())?;
            arguments.extend([
                "show".into(),
                format!("{git_ref}:{}", validate_path(&path)?),
            ]);
        }
        RepoOperation::Grep {
            pattern,
            git_ref,
            path,
        } => {
            ensure!(
                !pattern.trim().is_empty() && !pattern.starts_with('-'),
                "pattern must be non-empty and must not start with '-'"
            );
            let git_ref = validate_git_ref(git_ref.as_deref())?;
            arguments.extend([
                "grep".into(),
                "-n".into(),
                "-I".into(),
                "-E".into(),
                "-e".into(),
                pattern,
                git_ref,
            ]);
            if let Some(path) = path.as_deref() {
                arguments.extend(["--".into(), validate_path(path)?]);
            }
        }
        RepoOperation::Log { git_ref, path } => {
            let git_ref = validate_git_ref(git_ref.as_deref())?;
            arguments.extend([
                "log".into(),
                "--oneline".into(),
                "-n".into(),
                "30".into(),
                git_ref,
            ]);
            if let Some(path) = path.as_deref() {
                arguments.extend(["--".into(), validate_path(path)?]);
            }
        }
    }
    Ok(arguments)
}

fn validate_git_ref(git_ref: Option<&str>) -> Result<String> {
    let git_ref = git_ref.unwrap_or("HEAD");
    ensure!(
        !git_ref.trim().is_empty() && !git_ref.starts_with('-'),
        "git_ref must be non-empty and must not start with '-'"
    );
    Ok(git_ref.to_string())
}

fn validate_path(path: &str) -> Result<String> {
    ensure!(!path.trim().is_empty(), "path must be non-empty");
    ensure!(!path.starts_with('-'), "path must not start with '-'");
    Ok(path.to_string())
}

async fn repo_spec_for_id_at(repo_id: &str, home: &Path) -> Result<RepoSpec> {
    if let Some(name) = repo_id.strip_prefix("local/") {
        ensure!(
            valid_cache_part(name),
            "invalid local repository id {repo_id:?}"
        );
        let cache = home.join("store/local").join(format!("{name}.git"));
        let remote = git_stdout(Some(&cache), ["remote", "get-url", "origin"]).await?;
        return Ok(RepoSpec {
            repo_id: repo_id.to_string(),
            host: "local".to_string(),
            owner: "local".to_string(),
            name: name.to_string(),
            remote: remote.trim().to_string(),
            cache,
            local: true,
        });
    }
    RepoSpec::parse_at(repo_id, home)
}

pub async fn remove_session(session_id: &str) -> Result<()> {
    remove_session_at(&zession_home(), session_id).await
}

pub async fn remove_session_at(home: &Path, session_id: &str) -> Result<()> {
    let manifest = read_manifest_at(home, session_id)?;
    for repo in &manifest.repos {
        let output = run_git(
            Some(&repo.cache),
            [
                "worktree",
                "remove",
                "--force",
                &repo.path.to_string_lossy(),
            ],
        )
        .await?;
        if !output.status.success() && repo.path.exists() {
            return Err(git_error(output, "remove repository worktree"));
        }
        let output = run_git(Some(&repo.cache), ["worktree", "prune"]).await?;
        ensure_git_success(output, "prune repository worktrees")?;
    }
    let workspace = home.join("workspaces").join(session_id);
    match fs::remove_dir_all(&workspace) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("remove {}", workspace.display())),
    }
    let manifest_path = home.join("sessions").join(format!("{session_id}.json"));
    match fs::remove_file(&manifest_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("remove {}", manifest_path.display()));
        }
    }
    Ok(())
}

async fn run_git<I, S>(directory: Option<&Path>, arguments: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = new_command("git");
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    command.args(arguments);
    command.output().await.context("run git")
}

async fn git_stdout<I, S>(directory: Option<&Path>, arguments: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = run_git(directory, arguments).await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    ensure_git_success(output, "run git command")?;
    Ok(stdout)
}

fn ensure_git_success(output: Output, operation: &str) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(git_error(output, operation))
    }
}

fn git_error(output: Output, operation: &str) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        anyhow!("{operation} failed with {}", output.status)
    } else {
        anyhow!("{operation} failed: {stderr}")
    }
}

fn truncate_output(output: &[u8]) -> String {
    let truncated = output.len() > OUTPUT_LIMIT;
    let mut text = String::from_utf8_lossy(&output[..output.len().min(OUTPUT_LIMIT)]).into_owned();
    if truncated {
        text.push_str("\n[output truncated]");
    }
    text
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RepoCatalogEntry {
    #[serde(default, rename = "full_name")]
    pub full_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "pushed_at")]
    pub pushed_at: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default, rename = "default_branch")]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub topics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoHit {
    pub repo_id: String,
    pub description: String,
    pub stored: bool,
    pub indexed: bool,
    pub attached_in_current_session: bool,
    pub excerpt: String,
}

pub async fn refresh_catalog(
    http_client: Arc<dyn HttpClient>,
    owner: &str,
) -> Result<Vec<RepoCatalogEntry>> {
    ensure!(
        valid_cache_part(owner),
        "invalid repository owner {owner:?}"
    );
    refresh_catalog_at(&http_client, owner, &zession_home()).await
}

async fn refresh_catalog_at(
    http_client: &Arc<dyn HttpClient>,
    owner: &str,
    home: &Path,
) -> Result<Vec<RepoCatalogEntry>> {
    let catalog_path = home.join("index/catalog").join(format!("{owner}.json"));
    if let Some(catalog) = read_fresh_catalog(&catalog_path) {
        return Ok(catalog);
    }
    let token = github_token().await;
    let mut repositories = Vec::new();
    for page in 1..=10 {
        let url = format!(
            "https://api.github.com/users/{owner}/repos?per_page=100&page={page}&sort=pushed"
        );
        let response = github_get(http_client, &url, token.as_deref()).await?;
        let page_repositories: Vec<RepoCatalogEntry> = serde_json::from_str(&response)
            .with_context(|| format!("parse GitHub repository catalog for {owner}"))?;
        let page_len = page_repositories.len();
        repositories.extend(page_repositories);
        if page_len < 100 {
            break;
        }
    }
    repositories.truncate(1000);
    repositories.retain(|repository| !repository.archived);
    write_catalog(&catalog_path, &repositories)?;
    Ok(repositories)
}

async fn refresh_catalog_repo_at(
    http_client: &Arc<dyn HttpClient>,
    owner: &str,
    name: &str,
    home: &Path,
) -> Result<Vec<RepoCatalogEntry>> {
    let catalog_path = home.join("index/catalog").join(format!("{owner}.json"));
    let mut catalog = read_fresh_catalog(&catalog_path).unwrap_or_default();
    if catalog
        .iter()
        .any(|repository| repository.full_name == format!("{owner}/{name}"))
    {
        return Ok(catalog);
    }
    let token = github_token().await;
    let url = format!("https://api.github.com/repos/{owner}/{name}");
    let response = github_get(http_client, &url, token.as_deref()).await?;
    let repository: RepoCatalogEntry = serde_json::from_str(&response)
        .with_context(|| format!("parse GitHub repository {owner}/{name}"))?;
    if !repository.archived {
        catalog.retain(|existing| existing.full_name != repository.full_name);
        catalog.push(repository);
        write_catalog(&catalog_path, &catalog)?;
    }
    Ok(catalog)
}

fn read_fresh_catalog(path: &Path) -> Option<Vec<RepoCatalogEntry>> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    if modified.elapsed().ok()? > Duration::from_secs(60 * 60) {
        return None;
    }
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn write_catalog(path: &Path, catalog: &[RepoCatalogEntry]) -> Result<()> {
    let mut contents = serde_json::to_vec_pretty(catalog)?;
    contents.push(b'\n');
    atomic_write(path, &contents)
}

async fn github_token() -> Option<String> {
    if let Some(token) = env::var("GITHUB_TOKEN")
        .ok()
        .or_else(|| env::var("GH_TOKEN").ok())
        .filter(|token| !token.trim().is_empty())
    {
        return Some(token);
    }
    let mut command = new_command("gh");
    match command.args(["auth", "token"]).output().await {
        Ok(output) if output.status.success() => {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!token.is_empty()).then_some(token)
        }
        _ => None,
    }
}

async fn github_get(
    http_client: &Arc<dyn HttpClient>,
    url: &str,
    token: Option<&str>,
) -> Result<String> {
    let mut request = Request::builder()
        .method(Method::GET)
        .uri(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "Zed-Zession");
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {}", token.trim()));
    }
    let request = request.body(AsyncBody::default())?;
    let mut response = http_client.send(request).await?;
    let status = response.status();
    let mut body = String::new();
    smol::io::AsyncReadExt::read_to_string(response.body_mut(), &mut body).await?;
    ensure!(
        status.is_success(),
        "GitHub API request failed with {status}"
    );
    Ok(body)
}

pub async fn index_repo(
    model: LanguageModel,
    store_repo: &StoreRepo,
    cx: &gpui::AsyncApp,
) -> Result<PathBuf> {
    index_repo_at(&zession_home(), model, store_repo, cx).await
}

pub fn resolve_index_model(settings: &ZessionSettings, cx: &App) -> Option<LanguageModel> {
    let registry = LanguageModelRegistry::read_global(cx);
    settings
        .index_model
        .as_ref()
        .and_then(|selection| {
            let provider_id = LanguageModelProviderId::from(selection.provider.0.clone());
            let model_id = LanguageModelId::from(selection.model.clone());
            registry
                .provider(&provider_id)?
                .provided_models(cx)
                .into_iter()
                .find(|model| model.id == model_id)
        })
        .or_else(|| registry.thread_summary_model(cx))
        .or_else(|| registry.default_fast_model(cx))
}

pub async fn index_repo_if_enabled(
    settings: &ZessionSettings,
    store_repo: &StoreRepo,
    cx: &gpui::AsyncApp,
) -> Result<Option<PathBuf>> {
    if !settings.auto_index {
        return Ok(None);
    }
    let model = cx.update(|cx| resolve_index_model(settings, cx));
    let Some(model) = model else {
        return Ok(None);
    };
    index_repo(model, store_repo, cx).await.map(Some)
}

pub async fn index_repo_at(
    home: &Path,
    model: LanguageModel,
    store_repo: &StoreRepo,
    cx: &gpui::AsyncApp,
) -> Result<PathBuf> {
    let index_lock = INDEX_LOCK.get_or_init(|| AsyncMutex::new(()));
    let _guard = index_lock.lock().await;
    let default_ref = select_base(&store_repo.cache, None).await?;
    let commit = git_stdout(
        Some(&store_repo.cache),
        ["rev-parse", &format!("{default_ref}^{{commit}}")],
    )
    .await?
    .trim()
    .to_string();
    let index_path = index_path_for(home, store_repo);
    if index_commit(&index_path).as_deref() == Some(commit.as_str()) {
        return Ok(index_path);
    }

    let context = gather_index_context(store_repo, &commit).await?;
    let mut prompt = INDEX_PROMPT
        .replace("{repo}", &store_repo.repo_id)
        .replace("{remote}", &store_repo.remote)
        .replace("{commit}", &commit);
    prompt.push_str(&context.file_list);
    prompt.push_str(INDEX_FILE_CONTENTS_HEADER);
    for file in context.files {
        prompt.push_str("\n--- ");
        prompt.push_str(&file.path);
        prompt.push_str(" ---\n");
        prompt.push_str(&file.contents);
        prompt.push('\n');
    }
    let request = LanguageModelRequest {
        thread_id: None,
        prompt_cache_key: None,
        prompt_id: None,
        intent: None,
        messages: vec![LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text(prompt)],
            cache: false,
            reasoning_details: None,
        }],
        tools: Vec::new(),
        tool_choice: None,
        stop: Vec::new(),
        temperature: None,
        thinking_allowed: false,
        thinking_effort: None,
        speed: None,
        compact_at_tokens: None,
        max_output_tokens: Some(3000),
    };
    let provider =
        cx.update(|cx| LanguageModelRegistry::read_global(cx).provider_for_model(&model))?;
    let mut events = provider.stream_completion(&model, request, cx).await?;
    let mut summary = String::new();
    while let Some(event) = events.next().await {
        if let LanguageModelCompletionEvent::Text(text) = event? {
            summary.push_str(&text);
        }
    }
    ensure!(
        !summary.trim().is_empty(),
        "language model returned an empty index"
    );
    let indexed_at = Utc::now().to_rfc3339();
    let frontmatter = format!(
        "---\nrepo: {}\nremote: {}\ncommit: {}\nindexed_at: {}\n---\n\n",
        serde_json::to_string(&store_repo.repo_id)?,
        serde_json::to_string(&store_repo.remote)?,
        serde_json::to_string(&commit)?,
        serde_json::to_string(&indexed_at)?
    );
    let mut output = frontmatter;
    output.push_str(&summary);
    atomic_write(&index_path, output.as_bytes())?;
    Ok(index_path)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexContextFile {
    pub path: String,
    pub contents: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexContext {
    pub file_list: String,
    pub files: Vec<IndexContextFile>,
    pub total_bytes: usize,
}

pub async fn gather_index_context(store_repo: &StoreRepo, commit: &str) -> Result<IndexContext> {
    let output = run_git(
        Some(&store_repo.cache),
        ["ls-tree", "-r", "--name-only", commit],
    )
    .await?;
    let paths_output = String::from_utf8_lossy(&output.stdout).into_owned();
    ensure_git_success(output, "list repository files for indexing")?;
    let all_paths = paths_output.lines().collect::<Vec<_>>();
    let path_limit_truncated = all_paths.len() > INDEX_PATH_LIMIT;
    let paths = all_paths
        .into_iter()
        .take(INDEX_PATH_LIMIT)
        .collect::<Vec<_>>();
    let (file_list, mut total_bytes) = build_index_file_list(&paths, path_limit_truncated);
    let mut files = Vec::new();
    for path in paths.into_iter().filter(|path| should_index_path(path)) {
        let path = path.to_owned();
        let file_header_bytes = "\n--- ".len() + path.len() + " ---\n".len();
        let file_suffix_bytes = 1;
        let content_limit =
            INDEX_CONTEXT_LIMIT.saturating_sub(total_bytes + file_header_bytes + file_suffix_bytes);
        if content_limit == 0 {
            break;
        }
        let output = run_git(
            Some(&store_repo.cache),
            ["show", &format!("{commit}:{path}")],
        )
        .await?;
        if !output.status.success() {
            log::debug!("could not read index context file {path}");
            continue;
        }
        let cap = INDEX_FILE_LIMIT.min(content_limit);
        let bytes = &output.stdout[..output.stdout.len().min(cap)];
        let lossy_contents = String::from_utf8_lossy(bytes);
        let contents = truncate_to_bytes(&lossy_contents, cap).to_owned();
        total_bytes += file_header_bytes + contents.len() + file_suffix_bytes;
        files.push(IndexContextFile { path, contents });
    }
    Ok(IndexContext {
        file_list,
        files,
        total_bytes,
    })
}

fn build_index_file_list(paths: &[&str], path_limit_truncated: bool) -> (String, usize) {
    let path_limit_notice = if path_limit_truncated {
        format!(
            "\n[File list truncated at the path limit; showing the first {INDEX_PATH_LIMIT} paths.]"
        )
    } else {
        String::new()
    };
    let joined_paths = paths.join("\n");
    let complete_file_list = format!("{INDEX_FILE_LIST_HEADER}{joined_paths}{path_limit_notice}");
    let complete_context_bytes = complete_file_list.len() + INDEX_FILE_CONTENTS_HEADER.len();
    if complete_context_bytes <= INDEX_CONTEXT_LIMIT {
        return (complete_file_list, complete_context_bytes);
    }

    let context_limit_notice =
        format!("\n[File list truncated to fit the {INDEX_CONTEXT_LIMIT}-byte context limit.]");
    let fixed_bytes = INDEX_FILE_LIST_HEADER.len()
        + path_limit_notice.len()
        + context_limit_notice.len()
        + INDEX_FILE_CONTENTS_HEADER.len();
    let path_bytes_limit = INDEX_CONTEXT_LIMIT.saturating_sub(fixed_bytes);
    let mut truncated_paths = String::new();
    for path in paths {
        let separator_bytes = usize::from(!truncated_paths.is_empty());
        if truncated_paths.len() + separator_bytes + path.len() > path_bytes_limit {
            break;
        }
        if separator_bytes > 0 {
            truncated_paths.push('\n');
        }
        truncated_paths.push_str(path);
    }

    let file_list = format!(
        "{INDEX_FILE_LIST_HEADER}{truncated_paths}{path_limit_notice}{context_limit_notice}"
    );
    let total_bytes = file_list.len() + INDEX_FILE_CONTENTS_HEADER.len();
    (file_list, total_bytes)
}

fn truncate_to_bytes(contents: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(contents.len());
    while !contents.is_char_boundary(end) {
        end -= 1;
    }
    &contents[..end]
}

fn should_index_path(path: &str) -> bool {
    if let Some(name) = path.strip_prefix(".github/workflows/") {
        return name.ends_with(".yml");
    }
    if path.contains('/') {
        return false;
    }
    let name = path;
    name.starts_with("README")
        || matches!(name, "AGENTS.md" | "CLAUDE.md")
        || name.starts_with("CONTRIBUTING")
        || matches!(
            name,
            "Cargo.toml"
                | "package.json"
                | "go.mod"
                | "pyproject.toml"
                | "requirements.txt"
                | "Gemfile"
                | "pom.xml"
                | "Makefile"
                | "Dockerfile"
                | "docker-compose.yml"
        )
        || name.starts_with("requirements.")
        || name.starts_with("build.gradle")
        || name.starts_with("docker-compose.")
        || name.ends_with(".tf")
}

fn index_path_for(home: &Path, store_repo: &StoreRepo) -> PathBuf {
    if store_repo.local {
        home.join("index")
            .join("local")
            .join(format!("{}.md", store_repo.name))
    } else {
        home.join("index")
            .join(&store_repo.host)
            .join(&store_repo.owner)
            .join(format!("{}.md", store_repo.name))
    }
}

fn index_commit(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    let (frontmatter, _) = split_frontmatter(&contents)?;
    frontmatter.get("commit").cloned()
}

fn split_frontmatter(contents: &str) -> Option<(HashMap<String, String>, &str)> {
    let mut lines = contents.lines();
    if lines.next()? != "---" {
        return None;
    }
    let mut fields = HashMap::new();
    let mut body_offset = 4;
    for line in lines {
        if line == "---" {
            body_offset += line.len() + 1;
            return Some((fields, contents.get(body_offset..).unwrap_or_default()));
        }
        body_offset += line.len() + 1;
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        let decoded = serde_json::from_str::<String>(value)
            .unwrap_or_else(|_| value.trim_matches('"').to_string());
        fields.insert(key.trim().to_string(), decoded);
    }
    None
}

pub async fn search_repos(
    query: &str,
    session_id: Option<&str>,
    settings: &ZessionSettings,
    http_client: Arc<dyn HttpClient>,
) -> Result<Vec<RepoHit>> {
    let home = zession_home();
    let scopes = approved_catalog_scopes(settings)?;
    for (owner, exact_repo) in scopes {
        let refreshed = match exact_repo {
            Some(name) => refresh_catalog_repo_at(&http_client, &owner, &name, &home).await,
            None => refresh_catalog_at(&http_client, &owner, &home).await,
        };
        if let Err(error) = refreshed {
            log::debug!("failed to refresh Zession repository catalog for {owner}: {error:#}");
        }
    }
    search_repos_at(query, session_id, settings, &home).await
}

fn approved_catalog_scopes(
    settings: &ZessionSettings,
) -> Result<BTreeSet<(String, Option<String>)>> {
    let mut scopes = BTreeSet::new();
    for approval in &settings.repos_allow {
        let normalized = normalize_approval_entry(approval)?;
        if let Some((owner, repository)) = normalized.split_once('/') {
            if repository == "*" {
                scopes.insert((owner.to_string(), None));
            } else if !repository.contains(['*', '?', '[']) {
                scopes.insert((owner.to_string(), Some(repository.to_string())));
            } else {
                scopes.insert((owner.to_string(), None));
            }
        }
    }
    Ok(scopes)
}

async fn search_repos_at(
    query: &str,
    session_id: Option<&str>,
    settings: &ZessionSettings,
    home: &Path,
) -> Result<Vec<RepoHit>> {
    let mut candidates = BTreeMap::<String, SearchCandidate>::new();
    let catalog_root = home.join("index/catalog");
    if let Ok(entries) = fs::read_dir(&catalog_root) {
        for entry in entries.flatten() {
            if entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("json")
            {
                continue;
            }
            let Ok(contents) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(repositories) = serde_json::from_str::<Vec<RepoCatalogEntry>>(&contents) else {
                continue;
            };
            for repository in repositories {
                if !repository.archived && is_allowed(settings, &repository.full_name)? {
                    let candidate = candidates.entry(repository.full_name.clone()).or_default();
                    candidate.repo_id = repository.full_name;
                    candidate.description = repository.description.unwrap_or_default();
                    candidate.topics = repository.topics;
                }
            }
        }
    }

    for repository in load_store_repos_at(home).await? {
        if !is_allowed(settings, &repository.repo_id)? {
            continue;
        }
        let candidate = candidates.entry(repository.repo_id.clone()).or_default();
        candidate.repo_id = repository.repo_id;
        candidate.stored = true;
        if candidate.description.is_empty() {
            candidate.description = repository.remote;
        }
    }

    for (repo_id, body, remote) in load_index_summaries(home)? {
        if !is_allowed(settings, &repo_id)? {
            continue;
        }
        let candidate = candidates.entry(repo_id.clone()).or_default();
        candidate.repo_id = repo_id;
        candidate.indexed = true;
        candidate.body = body;
        candidate.remote = remote;
    }

    if let Some(session_id) = session_id {
        if let Ok(manifest) = read_manifest_at(home, session_id) {
            for repo in manifest.repos {
                if let Some(candidate) = candidates.get_mut(&repo.repo_id) {
                    candidate.attached = true;
                }
            }
        }
    }

    let terms = search_terms(query);
    let mut hits = candidates
        .into_values()
        .filter_map(|candidate| {
            let score = candidate.score(&terms);
            (score > 0 || terms.is_empty()).then(|| {
                (
                    score,
                    RepoHit {
                        repo_id: candidate.repo_id,
                        description: candidate.description,
                        stored: candidate.stored,
                        indexed: candidate.indexed,
                        attached_in_current_session: candidate.attached,
                        excerpt: excerpt(&candidate.body),
                    },
                )
            })
        })
        .collect::<Vec<_>>();
    hits.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.repo_id.cmp(&right.repo_id))
    });
    let limit = if terms.is_empty() { 1000 } else { 10 };
    Ok(hits.into_iter().take(limit).map(|(_, hit)| hit).collect())
}

#[derive(Default)]
struct SearchCandidate {
    repo_id: String,
    description: String,
    topics: Vec<String>,
    remote: String,
    body: String,
    stored: bool,
    indexed: bool,
    attached: bool,
}

impl SearchCandidate {
    fn score(&self, terms: &[String]) -> usize {
        let name = self.repo_id.to_ascii_lowercase();
        let keyword_section = section(&self.body, "Keywords");
        let description_and_topics = format!(
            "{} {} {}",
            self.description,
            self.topics.join(" "),
            keyword_section
        )
        .to_ascii_lowercase();
        let body = self.body.to_ascii_lowercase();
        terms
            .iter()
            .map(|term| {
                usize::from(name.contains(term)) * 5
                    + usize::from(description_and_topics.contains(term)) * 2
                    + usize::from(body.contains(term))
            })
            .sum()
    }
}

fn search_terms(query: &str) -> Vec<String> {
    query
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn section(markdown: &str, heading: &str) -> String {
    let heading = format!("## {heading}");
    let mut in_section = false;
    let mut output = String::new();
    for line in markdown.lines() {
        if line.starts_with("## ") {
            if in_section {
                break;
            }
            in_section = line.trim() == heading;
        } else if in_section {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

fn excerpt(markdown: &str) -> String {
    let mut text = [section(markdown, "Purpose"), section(markdown, "Keywords")]
        .into_iter()
        .filter(|section| !section.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        text = markdown.to_string();
    }
    let mut excerpt = text.chars().take(600).collect::<String>();
    if text.chars().count() > 600 {
        excerpt.push('…');
    }
    excerpt
}

async fn load_store_repos_at(home: &Path) -> Result<Vec<StoreRepo>> {
    let root = home.join("store");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut directories = vec![root.clone()];
    let mut repositories = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".git") {
                directories.push(path);
                continue;
            }
            let relative = path.strip_prefix(&root)?;
            let parts = relative
                .components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(value) => value.to_str(),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let (host, owner, repo_name, repo_id, local) = match parts.as_slice() {
                ["local", repo] => {
                    let Some(repo_name) = repo.strip_suffix(".git") else {
                        continue;
                    };
                    (
                        "local".to_string(),
                        "local".to_string(),
                        repo_name.to_string(),
                        format!("local/{repo_name}"),
                        true,
                    )
                }
                [host, owner, repo] => {
                    let Some(repo_name) = repo.strip_suffix(".git") else {
                        continue;
                    };
                    (
                        (*host).to_string(),
                        (*owner).to_string(),
                        repo_name.to_string(),
                        format!("{owner}/{repo_name}"),
                        false,
                    )
                }
                _ => continue,
            };
            let remote = git_stdout(Some(&path), ["remote", "get-url", "origin"])
                .await
                .unwrap_or_default()
                .trim()
                .to_string();
            repositories.push(StoreRepo {
                repo_id,
                host,
                owner,
                name: repo_name,
                remote,
                cache: path,
                local,
            });
        }
    }
    Ok(repositories)
}

fn load_index_summaries(home: &Path) -> Result<Vec<(String, String, String)>> {
    let root = home.join("index");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut directories = vec![root.clone()];
    let mut summaries = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if path.strip_prefix(&root).ok().is_some_and(|relative| {
                    relative
                        .components()
                        .next()
                        .is_some_and(|component| component.as_os_str() == "catalog")
                }) {
                    continue;
                }
                directories.push(path);
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
                continue;
            }
            let contents = fs::read_to_string(&path)?;
            let Some((frontmatter, body)) = split_frontmatter(&contents) else {
                continue;
            };
            let repo_id = frontmatter
                .get("repo")
                .cloned()
                .unwrap_or_else(|| infer_repo_id(&root, &path).unwrap_or_default());
            if repo_id.is_empty() {
                continue;
            }
            summaries.push((
                repo_id,
                body.trim().to_string(),
                frontmatter.get("remote").cloned().unwrap_or_default(),
            ));
        }
    }
    Ok(summaries)
}

fn infer_repo_id(index_root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(index_root).ok()?;
    let parts = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    match parts.as_slice() {
        ["local", name] => Some(format!(
            "local/{}",
            name.strip_suffix(".md").unwrap_or(name)
        )),
        [_, owner, name] => Some(format!(
            "{owner}/{}",
            name.strip_suffix(".md").unwrap_or(name)
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn repo_spec_parses_supported_values() {
        let temporary_directory = tempfile::tempdir().expect("temporary directory");
        let home = temporary_directory.path().join(".zession");
        let repository = temporary_directory.path().join("local-project");
        fs::create_dir_all(repository.join(".git")).expect("create local repository");

        let shorthand = RepoSpec::parse_at("owner/repo", &home).expect("parse shorthand");
        assert_eq!(shorthand.repo_id, "owner/repo");
        assert_eq!(shorthand.remote, "https://github.com/owner/repo.git");
        assert_eq!(
            shorthand.cache,
            home.join("store/github.com/owner/repo.git")
        );

        let https = RepoSpec::parse_at("https://git.example/team/project.git", &home)
            .expect("parse HTTPS URL");
        assert_eq!(https.host, "git.example");
        assert_eq!(https.repo_id, "team/project");
        assert_eq!(https.remote, "https://git.example/team/project.git");

        let scp = RepoSpec::parse_at("git@git.example:team/project.git", &home)
            .expect("parse SCP-style SSH URL");
        assert_eq!(scp.host, "git.example");
        assert_eq!(scp.repo_id, "team/project");

        let ssh = RepoSpec::parse_at("ssh://git@git.example/team/project.git", &home)
            .expect("parse SSH URL");
        assert_eq!(ssh.host, "git.example");
        assert_eq!(ssh.repo_id, "team/project");

        let local = RepoSpec::parse_at(repository.to_str().expect("UTF-8 path"), &home)
            .expect("parse local repository");
        assert_eq!(local.repo_id, "local/local-project");
        assert!(local.local);
        assert_eq!(local.cache, home.join("store/local/local-project.git"));
    }

    #[test]
    fn repo_spec_rejects_invalid_values() {
        let temporary_directory = tempfile::tempdir().expect("temporary directory");
        assert!(RepoSpec::parse_at("", temporary_directory.path()).is_err());
        assert!(
            RepoSpec::parse_at("not/a/repository/path/value", temporary_directory.path()).is_err()
        );
        assert!(RepoSpec::parse_at("https:///owner/repo", temporary_directory.path()).is_err());
        assert!(RepoSpec::parse_at("owner/../repo", temporary_directory.path()).is_err());
    }

    #[test]
    fn repository_approval_and_access_policies_match_expected_scope() {
        let mut settings = test_settings();
        assert!(!is_allowed(&settings, "owner/repo").expect("check default policy"));

        settings.repos_allow = vec!["owner".to_string()];
        assert!(is_allowed(&settings, "owner/repo").expect("owner approval"));
        assert!(!is_allowed(&settings, "other/repo").expect("different owner"));

        settings.repos_allow = vec!["owner/repo".to_string()];
        assert!(is_allowed(&settings, "owner/repo").expect("exact approval"));
        assert!(!is_allowed(&settings, "owner/other").expect("exact approval scope"));

        settings.repos_allow = vec!["local".to_string()];
        assert!(is_allowed(&settings, "local/worktree").expect("local approval"));
        assert!(!is_allowed(&settings, "owner/worktree").expect("local approval scope"));

        assert_eq!(
            access_for(&settings, "local/worktree").expect("default access"),
            Access::Write
        );
        settings.repos_access = BTreeMap::from([("local/*".to_string(), Access::Read)]);
        assert_eq!(
            access_for(&settings, "local/worktree").expect("read access"),
            Access::Read
        );
        assert_eq!(
            access_for(&settings, "owner/repo").expect("default write"),
            Access::Write
        );
    }

    #[test]
    fn create_session_writes_manifest_rules_and_workspace() {
        let temporary_directory = tempfile::tempdir().expect("temporary directory");
        let home = temporary_directory.path().join(".zession");
        let session =
            create_session_at(&home, Some("My Native Session!")).expect("create custom Session");

        assert_eq!(session.manifest.id, "my-native-session");
        assert_eq!(session.manifest.version, 1);
        assert!(session.dir.is_dir());
        assert!(session.dir.join(".rules").is_file());
        let manifest = read_manifest_at(&home, &session.manifest.id).expect("read manifest");
        assert_eq!(manifest, session.manifest);
        assert!(
            fs::read_to_string(session.dir.join(".rules"))
                .expect("read rules")
                .contains("session_read_repo")
        );
    }

    #[test]
    fn generated_session_ids_are_unique_and_duplicate_names_error() {
        let temporary_directory = tempfile::tempdir().expect("temporary directory");
        let home = temporary_directory.path().join(".zession");
        let first = create_session_at(&home, None).expect("create first session");
        let second = create_session_at(&home, None).expect("create second session");
        assert_ne!(first.manifest.id, second.manifest.id);

        create_session_at(&home, Some("Same Name")).expect("create named session");
        let duplicate = create_session_at(&home, Some("same-name"));
        assert!(duplicate.is_err());
    }

    #[gpui::test]
    async fn attach_checks_approval_and_creates_distinct_worktrees(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let temporary_directory = tempfile::tempdir().expect("temporary directory");
        let home = temporary_directory.path().join(".zession");
        let repository_path = temporary_directory.path().join("local-project");
        create_test_repository(&repository_path, &[("main.go", "package main\n")]).await;
        let spec = RepoSpec::parse_at(
            repository_path.to_str().expect("UTF-8 repository path"),
            &home,
        )
        .expect("parse local repository");
        let session = create_session_at(&home, Some("attach-test")).expect("create Session");

        let denied =
            attach_repo_at(&home, "attach-test", &spec, None, None, &test_settings()).await;
        assert!(matches!(
            denied
                .expect_err("unapproved local repository must be rejected")
                .downcast_ref::<ZessionError>(),
            Some(ZessionError::NotApproved { repo_id }) if repo_id == "local/local-project"
        ));

        let mut settings = test_settings();
        settings.repos_allow = vec!["local".into()];
        let first = attach_repo_at(&home, "attach-test", &spec, None, None, &settings)
            .await
            .expect("attach approved repository");
        assert_eq!(first.path, session.dir.join("local-project"));
        assert_eq!(first.branch, "session/attach-test");
        assert!(first.path.is_dir());
        assert!(!first.base_ref.is_empty());
        assert!(!first.base_commit.is_empty());

        let manifest = read_manifest_at(&home, "attach-test").expect("read Session manifest");
        assert_eq!(manifest.repos.len(), 1);
        assert_eq!(manifest.repos[0].repo_id, "local/local-project");
        assert_eq!(manifest.repos[0].base_ref, first.base_ref);
        assert_eq!(manifest.repos[0].base_commit, first.base_commit);
        assert!(
            attach_repo_at(&home, "attach-test", &spec, None, None, &settings)
                .await
                .is_err()
        );

        let second = attach_repo_at(
            &home,
            "attach-test",
            &spec,
            Some("another-name"),
            None,
            &settings,
        )
        .await
        .expect("attach repository under another name");
        assert_eq!(second.branch, "session/attach-test-another-name");

        fs::write(first.path.join("changed.txt"), "changed").expect("write worktree file");
        let status = session_status_at(&home, "attach-test")
            .await
            .expect("read Session status");
        assert_eq!(status.repos[0].changed, 1);

        let store_path = first.cache.clone();
        remove_session_at(&home, "attach-test")
            .await
            .expect("remove Session");
        assert!(!session.dir.exists());
        assert!(!home.join("sessions/attach-test.json").exists());
        assert!(store_path.is_dir());
    }

    #[gpui::test]
    async fn read_repo_supports_listing_show_grep_and_log(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let temporary_directory = tempfile::tempdir().expect("temporary directory");
        let home = temporary_directory.path().join(".zession");
        let repository_path = temporary_directory.path().join("read-project");
        create_test_repository(
            &repository_path,
            &[("main.go", "package main\nneedle-value\n")],
        )
        .await;
        let spec = RepoSpec::parse_at(
            repository_path.to_str().expect("UTF-8 repository path"),
            &home,
        )
        .expect("parse local repository");
        create_session_at(&home, Some("read-test")).expect("create Session");
        let mut settings = test_settings();
        settings.repos_allow = vec!["local".into()];
        attach_repo_at(&home, "read-test", &spec, None, None, &settings)
            .await
            .expect("attach repository");

        let files = read_repo_at(
            &home,
            "local/read-project",
            RepoOperation::ListFiles {
                git_ref: None,
                path: None,
            },
            &settings,
        )
        .await
        .expect("list repository files");
        assert!(files.contains("main.go"));
        let shown = read_repo_at(
            &home,
            "local/read-project",
            RepoOperation::Show {
                path: "main.go".into(),
                git_ref: None,
            },
            &settings,
        )
        .await
        .expect("show repository file");
        assert!(shown.contains("needle-value"));
        let matches = read_repo_at(
            &home,
            "local/read-project",
            RepoOperation::Grep {
                pattern: "needle-value|package".into(),
                git_ref: None,
                path: None,
            },
            &settings,
        )
        .await
        .expect("grep repository");
        assert!(matches.contains("package main"));
        assert!(matches.contains("needle-value"));
        let no_matches = read_repo_at(
            &home,
            "local/read-project",
            RepoOperation::Grep {
                pattern: "no-such-pattern".into(),
                git_ref: None,
                path: None,
            },
            &settings,
        )
        .await
        .expect("grep with no matches");
        assert_eq!(no_matches, "No matches.");
        let log = read_repo_at(
            &home,
            "local/read-project",
            RepoOperation::Log {
                git_ref: None,
                path: None,
            },
            &settings,
        )
        .await
        .expect("read repository log");
        assert!(log.contains("base commit"));

        let grep_arguments = read_repo_git_args(RepoOperation::Grep {
            pattern: "a|b".into(),
            git_ref: None,
            path: None,
        })
        .expect("build grep arguments");
        assert_eq!(
            grep_arguments,
            ["grep", "-n", "-I", "-E", "-e", "a|b", "HEAD"]
        );

        assert!(
            read_repo_git_args(RepoOperation::Show {
                path: "-option".into(),
                git_ref: None,
            })
            .is_err()
        );
        assert!(
            read_repo_git_args(RepoOperation::ListFiles {
                git_ref: Some("-option".into()),
                path: None,
            })
            .is_err()
        );
        assert!(
            read_repo_git_args(RepoOperation::Grep {
                pattern: "-option".into(),
                git_ref: None,
                path: None,
            })
            .is_err()
        );
    }

    #[gpui::test]
    async fn search_ranks_name_matches_and_marks_attached_repositories(
        _cx: &mut gpui::TestAppContext,
    ) {
        let temporary_directory = tempfile::tempdir().expect("temporary directory");
        let home = temporary_directory.path().join(".zession");
        fs::create_dir_all(home.join("index/catalog")).expect("create catalog directory");
        fs::create_dir_all(home.join("index/github.com/owner")).expect("create index directory");
        write_catalog(
            &home.join("index/catalog/owner.json"),
            &[
                RepoCatalogEntry {
                    full_name: "owner/session-tool".into(),
                    description: Some("A repository".into()),
                    ..Default::default()
                },
                RepoCatalogEntry {
                    full_name: "owner/body-match".into(),
                    description: Some("Another repository".into()),
                    ..Default::default()
                },
            ],
        )
        .expect("write repository catalog");
        fs::write(
            home.join("index/github.com/owner/body-match.md"),
            "---\nrepo: \"owner/body-match\"\nremote: \"https://github.com/owner/body-match.git\"\ncommit: \"abc\"\nindexed_at: \"2025-01-01T00:00:00Z\"\n---\n## Purpose\nThis repository documents Session behavior.\n",
        )
        .expect("write repository summary");
        let session = create_session_at(&home, Some("search-test")).expect("create Session");
        let mut manifest = session.manifest;
        manifest.repos.push(SessionRepo {
            name: "body-match".into(),
            repo_id: "owner/body-match".into(),
            remote: "https://github.com/owner/body-match.git".into(),
            cache: home.join("store/github.com/owner/body-match.git"),
            path: session.dir.join("body-match"),
            branch: "session/search-test".into(),
            base_ref: "origin/main".into(),
            base_commit: "abc".into(),
        });
        write_manifest_at(&home, &manifest).expect("write attached Session manifest");

        let mut settings = test_settings();
        settings.repos_allow = vec!["owner".into()];
        let hits = search_repos_at("session", Some("search-test"), &settings, &home)
            .await
            .expect("search repository catalog");
        assert_eq!(
            hits.first().map(|hit| hit.repo_id.as_str()),
            Some("owner/session-tool")
        );
        let attached_hit = hits
            .iter()
            .find(|hit| hit.repo_id == "owner/body-match")
            .expect("indexed body match");
        assert!(attached_hit.attached_in_current_session);
        assert!(attached_hit.indexed);
    }

    #[gpui::test]
    async fn index_context_respects_per_file_and_total_caps(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let temporary_directory = tempfile::tempdir().expect("temporary directory");
        let home = temporary_directory.path().join(".zession");
        let repository_path = temporary_directory.path().join("index-project");
        let readmes = (0..12)
            .map(|index| {
                (
                    format!("README-{index}.md"),
                    "context".repeat(INDEX_FILE_LIMIT),
                )
            })
            .collect::<Vec<_>>();
        let files = readmes
            .iter()
            .map(|(path, contents)| (path.as_str(), contents.as_str()))
            .collect::<Vec<_>>();
        create_test_repository(&repository_path, &files).await;
        let spec = RepoSpec::parse_at(
            repository_path.to_str().expect("UTF-8 repository path"),
            &home,
        )
        .expect("parse local repository");
        let store = ensure_store_fresh(&spec, Duration::ZERO, false)
            .await
            .expect("create local bare cache");
        let commit = git_stdout(Some(&store.cache), ["rev-parse", "HEAD"])
            .await
            .expect("resolve cache head");
        let context = gather_index_context(&store, commit.trim())
            .await
            .expect("gather index context");
        assert!(
            context
                .files
                .iter()
                .all(|file| file.contents.len() <= INDEX_FILE_LIMIT)
        );
        assert!(context.total_bytes <= INDEX_CONTEXT_LIMIT);
    }

    #[test]
    fn index_file_list_notes_path_and_byte_truncation() {
        let paths = (0..=INDEX_PATH_LIMIT)
            .map(|index| format!("src/{index:04}.rs"))
            .collect::<Vec<_>>();
        let path_references = paths
            .iter()
            .take(INDEX_PATH_LIMIT)
            .map(String::as_str)
            .collect::<Vec<_>>();
        let (file_list, total_bytes) = build_index_file_list(&path_references, true);
        assert!(file_list.contains(&format!(
            "File list truncated at the path limit; showing the first {INDEX_PATH_LIMIT} paths"
        )));
        assert!(file_list.contains("src/1999.rs"));
        assert!(!file_list.contains("src/2000.rs"));
        assert!(total_bytes <= INDEX_CONTEXT_LIMIT);

        let long_path = "x".repeat(INDEX_CONTEXT_LIMIT);
        let (file_list, total_bytes) = build_index_file_list(&[&long_path], false);
        assert!(file_list.contains("truncated to fit the 81920-byte context limit"));
        assert!(!file_list.contains(&long_path));
        assert!(total_bytes <= INDEX_CONTEXT_LIMIT);
    }

    #[gpui::test]
    async fn index_repo_uses_fake_language_model_and_writes_frontmatter(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.executor().allow_parking();
        let temporary_directory = tempfile::tempdir().expect("temporary directory");
        let home = temporary_directory.path().join(".zession");
        let repository_path = temporary_directory.path().join("index-project");
        create_test_repository(
            &repository_path,
            &[
                ("README.md", "a small repository"),
                ("src/lib.rs", "nested source is not selected"),
            ],
        )
        .await;
        let spec = RepoSpec::parse_at(
            repository_path.to_str().expect("UTF-8 repository path"),
            &home,
        )
        .expect("parse local repository");
        let store = ensure_store_fresh(&spec, Duration::ZERO, false)
            .await
            .expect("create local bare cache");
        let fake_provider = cx.update(|cx| LanguageModelRegistry::test(cx));
        let model = fake_provider.model("fake");
        let cloned_model = model.clone();
        let cloned_store = store.clone();
        let cloned_home = home.clone();
        let mut indexing_task = cx.spawn(async move |cx| {
            index_repo_at(&cloned_home, cloned_model, &cloned_store, &cx).await
        });
        let mut completion_request = None;
        for _ in 0..500 {
            cx.run_until_parked();
            completion_request = fake_provider
                .pending_completions_for(&model)
                .into_iter()
                .next();
            if completion_request.is_some() {
                break;
            }
            if let Some(result) = futures::FutureExt::now_or_never(&mut indexing_task) {
                panic!("indexing finished before requesting a completion: {result:?}");
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        let request = completion_request.expect("index completion request");
        let prompt = request
            .messages
            .first()
            .expect("index completion message")
            .string_contents();
        assert!(prompt.contains("## File list\nREADME.md\nsrc/lib.rs"));
        assert!(!prompt.contains("\n--- src/lib.rs ---\n"));
        fake_provider.send_text(&model, &request, "## Purpose\nA fixture repository.");
        fake_provider.end_stream(&model, &request);
        let index_path = indexing_task.await.expect("index repository");
        let summary = fs::read_to_string(index_path).expect("read repository index");
        assert!(summary.starts_with("---\nrepo: \"local/index-project\"\n"));
        assert!(summary.contains(&format!(
            "remote: {}",
            serde_json::to_string(&spec.remote).expect("serialize remote")
        )));
        assert!(summary.contains(&format!("commit: \"{}", commit_for_store(&store).await)));
        assert!(summary.contains("indexed_at: \""));
        assert!(summary.contains("A fixture repository."));
    }

    fn test_settings() -> ZessionSettings {
        ZessionSettings {
            repos_allow: Vec::new(),
            repos_access: BTreeMap::new(),
            auto_index: true,
            store_max_age_minutes: 10,
            index_model: None,
        }
    }

    async fn create_test_repository(path: &Path, files: &[(&str, &str)]) {
        fs::create_dir_all(path).expect("create git repository directory");
        run_test_git(path, &["init", "--quiet", "-b", "main"]).await;
        run_test_git(path, &["config", "user.name", "Zession Tests"]).await;
        run_test_git(path, &["config", "user.email", "zession-tests@example.com"]).await;
        for (file_path, contents) in files {
            let destination = path.join(file_path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).expect("create nested repository path");
            }
            fs::write(destination, contents).expect("write repository fixture");
        }
        run_test_git(path, &["add", "--all"]).await;
        run_test_git(path, &["commit", "--quiet", "-m", "base commit"]).await;
    }

    async fn run_test_git(path: &Path, arguments: &[&str]) {
        let output = smol::process::Command::new("git")
            .current_dir(path)
            .args(arguments)
            .output()
            .await
            .expect("run git fixture command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn commit_for_store(store: &StoreRepo) -> String {
        git_stdout(
            Some(&store.cache),
            ["rev-parse", "refs/remotes/origin/main"],
        )
        .await
        .expect("resolve stored commit")
        .trim()
        .to_string()
    }
}
