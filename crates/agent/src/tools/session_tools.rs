use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use agent_client_protocol::v2::schema as acp;
use futures::FutureExt as _;
use gpui::{App, AsyncApp, SharedString, Task};
use http_client::HttpClient;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings, update_settings_file_with_completion};

use crate::session_sandbox::session_home_for_roots;
use crate::{AgentTool, ToolCallEventStream, ToolInput, ToolPermissionContext};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionWorkspace {
    pub session_dir: PathBuf,
    pub session_id: String,
}

pub(crate) fn session_workspace_for_roots(
    session_home: &Path,
    roots: &[PathBuf],
) -> Option<SessionWorkspace> {
    if roots.len() != 1 || session_home_for_roots(session_home, roots).is_none() {
        return None;
    }
    let workspaces_root = session_home.join("workspaces");
    let root = &roots[0];
    let relative = match (root.canonicalize(), workspaces_root.canonicalize()) {
        (Ok(root), Ok(workspaces_root)) => root.strip_prefix(workspaces_root).ok()?.to_path_buf(),
        _ => root.strip_prefix(&workspaces_root).ok()?.to_path_buf(),
    };
    let mut components = relative.components();
    let Component::Normal(session_id) = components.next()? else {
        return None;
    };
    if components.next().is_some() {
        return None;
    }
    let session_id = session_id.to_str()?.to_owned();
    zession_core::read_manifest_at(session_home, &session_id).ok()?;
    Some(SessionWorkspace {
        session_dir: workspaces_root.join(&session_id),
        session_id,
    })
}

/// Find approved repositories relevant to a question, without attaching them.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SessionSearchReposInput {
    /// A question or topic to search for in the user's approved repositories.
    query: String,
}

pub struct SessionSearchReposTool {
    workspace: SessionWorkspace,
    http_client: Arc<dyn HttpClient>,
}

impl SessionSearchReposTool {
    pub(crate) fn new(workspace: SessionWorkspace, http_client: Arc<dyn HttpClient>) -> Self {
        Self {
            workspace,
            http_client,
        }
    }
}

impl AgentTool for SessionSearchReposTool {
    type Input = SessionSearchReposInput;
    type Output = String;
    const NAME: &'static str = "session_search_repos";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Search Session repositories".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let workspace = self.workspace.clone();
        let http_client = self.http_client.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            let settings = cx.update(|cx| zession_core::ZessionSettings::get_global(cx).clone());
            let hits = zession_core::search_repos(
                &input.query,
                Some(&workspace.session_id),
                &settings,
                http_client,
            )
            .await
            .map_err(|error| format!("Could not search Session repositories: {error:#}"))?;
            let hits = hits
                .into_iter()
                .map(|hit| {
                    serde_json::json!({
                        "repoId": hit.repo_id,
                        "description": hit.description,
                        "stored": hit.stored,
                        "indexed": hit.indexed,
                        "attachedInCurrentSession": hit.attached_in_current_session,
                        "excerpt": hit.excerpt,
                    })
                })
                .collect::<Vec<_>>();
            json_output(&hits)
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SessionRepoOperation {
    ListFiles,
    Show,
    Grep,
    Log,
}

/// Read code from an approved repository without attaching it. Attach only before editing, building, or testing.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SessionReadRepoInput {
    /// An approved repository identifier, such as `owner/repository`.
    repo: String,
    /// One fixed read operation: `list_files`, `show`, `grep`, or `log`.
    operation: SessionRepoOperation,
    /// A repository-relative path to show or filter by.
    #[serde(default)]
    path: Option<String>,
    /// A search pattern, required for `grep`.
    #[serde(default)]
    pattern: Option<String>,
    /// A Git ref; defaults to `HEAD`.
    #[serde(default)]
    git_ref: Option<String>,
}

pub struct SessionReadRepoTool {
    workspace: SessionWorkspace,
}

