use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use fs::Fs;
use futures::StreamExt;
use fuzzy_nucleo::{
    MaxResults, StringMatch, StringMatchCandidate, StringMatchOptions, match_strings,
};
use gpui::{
    Action, AnyElement, App, AsyncApp, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, Global, Render, SharedString, Subscription, Task, TaskExt, Window, actions,
};
use http_client::HttpClient;
use picker::{Picker, PickerDelegate};
use project::ProjectGroupKey;
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{Settings, update_settings_file_with_completion};
use ui::{ListItem, prelude::*};
use util::{ResultExt, path_list::PathList};
use workspace::{
    ModalView, MultiWorkspace, OpenMode, Toast, Workspace, notifications::NotificationId,
};
pub use zession_core::SessionManifest;
use zession_core::ZessionSettings;

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
    zession_core::sessions_dir()
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
                    cx.background_executor().timer(Duration::from_secs(2)).await;
                    continue;
                }

                let (root_events, _root_watcher) = watch_fs
                    .watch(&watch_root, Duration::from_millis(100))
                    .await;
                let mut events = root_events;
                let sessions = read_sessions(watch_fs.clone(), &watch_root).await;
                this.update(cx, |store, cx| store.replace_sessions(sessions, cx))
                    .log_err();

                if events.next().await.is_none() {
                    cx.background_executor().timer(Duration::from_secs(2)).await;
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
        let create_session = cx.background_spawn(async { zession_core::create_session(None) });
        cx.spawn(async move |cx| match create_session.await {
            Ok(session) => {
                let session_directory = session.dir;
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
            Err(error) => {
                show_session_error(
                    active_workspace,
                    format!("Failed to create Session: {error:#}"),
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
            multi_workspace
                .update(cx, |multi_workspace, _, cx| {
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
                })
                .log_err();
            return;
        };

        let fs = <dyn Fs>::global(cx);
        let cache_root = zession_core::store_dir();
        let settings = ZessionSettings::get_global(cx).clone();
        let Some(weak_workspace) = multi_workspace
            .read(cx)
            .ok()
            .map(|multi_workspace| multi_workspace.workspace().downgrade())
        else {
            return;
        };
        let Some(http_client) = multi_workspace.read(cx).ok().map(|multi_workspace| {
            let project = multi_workspace.workspace().read(cx).project().clone();
            project.read(cx).client().http_client()
        }) else {
            return;
        };
        let http_client: Arc<dyn HttpClient> = http_client;
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
                            settings,
                            http_client,
                            window,
                            cx,
                        )
                    });
                });
            })
            .log_err();
    });
}

fn show_session_error(workspace: gpui::WeakEntity<Workspace>, message: String, cx: &mut AsyncApp) {
    workspace
        .update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(NotificationId::unique::<SessionCommandError>(), message).autohide(),
                cx,
            );
        })
        .log_err();
}

struct SessionCommandError;

struct CachedRepositoryPicker {
    workspace: gpui::WeakEntity<Workspace>,
    session_id: String,
    settings: ZessionSettings,
    repositories: Vec<String>,
    filtered_repositories: Vec<StringMatch>,
    query_repository: Option<String>,
    selected_index: usize,
}

impl CachedRepositoryPicker {
    fn new(
        workspace: gpui::WeakEntity<Workspace>,
        session_id: String,
        settings: ZessionSettings,
    ) -> Self {
        Self {
            workspace,
            session_id,
            settings,
            repositories: Vec::new(),
            filtered_repositories: Vec::new(),
            query_repository: None,
            selected_index: 0,
        }
    }

    fn set_repositories(&mut self, repositories: Vec<String>) {
        self.repositories = repositories;
    }

