use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, FixedOffset};
use fs::Fs;
use fuzzy_nucleo::{MaxResults, StringMatch, StringMatchCandidate, StringMatchOptions, match_strings};
use futures::StreamExt;
use gpui::{
    Action, AnyElement, App, AsyncApp, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, Global, Render, SharedString, Subscription, Task, TaskExt, Window, actions,
};
use picker::{Picker, PickerDelegate};
use project::ProjectGroupKey;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ui::{ListItem, prelude::*};
use util::{ResultExt, path_list::PathList};
use workspace::{
    ModalView, MultiWorkspace, OpenMode, Toast, Workspace,
    notifications::NotificationId,
};

actions!(
    zession,
    [
        /// Creates and opens a new session.
        NewSession
    ]
);

#[derive(Clone, Debug, Deserialize, PartialEq, JsonSchema, Action)]
#[serde(rename_all = "camelCase")]
pub struct AttachRepository {
    pub session_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionManifest {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    pub created: DateTime<FixedOffset>,
    #[serde(default)]
    pub last_opened: Option<DateTime<FixedOffset>>,
    #[serde(default)]
    pub repos: Vec<SessionRepo>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRepo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub remote: String,
    #[serde(default)]
    pub cache: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub base_ref: String,
    #[serde(default)]
    pub base_commit: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub dir: PathBuf,
    pub manifest: SessionManifest,
}

impl Session {
    pub fn display_title(&self) -> SharedString {
        self.manifest
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or(&self.manifest.id)
            .into()
    }
}

pub fn default_sessions_root() -> PathBuf {
    let session_home = env::var_os("SESSION_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".session"));
    expand_home(&session_home).join("sessions")
}

fn expand_home(path: &Path) -> PathBuf {
    let Some(path_string) = path.to_str() else {
        return path.to_path_buf();
    };
    if path_string == "~" {
        return paths::home_dir().to_path_buf();
    }
    if let Some(suffix) = path_string.strip_prefix("~/") {
        return paths::home_dir().join(suffix);
    }
    path.to_path_buf()
}

struct GlobalSessionStore(Entity<SessionStore>);

impl Global for GlobalSessionStore {}

pub struct SessionStore {
    fs: Arc<dyn Fs>,
    root: PathBuf,
    sessions: Vec<Session>,
    _watch: Task<()>,
}

impl SessionStore {
    pub fn init(fs: Arc<dyn Fs>, cx: &mut App) -> Entity<Self> {
        let store = cx.new(|cx| Self::new(fs, default_sessions_root(), cx));
        cx.set_global(GlobalSessionStore(store.clone()));
        store
    }

    pub fn new(fs: Arc<dyn Fs>, root: PathBuf, cx: &mut Context<Self>) -> Self {
        let watch = cx.spawn(async move |this, cx| {
            let Ok((watch_fs, watch_root)) =
                this.read_with(cx, |store, _| (store.fs.clone(), store.root.clone()))
            else {
                return;
            };

            loop {
                if !watch_fs.is_dir(&watch_root).await {
                    cx.background_executor()
                        .timer(Duration::from_secs(2))
                        .await;
                    continue;
                }

                let (root_events, root_watcher) = watch_fs
                    .watch(&watch_root, Duration::from_millis(100))
                    .await;
                let mut events = futures::stream::SelectAll::new();
                events.push(root_events);
                let mut _watchers = vec![root_watcher];

                let (_, session_directories) =
                    read_sessions(watch_fs.clone(), &watch_root).await;
                for session_directory in session_directories {
                    if watch_fs.is_dir(&session_directory).await {
                        let (session_events, watcher) = watch_fs
                            .watch(&session_directory, Duration::from_millis(100))
                            .await;
                        events.push(session_events);
                        _watchers.push(watcher);
                    }
                }

                let (sessions, _) = read_sessions(watch_fs.clone(), &watch_root).await;
                this.update(cx, |store, cx| store.replace_sessions(sessions, cx))
                    .log_err();

                if events.next().await.is_none() {
                    cx.background_executor()
                        .timer(Duration::from_secs(2))
                        .await;
                }
            }
        });

        Self {
            fs,
            root,
            sessions: Vec::new(),
            _watch: watch,
        }
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSessionStore>()
            .map(|store| store.0.clone())
    }

    pub fn set_global(store: Entity<Self>, cx: &mut App) {
        cx.set_global(GlobalSessionStore(store));
    }

    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn session_for_dir(&self, dir: &Path) -> Option<&Session> {
        self.sessions.iter().find(|session| session.dir == dir)
    }

    pub fn group_key(session: &Session) -> ProjectGroupKey {
        ProjectGroupKey::new(None, PathList::new(std::slice::from_ref(&session.dir)))
    }

    fn replace_sessions(&mut self, sessions: Vec<Session>, cx: &mut Context<Self>) {
        if self.sessions != sessions {
            self.sessions = sessions;
            cx.notify();
        }
    }
}

pub fn init(fs: Arc<dyn Fs>, cx: &mut App) {
    SessionStore::init(fs, cx);

    cx.on_action(|_: &NewSession, cx| {
        let Some(multi_workspace) = cx
            .active_window()
            .and_then(|window| window.downcast::<MultiWorkspace>())
        else {
            return;
        };
        let Some(active_workspace) = multi_workspace
            .read(cx)
            .ok()
            .map(|multi_workspace| multi_workspace.workspace().downgrade())
        else {
            return;
        };
        let command = cx.background_spawn(async move {
            smol::process::Command::new("session")
                .args(["new", "--no-agent"])
                .output()
                .await
        });
        cx.spawn(async move |cx| match command.await {
            Ok(output) if output.status.success() => {
                let output = String::from_utf8_lossy(&output.stdout);
                let Some(session_directory) = output
                    .lines()
                    .map(str::trim)
                    .rfind(|line| !line.is_empty())
                    .map(PathBuf::from)
                else {
                    show_command_error(
                        active_workspace,
                        "The session command did not return a session directory".to_string(),
                        cx,
                    );
                    return;
                };
                let path_list = PathList::new(std::slice::from_ref(&session_directory));
                let project_group = ProjectGroupKey::new(None, path_list.clone());
                if let Err(error) = multi_workspace.update(cx, |multi_workspace, window, cx| {
                    multi_workspace
                        .find_or_create_local_workspace(
                            path_list,
                            Some(project_group),
                            None,
                            OpenMode::Activate,
                            None,
                            window,
                            cx,
                        )
                        .detach_and_log_err(cx);
                }) {
                    log::debug!("Failed to open new session workspace: {error:#}");
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                show_command_error(
                    active_workspace,
                    command_output_error(output.status, &stderr),
                    cx,
                );
            }
            Err(error) => {
                show_command_error(
                    active_workspace,
                    format!("Failed to run session new: {error}"),
                    cx,
                );
            }
        })
        .detach();
    });

    cx.on_action(|action: &AttachRepository, cx| {
        let Some(multi_workspace) = cx
            .active_window()
            .and_then(|window| window.downcast::<MultiWorkspace>())
        else {
            return;
        };
        let session_directory = action.session_dir.clone();
        let Some(session) = SessionStore::try_global(cx).and_then(|store| {
            let store = store.read(cx);
            let dir = session_directory.clone().or_else(|| {
                multi_workspace
                    .read(cx)
                    .ok()?
                    .workspace()
                    .read(cx)
                    .root_paths(cx)
                    .into_iter()
                    .next()
                    .map(|path| path.to_path_buf())
            })?;
            store.session_for_dir(&dir).cloned()
        }) else {
            multi_workspace.update(cx, |multi_workspace, _, cx| {
                let workspace = multi_workspace.workspace().clone();
                workspace.update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(
                            NotificationId::unique::<AttachRepository>(),
                            "No session manifest found for this project",
                        )
                        .autohide(),
                        cx,
                    );
                });
            }).log_err();
            return;
        };

        let fs = <dyn Fs>::global(cx);
        let cache_root = default_sessions_root()
            .parent()
            .map(|root| root.join("store"))
            .unwrap_or_else(|| paths::home_dir().join(".session/store"));
        let Some(weak_workspace) = multi_workspace
            .read(cx)
            .ok()
            .map(|multi_workspace| multi_workspace.workspace().downgrade())
        else {
            return;
        };
        multi_workspace
            .update(cx, move |multi_workspace, window, cx| {
                let active_workspace = multi_workspace.workspace().clone();
                let weak_workspace = weak_workspace.clone();
                active_workspace.update(cx, move |active_workspace, cx| {
                    active_workspace.toggle_modal(window, cx, move |window, cx| {
                        AttachRepositoryModal::new(
                            session,
                            weak_workspace,
                            fs,
                            cache_root,
                            window,
                            cx,
                        )
                    });
                });
            })
            .log_err();
    });
}