impl SessionReadRepoTool {
    pub(crate) fn new(workspace: SessionWorkspace) -> Self {
        Self { workspace }
    }
}

impl AgentTool for SessionReadRepoTool {
    type Input = SessionReadRepoInput;
    type Output = String;
    const NAME: &'static str = "session_read_repo";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        input.map_or_else(
            |_| "Read Session repository".into(),
            |input| format!("Read {:?} from {}", input.operation, input.repo).into(),
        )
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let _workspace = self.workspace.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            let settings = cx.update(|cx| zession_core::ZessionSettings::get_global(cx).clone());
            let operation = repo_operation(&input)?;
            let output = zession_core::read_repo(&input.repo, operation, &settings)
                .await
                .map_err(|error| format!("Could not read repository: {error:#}"))?;
            Ok(truncate_text(&output))
        })
    }
}

fn repo_operation(input: &SessionReadRepoInput) -> Result<zession_core::RepoOperation, String> {
    let git_ref = input.git_ref.clone();
    if git_ref
        .as_deref()
        .is_some_and(|value| value.starts_with('-'))
    {
        return Err("git_ref must not start with '-'".to_string());
    }
    if input.repo.trim().is_empty() || input.repo.starts_with('-') {
        return Err("repo must be a repository identifier".to_string());
    }
    match input.operation {
        SessionRepoOperation::ListFiles => Ok(zession_core::RepoOperation::ListFiles {
            git_ref,
            path: input.path.clone(),
        }),
        SessionRepoOperation::Show => {
            let path = input
                .path
                .clone()
                .filter(|path| !path.trim().is_empty())
                .ok_or_else(|| "path is required for show".to_string())?;
            Ok(zession_core::RepoOperation::Show { path, git_ref })
        }
        SessionRepoOperation::Grep => {
            let pattern = input
                .pattern
                .clone()
                .filter(|pattern| !pattern.trim().is_empty())
                .ok_or_else(|| "pattern is required for grep".to_string())?;
            if pattern.starts_with('-') {
                return Err("pattern must not start with '-'".to_string());
            }
            Ok(zession_core::RepoOperation::Grep {
                pattern,
                git_ref,
                path: input.path.clone(),
            })
        }
        SessionRepoOperation::Log => Ok(zession_core::RepoOperation::Log {
            git_ref,
            path: input.path.clone(),
        }),
    }
}

/// Attach a repository before editing, building, or testing it. Use `session_read_repo` for read-only questions.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SessionAttachRepoInput {
    /// The repository shorthand, URL, or local Git directory to attach.
    repo: String,
    /// Optional worktree directory name.
    #[serde(default)]
    name: Option<String>,
    /// Optional base branch or ref.
    #[serde(default)]
    base: Option<String>,
}

pub struct SessionAttachRepoTool {
    workspace: SessionWorkspace,
}

impl SessionAttachRepoTool {
    pub(crate) fn new(workspace: SessionWorkspace) -> Self {
        Self { workspace }
    }
}