    fn attach_repository(&self, spec: zession_core::RepoSpec, cx: &mut Context<Picker<Self>>) {
        let workspace = self.workspace.clone();
        let session_id = self.session_id.clone();
        let settings = self.settings.clone();
        let task = cx.spawn(async move |_, cx| {
            let result = zession_core::attach_repo(&session_id, &spec, None, None, &settings).await;
            if let Err(error) = result {
                return Err(format!("Failed to attach repository: {error:#}"));
            }
            start_session_indexing(spec, settings, cx);
            Ok(())
        });
        let weak_picker = cx.weak_entity();
        cx.spawn(async move |_, cx| match task.await {
            Ok(()) => {
                weak_picker
                    .update(cx, |_, cx| cx.emit(DismissEvent))
                    .log_err();
            }
            Err(error) => show_session_error(workspace, error, cx),
        })
        .detach();
    }

    fn show_error(&self, message: String, cx: &mut Context<Picker<Self>>) {
        let workspace = self.workspace.clone();
        cx.spawn(async move |_, cx| show_session_error(workspace, message, cx))
            .detach();
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

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
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
        let spec = match zession_core::RepoSpec::parse(&repository) {
            Ok(spec) => spec,
            Err(error) => {
                self.show_error(format!("Invalid repository: {error:#}"), cx);
                return;
            }
        };
        match zession_core::is_allowed(&self.settings, &spec.repo_id) {
            Ok(true) => self.attach_repository(spec, cx),
            Ok(false) => {
                let workspace = self.workspace.clone();
                let modal_workspace = workspace.clone();
                let session_id = self.session_id.clone();
                let file_system = <dyn Fs>::global(cx);
                let picker = cx.weak_entity();
                workspace
                    .update(cx, move |workspace, cx| {
                        workspace.toggle_modal(window, cx, move |_, cx| {
                            SessionApprovalModal::new(
                                session_id,
                                spec,
                                file_system,
                                modal_workspace,
                                picker,
                                cx,
                            )
                        });
                    })
                    .log_err();
            }
            Err(error) => self.show_error(
                format!("Could not check repository approval: {error:#}"),
                cx,
            ),
        }
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
        settings: ZessionSettings,
        http_client: Arc<dyn HttpClient>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate =
            CachedRepositoryPicker::new(workspace, session.manifest.id.clone(), settings.clone());
        let picker = cx.new(|cx| Picker::list(delegate, window, cx));
        let weak_picker = picker.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let mut repositories = match zession_core::search_repos(
                "",
                Some(&session.manifest.id),
                &settings,
                http_client,
            )
            .await
            {
                Ok(hits) => hits.into_iter().map(|hit| hit.repo_id).collect(),
                Err(error) => {
                    log::debug!("failed to load Session repository catalog: {error:#}");
                    Vec::new()
                }
            };
            repositories.extend(read_cached_repositories(fs, cache_root).await);
            repositories.sort();
            repositories.dedup();
            weak_picker
                .update_in(cx, move |picker, window, cx| {
                    picker.delegate.set_repositories(repositories);
                    let query = picker.query(cx);
                    picker.update_matches(query, window, cx);
                })
                .log_err();
        })
        .detach();
        let _subscription =
            cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent));
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
        v_flex()
            .w(rems(38.))
            .h(rems(30.))
            .child(self.picker.clone())
    }
}

impl ModalView for AttachRepositoryModal {}

struct SessionApprovalModal {
    session_id: String,
    spec: zession_core::RepoSpec,
    file_system: Arc<dyn Fs>,
    workspace: gpui::WeakEntity<Workspace>,
    picker: gpui::WeakEntity<Picker<CachedRepositoryPicker>>,
    focus_handle: FocusHandle,
}