fn show_command_error(
    workspace: gpui::WeakEntity<Workspace>,
    message: String,
    cx: &mut AsyncApp,
) {
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<SessionCommandError>(),
                    message,
                )
                .autohide(),
                cx,
            );
        })
        .log_err();
}

fn command_output_error(status: std::process::ExitStatus, stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("session command exited with {status}")
    } else {
        stderr.to_string()
    }
}

struct SessionCommandError;

struct CachedRepositoryPicker {
    workspace: gpui::WeakEntity<Workspace>,
    session_id: String,
    repositories: Vec<String>,
    filtered_repositories: Vec<StringMatch>,
    query_repository: Option<String>,
    selected_index: usize,
}

impl CachedRepositoryPicker {
    fn new(workspace: gpui::WeakEntity<Workspace>, session_id: String) -> Self {
        Self {
            workspace,
            session_id,
            repositories: Vec::new(),
            filtered_repositories: Vec::new(),
            query_repository: None,
            selected_index: 0,
        }
    }

    fn set_repositories(&mut self, repositories: Vec<String>) {
        self.repositories = repositories;
    }
}

impl PickerDelegate for CachedRepositoryPicker {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "session repositories"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search repositories…".into()
    }

    fn match_count(&self) -> usize {
        self.filtered_repositories.len() + usize::from(self.query_repository.is_some())
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_string();
        self.query_repository = (!query.is_empty()
            && !self
                .repositories
                .iter()
                .any(|repository| repository.eq_ignore_ascii_case(&query)))
        .then_some(query.clone());
        let candidates = self
            .repositories
            .iter()
            .enumerate()
            .map(|(index, repository)| StringMatchCandidate::new(index, repository))
            .collect::<Vec<_>>();
        self.filtered_repositories = if query.is_empty() {
            candidates
                .into_iter()
                .map(|candidate| StringMatch {
                    candidate_id: candidate.id,
                    score: 0.0,
                    positions: Vec::new(),
                    string: candidate.string,
                })
                .collect()
        } else {
            match_strings(
                &candidates,
                &query,
                StringMatchOptions {
                    max_results: MaxResults::limit::<100>(),
                    ..Default::default()
                },
            )
        };
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(
        &mut self,
        _secondary: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let query_row_offset = usize::from(self.query_repository.is_some());
        let repository = if self.query_repository.is_some() && self.selected_index == 0 {
            self.query_repository.clone()
        } else {
            self.filtered_repositories
                .get(self.selected_index.saturating_sub(query_row_offset))
                .and_then(|matched| self.repositories.get(matched.candidate_id))
                .cloned()
        };
        let Some(repository) = repository else {
            return;
        };
        let workspace = self.workspace.clone();
        let session_id = self.session_id.clone();
        let command = cx.background_spawn(async move {
            smol::process::Command::new("session")
                .args(["repos", "add", &repository, "--session", &session_id])
                .output()
                .await
        });
        cx.spawn(async move |_, cx| match command.await {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                show_command_error(
                    workspace,
                    command_output_error(output.status, &stderr),
                    cx,
                );
            }
            Err(error) => {
                show_command_error(
                    workspace,
                    format!("Failed to attach repository: {error}"),
                    cx,
                );
            }
        })
        .detach();
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        index: usize,
        _selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        if self.query_repository.is_some() && index == 0 {
            return Some(
                ListItem::new(index)
                    .child(Label::new(format!(
                        "Attach {}",
                        self.query_repository.as_ref()?
                    )))
                    .into_any_element(),
            );
        }
        let candidate_index = index.saturating_sub(usize::from(self.query_repository.is_some()));
        let matched = self.filtered_repositories.get(candidate_index)?;
        let repository = self.repositories.get(matched.candidate_id)?;
        Some(
            ListItem::new(index)
                .child(Label::new(repository.clone()))
                .into_any_element(),
        )
    }
}