impl AgentTool for SessionAttachRepoTool {
    type Input = SessionAttachRepoInput;
    type Output = String;
    const NAME: &'static str = "session_attach_repo";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Execute
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        input.map_or_else(
            |_| "Attach repository to Session".into(),
            |input| format!("Attach {} to Session", input.repo).into(),
        )
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let workspace = self.workspace.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            let spec = zession_core::RepoSpec::parse(&input.repo)
                .map_err(|error| format!("Invalid repository: {error:#}"))?;
            let permission = cx.update(|cx| {
                event_stream.authorize(
                    format!("Attach {} to Session", spec.repo_id),
                    ToolPermissionContext::new(Self::NAME, vec![spec.repo_id.clone()]),
                    cx,
                )
            });
            futures::select! {
                result = permission.fuse() => result.map_err(|error| error.to_string())?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err("Session repository attachment was cancelled.".to_string());
                }
            };
            let settings =
                cx.update(|cx| zession_core::ZessionSettings::get_global(cx).clone());
            match zession_core::attach_repo(
                &workspace.session_id,
                &spec,
                input.name.as_deref(),
                input.base.as_deref(),
                &settings,
            )
                .await
            {
                Ok(repository) => {
                    index_attached_repo(&spec, &settings, cx);
                    Ok(format!(
                        "Attached {} at {} on branch {}.",
                        repository.name,
                        repository.path.display(),
                        repository.branch
                    ))
                }
                Err(error)
                    if error
                        .downcast_ref::<zession_core::ZessionError>()
                        .is_some_and(|error| matches!(error, zession_core::ZessionError::NotApproved { .. })) =>
                {
                    let approval = cx.update(|cx| {
                        event_stream.authorize_always_prompt(
                            format!("Approve {} for Session?", spec.repo_id),
                            ToolPermissionContext::new(
                                Self::NAME,
                                vec![format!("zession.repos.allow {}", spec.repo_id)],
                            ),
                            cx,
                        )
                    });
                    futures::select! {
                        result = approval.fuse() => {
                            if result.is_err() {
                                return Err(format!("The user declined to approve {} for Session.", spec.repo_id));
                            }
                        },
                        _ = event_stream.cancelled_by_user().fuse() => {
                            return Err(format!("The user declined to approve {} for Session.", spec.repo_id));
                        }
                    };
                    let fs = cx.update(|cx| <dyn fs::Fs>::global(cx));
                    let repo_id = spec.repo_id.clone();
                    let settings_update = cx.update(|cx| {
                        update_settings_file_with_completion(fs, cx, move |settings, _| {
                            let repos = settings
                                .zession
                                .get_or_insert_default()
                                .repos
                                .get_or_insert_default();
                            let allow = repos.allow.get_or_insert_default();
                            add_repo_to_allow_list(allow, &repo_id);
                        })
                    });
                    settings_update
                        .await
                        .map_err(|error| {
                            format!("Could not persist Session repository approval: {error}")
                        })?
                        .map_err(|error| {
                            format!("Could not persist Session repository approval: {error:#}")
                        })?;
                    let mut settings =
                        cx.update(|cx| zession_core::ZessionSettings::get_global(cx).clone());
                    add_repo_to_allow_list(&mut settings.repos_allow, &spec.repo_id);
                    let repository = zession_core::attach_repo(
                        &workspace.session_id,
                        &spec,
                        input.name.as_deref(),
                        input.base.as_deref(),
                        &settings,
                    )
                    .await
                    .map_err(|error| format!("Could not attach repository after approval: {error:#}"))?;
                    index_attached_repo(&spec, &settings, cx);
                    Ok(format!(
                        "Attached {} at {} on branch {}.",
                        repository.name,
                        repository.path.display(),
                        repository.branch
                    ))
                }
                Err(error) => Err(format!("Could not attach repository: {error:#}")),
            }
        })
    }
}

fn index_attached_repo(
    spec: &zession_core::RepoSpec,
    settings: &zession_core::ZessionSettings,
    cx: &mut AsyncApp,
) {
    if !settings.auto_index {
        return;
    }
    let spec = spec.clone();
    let settings = settings.clone();
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
            log::debug!("could not index attached Session repository: {error:#}");
        }
    })
    .detach();
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SessionStatusInput {}

pub struct SessionStatusTool {
    workspace: SessionWorkspace,
}

impl SessionStatusTool {
    pub(crate) fn new(workspace: SessionWorkspace) -> Self {
        Self { workspace }
    }
}

impl AgentTool for SessionStatusTool {
    type Input = SessionStatusInput;
    type Output = String;
    const NAME: &'static str = "session_status";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Read
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Show Session status".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let session_id = self.workspace.session_id.clone();
        cx.spawn(async move |_cx| {
            input.recv().await.map_err(|error| error.to_string())?;
            let status = zession_core::session_status(&session_id)
                .await
                .map_err(|error| format!("Could not read Session status: {error:#}"))?;
            json_output(&status)
        })
    }
}