impl SessionApprovalModal {
    fn new(
        session_id: String,
        spec: zession_core::RepoSpec,
        file_system: Arc<dyn Fs>,
        workspace: gpui::WeakEntity<Workspace>,
        picker: gpui::WeakEntity<Picker<CachedRepositoryPicker>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            session_id,
            spec,
            file_system,
            workspace,
            picker,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl EventEmitter<DismissEvent> for SessionApprovalModal {}

impl Focusable for SessionApprovalModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SessionApprovalModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(30.))
            .gap_3()
            .p_4()
            .child(Label::new(format!("Approve {}?", self.spec.repo_id)))
            .child(Label::new("Allow this repository to be used by Zession?"))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("cancel-session-repo-approval", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("approve-session-repo", "Approve").on_click(cx.listener(
                            |this, _, _, cx| {
                                let repo_id = this.spec.repo_id.clone();
                                let settings_repo_id = repo_id.clone();
                                let file_system = this.file_system.clone();
                                let session_id = this.session_id.clone();
                                let spec = this.spec.clone();
                                let workspace = this.workspace.clone();
                                let picker = this.picker.clone();
                                let settings_update = update_settings_file_with_completion(
                                    file_system,
                                    cx,
                                    move |settings, _| {
                                        let repos = settings
                                            .zession
                                            .get_or_insert_default()
                                            .repos
                                            .get_or_insert_default();
                                        let allow = repos.allow.get_or_insert_default();
                                        if !allow.iter().any(|entry| entry == &settings_repo_id) {
                                            allow.push(settings_repo_id);
                                        }
                                    },
                                );
                                let mut settings = ZessionSettings::get_global(cx).clone();
                                if !settings.repos_allow.contains(&repo_id) {
                                    settings.repos_allow.push(repo_id);
                                }
                                cx.spawn(async move |_, cx| {
                                    match settings_update.await {
                                        Ok(Ok(())) => {}
                                        Ok(Err(error)) => {
                                            show_session_error(
                                                workspace,
                                                format!(
                                                    "Could not save repository approval: {error:#}"
                                                ),
                                                cx,
                                            );
                                            return;
                                        }
                                        Err(error) => {
                                            show_session_error(
                                                workspace,
                                                format!(
                                                    "Could not save repository approval: {error}"
                                                ),
                                                cx,
                                            );
                                            return;
                                        }
                                    }
                                    match zession_core::attach_repo(
                                        &session_id,
                                        &spec,
                                        None,
                                        None,
                                        &settings,
                                    )
                                    .await
                                    {
                                        Ok(_) => {
                                            start_session_indexing(spec, settings, cx);
                                            picker
                                                .update(cx, |_, cx| cx.emit(DismissEvent))
                                                .log_err();
                                        }
                                        Err(error) => {
                                            show_session_error(
                                                workspace,
                                                format!("Failed to attach repository: {error:#}"),
                                                cx,
                                            );
                                        }
                                    }
                                })
                                .detach();
                                cx.emit(DismissEvent);
                            },
                        )),
                    ),
            )
    }
}

impl ModalView for SessionApprovalModal {}

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

fn start_session_indexing(
    spec: zession_core::RepoSpec,
    settings: ZessionSettings,
    cx: &mut AsyncApp,
) {
    if !settings.auto_index {
        return;
    }
    cx.spawn(async move |cx| {
        let result = async {
            let store = zession_core::ensure_store_fresh(
                &spec,
                Duration::from_secs(settings.store_max_age_minutes.saturating_mul(60)),
                false,
            )
            .await?;
            zession_core::index_repo_if_enabled(&settings, &store, cx).await
        }
        .await;
        if let Err(error) = result {
            log::debug!("failed to index attached repository: {error:#}");
        }
    })
    .detach();
}