struct AttachRepositoryModal {
    picker: Entity<Picker<CachedRepositoryPicker>>,
    _subscription: Subscription,
}

impl AttachRepositoryModal {
    fn new(
        session: Session,
        workspace: gpui::WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        cache_root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = CachedRepositoryPicker::new(workspace, session.manifest.id);
        let picker = cx.new(|cx| Picker::list(delegate, window, cx));
        let weak_picker = picker.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let repositories = read_cached_repositories(fs, cache_root).await;
            weak_picker
                .update_in(cx, move |picker, window, cx| {
                    picker.delegate.set_repositories(repositories);
                    let query = picker.query(cx);
                    picker.update_matches(query, window, cx);
                })
                .log_err();
        })
        .detach();
        let _subscription = cx.subscribe(
            &picker,
            |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent),
        );
        Self {
            picker,
            _subscription,
        }
    }
}

impl EventEmitter<DismissEvent> for AttachRepositoryModal {}

impl Focusable for AttachRepositoryModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for AttachRepositoryModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(38.)).h(rems(30.)).child(self.picker.clone())
    }
}

impl ModalView for AttachRepositoryModal {}

async fn read_cached_repositories(fs: Arc<dyn Fs>, cache_root: PathBuf) -> Vec<String> {
    let mut directories = vec![cache_root.clone()];
    let mut repositories = Vec::new();
    while let Some(directory) = directories.pop() {
        let Ok(mut entries) = fs.read_dir(&directory).await else {
            continue;
        };
        while let Some(entry) = entries.next().await {
            let Ok(path) = entry else {
                continue;
            };
            if !fs.is_dir(&path).await {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if let Some(repository_name) = name.strip_suffix(".git") {
                let Ok(relative_path) = path.strip_prefix(&cache_root) else {
                    continue;
                };
                let components = relative_path
                    .components()
                    .filter_map(|component| match component {
                        std::path::Component::Normal(name) => {
                            Some(name.to_string_lossy().into_owned())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if components.len() == 2 && components.first().is_some_and(|host| host == "local") {
                    repositories.push(repository_name.to_owned());
                } else if components.len() >= 3 {
                    if let Some(owner) = components.get(components.len() - 2) {
                        repositories.push(format!("{owner}/{repository_name}"));
                    }
                }
            } else {
                directories.push(path);
            }
        }
    }
    repositories.sort();
    repositories.dedup();
    repositories
}

async fn read_sessions(fs: Arc<dyn Fs>, root: &Path) -> (Vec<Session>, Vec<PathBuf>) {
    let mut directories = match fs.read_dir(root).await {
        Ok(directories) => directories,
        Err(error) => {
            log::debug!(
                "Failed to read session directory {}: {error:#}",
                root.display()
            );
            return (Vec::new(), Vec::new());
        }
    };

    let mut sessions = Vec::new();
    let mut session_directories = Vec::new();
    while let Some(entry) = directories.next().await {
        let Ok(dir) = entry else {
            continue;
        };
        if !fs.is_dir(&dir).await {
            continue;
        }
        session_directories.push(dir.clone());

        let manifest_path = dir.join("session.json");
        let contents = match fs.load(&manifest_path).await {
            Ok(contents) => contents,
            Err(error) => {
                log::debug!(
                    "Failed to read session manifest {}: {error:#}",
                    manifest_path.display()
                );
                continue;
            }
        };
        if let Some(session) = parse_session(dir, &contents) {
            sessions.push(session);
        }
    }

    sort_sessions(&mut sessions);
    (sessions, session_directories)
}

fn parse_session(dir: PathBuf, contents: &str) -> Option<Session> {
    match serde_json::from_str(contents) {
        Ok(manifest) => Some(Session { dir, manifest }),
        Err(error) => {
            log::debug!(
                "Failed to parse session manifest {}: {error:#}",
                dir.join("session.json").display()
            );
            None
        }
    }
}

fn sort_sessions(sessions: &mut [Session]) {
    sessions.sort_by(|left, right| {
        let left_opened = left
            .manifest
            .last_opened
            .as_ref()
            .unwrap_or(&left.manifest.created);
        let right_opened = right
            .manifest
            .last_opened
            .as_ref()
            .unwrap_or(&right.manifest.created);
        right_opened
            .cmp(left_opened)
            .then_with(|| left.manifest.id.cmp(&right.manifest.id))
    });
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use fs::{FakeFs, RemoveOptions};
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_unknown_fields_and_falls_back_to_id() {
        let manifest = serde_json::from_str::<SessionManifest>(
            r#"{"id":"demo-a","created":"2026-01-01T00:00:00Z","unknown":"ignored"}"#,
        )
        .expect("valid session manifest");
        let session = Session {
            dir: PathBuf::from("/sessions/demo-a"),
            manifest,
        };

        assert_eq!(session.display_title().as_ref(), "demo-a");
    }

    #[test]
    fn orders_by_last_opened_before_created() {
        let older = parse_session(
            PathBuf::from("/sessions/older"),
            r#"{"id":"older","created":"2026-02-01T00:00:00Z","lastOpened":"2026-02-02T00:00:00Z"}"#,
        )
        .expect("valid manifest");
        let newer = parse_session(
            PathBuf::from("/sessions/newer"),
            r#"{"id":"newer","created":"2026-01-01T00:00:00Z","lastOpened":"2026-02-03T00:00:00Z"}"#,
        )
        .expect("valid manifest");
        let created = parse_session(
            PathBuf::from("/sessions/created"),
            r#"{"id":"created","created":"2026-02-04T00:00:00Z"}"#,
        )
        .expect("valid manifest");
        let mut sessions = vec![older, newer, created];

        sort_sessions(&mut sessions);

        assert_eq!(
            sessions
                .iter()
                .map(|session| session.manifest.id.as_str())
                .collect::<Vec<_>>(),
            ["created", "newer", "older"]
        );
    }

    #[test]
    fn skips_invalid_json() {
        assert!(parse_session(PathBuf::from("/sessions/broken"), "{").is_none());
    }

    #[gpui::test]
    async fn lists_local_and_github_cached_repositories(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/store",
            serde_json::json!({
                "local": {
                    "foo.git": null
                },
                "github.com": {
                    "maxnorth": {
                        "nv.git": null
                    }
                }
            }),
        )
        .await;

        assert_eq!(
            read_cached_repositories(fs, PathBuf::from("/store")).await,
            ["foo", "maxnorth/nv"]
        );
    }

    #[gpui::test]
    async fn store_scans_and_watches_session_manifests(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/sessions",
            serde_json::json!({
                "first": {
                    "session.json": r#"{"id":"first","created":"2026-01-01T00:00:00Z"}"#
                },
                "second": {
                    "session.json": r#"{"id":"second","created":"2026-02-01T00:00:00Z"}"#
                }
            }),
        )
        .await;
        let store = cx.new(|cx| {
            SessionStore::new(fs.clone(), PathBuf::from("/sessions"), cx)
        });
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed_notifications = notifications.clone();
        cx.update(|cx| {
            cx.observe(&store, move |_, _| {
                observed_notifications.fetch_add(1, Ordering::SeqCst);
            })
            .detach();
        });
        cx.run_until_parked();

        assert_eq!(
            store.read_with(cx, |store, _| {
                store
                    .sessions()
                    .iter()
                    .map(|session| session.manifest.id.clone())
                    .collect::<Vec<_>>()
            }),
            ["second", "first"]
        );

        notifications.store(0, Ordering::SeqCst);
        fs.insert_tree(
            "/sessions/newest",
            serde_json::json!({
                "session.json": r#"{"id":"newest","created":"2026-03-01T00:00:00Z"}"#
            }),
        )
        .await;
        cx.run_until_parked();

        assert!(
            notifications.load(Ordering::SeqCst) > 0,
            "session changes should notify observers"
        );
        assert_eq!(
            store.read_with(cx, |store, _| {
                store
                    .sessions()
                    .first()
                    .map(|session| session.manifest.id.clone())
                    .unwrap_or_default()
            }),
            "newest"
        );

        notifications.store(0, Ordering::SeqCst);
        fs.insert_file(
            "/sessions/first/session.json",
            br#"{"id":"first","title":"First Updated","created":"2026-01-01T00:00:00Z"}"#.to_vec(),
        )
        .await;
        cx.run_until_parked();

        assert!(
            notifications.load(Ordering::SeqCst) > 0,
            "manifest changes should notify observers"
        );
        assert_eq!(
            store.read_with(cx, |store, _| {
                store
                    .session_for_dir(Path::new("/sessions/first"))
                    .map(Session::display_title)
            }),
            Some("First Updated".into())
        );

        fs.remove_file(
            Path::new("/sessions/first/session.json"),
            RemoveOptions::default(),
        )
        .await
        .expect("remove session manifest");
        fs.remove_dir(Path::new("/sessions/first"), RemoveOptions::default())
            .await
            .expect("remove session directory");
        cx.run_until_parked();

        assert!(
            store.read_with(cx, |store, _| {
                !store
                    .sessions()
                    .iter()
                    .any(|session| session.manifest.id == "first")
            })
        );
    }
}