fn json_output(value: &impl Serialize) -> Result<String, String> {
    let output = serde_json::to_string(value).map_err(|error| error.to_string())?;
    Ok(truncate_text(&output))
}

fn add_repo_to_allow_list(allow: &mut Vec<String>, repo_id: &str) -> bool {
    if allow.iter().any(|entry| entry == repo_id) {
        false
    } else {
        allow.push(repo_id.to_owned());
        true
    }
}

fn truncate_text(text: &str) -> String {
    if text.len() <= 16 * 1024 {
        return text.to_owned();
    }
    let mut end = 16 * 1024;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [output truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_operations_have_fixed_arguments_and_validate_required_values() {
        let input = |operation, path, pattern, git_ref| SessionReadRepoInput {
            repo: "owner/repo".to_string(),
            operation,
            path,
            pattern,
            git_ref,
        };
        assert_eq!(
            repo_operation(&input(
                SessionRepoOperation::ListFiles,
                Some("src".into()),
                None,
                None
            )),
            Ok(zession_core::RepoOperation::ListFiles {
                git_ref: None,
                path: Some("src".into()),
            })
        );
        assert_eq!(
            repo_operation(&input(
                SessionRepoOperation::Show,
                Some("src/lib.rs".into()),
                None,
                None
            )),
            Ok(zession_core::RepoOperation::Show {
                path: "src/lib.rs".into(),
                git_ref: None,
            })
        );
        assert_eq!(
            repo_operation(&input(
                SessionRepoOperation::Grep,
                None,
                Some("needle".into()),
                Some("main".into())
            )),
            Ok(zession_core::RepoOperation::Grep {
                pattern: "needle".into(),
                git_ref: Some("main".into()),
                path: None,
            })
        );
        assert_eq!(
            repo_operation(&input(SessionRepoOperation::Log, None, None, None)),
            Ok(zession_core::RepoOperation::Log {
                git_ref: None,
                path: None,
            })
        );
        assert!(repo_operation(&input(SessionRepoOperation::Show, None, None, None)).is_err());
        assert!(repo_operation(&input(SessionRepoOperation::Grep, None, None, None)).is_err());
        assert!(
            repo_operation(&input(
                SessionRepoOperation::ListFiles,
                None,
                None,
                Some("-n".into())
            ))
            .is_err()
        );
        assert!(
            repo_operation(&input(
                SessionRepoOperation::Grep,
                None,
                Some("-e".into()),
                None
            ))
            .is_err()
        );
    }

    #[test]
    fn repo_approval_adds_only_the_exact_entry_once() {
        let mut allow = vec!["owner/*".to_string()];
        assert!(add_repo_to_allow_list(&mut allow, "owner/repo"));
        assert!(!add_repo_to_allow_list(&mut allow, "owner/repo"));
        assert_eq!(allow, vec!["owner/*".to_string(), "owner/repo".to_string()]);

        let mut settings = zession_core::ZessionSettings {
            repos_allow: Vec::new(),
            repos_access: Default::default(),
            auto_index: true,
            store_max_age_minutes: 10,
            index_model: None,
        };
        add_repo_to_allow_list(&mut settings.repos_allow, "owner/repo");
        assert!(zession_core::is_allowed(&settings, "owner/repo").expect("check approval"));
        assert!(!zession_core::is_allowed(&settings, "owner/other").expect("check other repo"));
    }

    #[test]
    fn session_registration_requires_a_single_manifest_workspace_root() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let home = temporary_directory.path();
        let session = zession_core::create_session_at(home, Some("first")).unwrap();
        assert!(session_workspace_for_roots(home, std::slice::from_ref(&session.dir)).is_some());
        assert!(session_workspace_for_roots(home, &[home.join("elsewhere")]).is_none());
        let duplicate_roots = [session.dir.clone(), session.dir];
        assert!(session_workspace_for_roots(home, &duplicate_roots).is_none());
    }
}