async fn read_sessions(fs: Arc<dyn Fs>, root: &Path) -> Vec<Session> {
    let mut directories = match fs.read_dir(root).await {
        Ok(directories) => directories,
        Err(error) => {
            log::debug!(
                "Failed to read session directory {}: {error:#}",
                root.display()
            );
            return Vec::new();
        }
    };

    let mut sessions = Vec::new();
    while let Some(entry) = directories.next().await {
        let Ok(manifest_path) = entry else {
            continue;
        };
        if fs.is_dir(&manifest_path).await
            || manifest_path
                .extension()
                .and_then(|extension| extension.to_str())
                != Some("json")
        {
            continue;
        }
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
        let Ok(manifest) = serde_json::from_str::<SessionManifest>(&contents) else {
            log::debug!(
                "Failed to parse Session manifest {}",
                manifest_path.display()
            );
            continue;
        };
        if let Some(workspaces_root) = root.parent().map(|home| home.join("workspaces")) {
            sessions.push(Session {
                dir: workspaces_root.join(&manifest.id),
                manifest,
            });
        }
    }

    sort_sessions(&mut sessions);
    sessions
}

#[cfg(test)]
fn parse_session(dir: PathBuf, contents: &str) -> Option<Session> {
    match serde_json::from_str(contents) {
        Ok(manifest) => Some(Session { dir, manifest }),
        Err(error) => {
            log::debug!(
                "Failed to parse Session manifest {}: {error:#}",
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
            r#"{"version":1,"id":"demo-a","created":"2026-01-01T00:00:00Z","unknown":"ignored"}"#,
        )
        .expect("valid session manifest");
        let session = Session {
            dir: PathBuf::from("/zession/workspaces/demo-a"),
            manifest,
        };

        assert_eq!(session.display_title().as_ref(), "demo-a");
    }

    #[test]
    fn orders_by_last_opened_before_created() {
        let older = parse_session(
            PathBuf::from("/zession/workspaces/older"),
            r#"{"version":1,"id":"older","created":"2026-02-01T00:00:00Z","lastOpened":"2026-02-02T00:00:00Z"}"#,
        )
        .expect("valid manifest");
        let newer = parse_session(
            PathBuf::from("/zession/workspaces/newer"),
            r#"{"version":1,"id":"newer","created":"2026-01-01T00:00:00Z","lastOpened":"2026-02-03T00:00:00Z"}"#,
        )
        .expect("valid manifest");
        let created = parse_session(
            PathBuf::from("/zession/workspaces/created"),
            r#"{"version":1,"id":"created","created":"2026-02-04T00:00:00Z"}"#,
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
        assert!(parse_session(PathBuf::from("/zession/workspaces/broken"), "{").is_none());
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
            "/zession",
            serde_json::json!({
                "sessions": {
                    "first.json": r#"{"version":1,"id":"first","created":"2026-01-01T00:00:00Z"}"#,
                    "second.json": r#"{"version":1,"id":"second","created":"2026-02-01T00:00:00Z"}"#
                },
                "workspaces": {
                    "first": {},
                    "second": {}
                }
            }),
        )
        .await;
        let store =
            cx.new(|cx| SessionStore::new(fs.clone(), PathBuf::from("/zession/sessions"), cx));
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
        fs.insert_file(
            "/zession/sessions/newest.json",
            br#"{"version":1,"id":"newest","created":"2026-03-01T00:00:00Z"}"#.to_vec(),
        )
        .await;
        cx.run_until_parked();
        assert!(notifications.load(Ordering::SeqCst) > 0);
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
            "/zession/sessions/first.json",
            br#"{"version":1,"id":"first","title":"First Updated","created":"2026-01-01T00:00:00Z"}"#.to_vec(),
        )
        .await;
        cx.run_until_parked();
        assert!(notifications.load(Ordering::SeqCst) > 0);
        assert_eq!(
            store.read_with(cx, |store, _| {
                store
                    .session_for_dir(Path::new("/zession/workspaces/first"))
                    .map(Session::display_title)
            }),
            Some("First Updated".into())
        );

        fs.remove_file(
            Path::new("/zession/sessions/first.json"),
            RemoveOptions::default(),
        )
        .await
        .expect("remove session manifest");
        cx.run_until_parked();
        assert!(store.read_with(cx, |store, _| {
            !store
                .sessions()
                .iter()
                .any(|session| session.manifest.id == "first")
        }));
    }
}
