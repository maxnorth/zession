use std::{
    path::{Component, Path, PathBuf},
    process::Output,
    sync::Arc,
};

use agent_client_protocol::v2::schema as acp;
use futures::FutureExt as _;
use gpui::{App, AppContext as _, AsyncApp, SharedString, Task};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use util::command::new_command;

use crate::session_sandbox::session_home_for_roots;
use crate::{AgentTool, ToolCallEventStream, ToolInput, ToolPermissionContext};

const OUTPUT_LIMIT: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionWorkspace {
    session_dir: PathBuf,
    session_id: String,
}

pub(crate) fn session_workspace_for_roots(
    session_home: &Path,
    roots: &[PathBuf],
) -> Option<SessionWorkspace> {
    let session_home = session_home_for_roots(session_home, roots)?;
    let sessions_root = session_home.join("sessions");
    let canonical_sessions_root = sessions_root.canonicalize().ok();

    roots.iter().find_map(|root| {
        let relative_root = match (root.canonicalize(), canonical_sessions_root.as_deref()) {
            (Ok(canonical_root), Some(canonical_sessions_root)) => canonical_root
                .strip_prefix(canonical_sessions_root)
                .ok()?
                .to_path_buf(),
            _ => root.strip_prefix(&sessions_root).ok()?.to_path_buf(),
        };
        let session_id = relative_root
            .components()
            .find_map(|component| match component {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })?
            .to_owned();

        Some(SessionWorkspace {
            session_dir: sessions_root.join(&session_id),
            session_id,
        })
    })
}

/// Find Session repositories that are relevant to a natural-language question.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SessionDiscoverReposInput {
    /// The question or topic to search for in the user's approved repositories.
    question: String,
}

pub struct SessionDiscoverReposTool {
    workspace: SessionWorkspace,
}

impl SessionDiscoverReposTool {
    pub(crate) fn new(workspace: SessionWorkspace) -> Self {
        Self { workspace }
    }
}

impl AgentTool for SessionDiscoverReposTool {
    type Input = SessionDiscoverReposInput;
    type Output = String;

    const NAME: &'static str = "session_discover_repos";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "Discover Session repositories".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let workspace = self.workspace.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            let arguments = vec![
                "repos".to_string(),
                "discover".to_string(),
                "--json".to_string(),
                "--quiet".to_string(),
                input.question,
            ];
            let output = background_session_command(cx, workspace, arguments).await?;
            command_stdout(output)
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

/// Read approved repository code without attaching it. Attach a repository only before editing, building, or testing.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SessionReadRepoInput {
    /// The approved repository identifier, such as `owner/repository`.
    repo: String,
    /// The fixed read operation to perform.
    operation: SessionRepoOperation,
    /// A repository-relative path for filtering or showing files.
    #[serde(default)]
    path: Option<String>,
    /// A literal search pattern required for the `grep` operation.
    #[serde(default)]
    pattern: Option<String>,
    /// A Git ref to read; defaults to `HEAD`.
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

fn read_repo_args(input: &SessionReadRepoInput) -> Result<Vec<String>, String> {
    if input.repo.trim().is_empty() || input.repo.starts_with('-') {
        return Err("repo must be a repository identifier".to_string());
    }

    let git_ref = input.git_ref.as_deref().unwrap_or("HEAD");
    if git_ref.trim().is_empty() || git_ref.starts_with('-') {
        return Err("git_ref must be a non-option Git ref".to_string());
    }

    let mut arguments = vec!["repos".to_string(), "git".to_string(), input.repo.clone()];
    match input.operation {
        SessionRepoOperation::ListFiles => {
            arguments.extend([
                "ls-tree".to_string(),
                "-r".to_string(),
                "--name-only".to_string(),
                git_ref.to_string(),
            ]);
            if let Some(path) = non_empty(input.path.as_deref()) {
                arguments.extend(["--".to_string(), path.to_string()]);
            }
        }
        SessionRepoOperation::Show => {
            let path = required_path(input.path.as_deref())?;
            arguments.extend(["show".to_string(), format!("{git_ref}:{path}")]);
        }
        SessionRepoOperation::Grep => {
            let pattern = non_empty(input.pattern.as_deref())
                .ok_or_else(|| "pattern is required for grep".to_string())?;
            if pattern.starts_with('-') {
                return Err("pattern must not start with '-'".to_string());
            }
            arguments.extend([
                "grep".to_string(),
                "-n".to_string(),
                "-e".to_string(),
                pattern.to_string(),
                git_ref.to_string(),
            ]);
            if let Some(path) = non_empty(input.path.as_deref()) {
                arguments.extend(["--".to_string(), path.to_string()]);
            }
        }
        SessionRepoOperation::Log => {
            arguments.extend([
                "log".to_string(),
                "--oneline".to_string(),
                "-n".to_string(),
                "30".to_string(),
                git_ref.to_string(),
            ]);
            if let Some(path) = non_empty(input.path.as_deref()) {
                arguments.extend(["--".to_string(), path.to_string()]);
            }
        }
    }
    Ok(arguments)
}

fn required_path(path: Option<&str>) -> Result<&str, String> {
    non_empty(path).ok_or_else(|| "path is required for show".to_string())
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.trim().is_empty())
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
        match input {
            Ok(input) => format!(
                "Read {} from {}",
                operation_name(input.operation),
                input.repo
            )
            .into(),
            Err(_) => "Read Session repository".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let workspace = self.workspace.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|error| error.to_string())?;
            let arguments = read_repo_args(&input)?;
            let output = background_session_command(cx, workspace, arguments).await?;
            command_stdout(output)
        })
    }
}

fn operation_name(operation: SessionRepoOperation) -> &'static str {
    match operation {
        SessionRepoOperation::ListFiles => "files",
        SessionRepoOperation::Show => "file",
        SessionRepoOperation::Grep => "matches",
        SessionRepoOperation::Log => "history",
    }
}

/// Attach a repository before editing, building, or testing it. Use `session_read_repo` for questions that only need code reading.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SessionAttachRepoInput {
    /// The repository identifier to attach, such as `owner/repository`.
    repo: String,
    /// Optional worktree directory name.
    #[serde(default)]
    name: Option<String>,
    /// Optional base branch for the Session worktree.
    #[serde(default)]
    base: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddRepoSuccess {
    session: String,
    repos: Vec<AttachedRepository>,
}

#[derive(Debug, Deserialize)]
struct AttachedRepository {
    name: String,
    path: String,
    branch: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AddRepoResponse {
    Success(AddRepoSuccess),
    Error(AddRepoFailure),
}

#[derive(Debug, Deserialize)]
struct AddRepoFailure {
    error: String,
    code: String,
    repo: String,
}

fn parse_add_json(output: &str) -> Result<AddRepoSuccess, AddRepoFailure> {
    match serde_json::from_str::<AddRepoResponse>(output) {
        Ok(AddRepoResponse::Success(success)) => Ok(success),
        Ok(AddRepoResponse::Error(error)) => Err(error),
        Err(error) => Err(AddRepoFailure {
            error: format!("could not parse `session repos add --json` output: {error}"),
            code: "error".to_string(),
            repo: String::new(),
        }),
    }
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
        match input {
            Ok(input) => format!("Attach {} to Session", input.repo).into(),
            Err(_) => "Attach repository to Session".into(),
        }
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
            let command_description = attach_command_description(&input);
            let authorization = cx.update(|cx| {
                event_stream.authorize(
                    format!("Attach {} to Session", input.repo),
                    ToolPermissionContext::new(Self::NAME, vec![command_description]),
                    cx,
                )
            });
            futures::select! {
                result = authorization.fuse() => result.map_err(|error| error.to_string())?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err("Session repository attachment cancelled by the user.".to_string());
                }
            };

            let output = background_session_command(
                cx,
                workspace.clone(),
                attach_repo_args(&input, &workspace.session_id),
            )
            .await?;
            let attached = match add_command_result(output) {
                Ok(attached) => attached,
                Err(failure) if failure.code == "repo_not_approved" => {
                    let repo = if failure.repo.is_empty() {
                        input.repo.clone()
                    } else {
                        failure.repo
                    };
                    let approval = cx.update(|cx| {
                        event_stream.authorize_always_prompt(
                            format!("Approve {repo} for Session?"),
                            ToolPermissionContext::new(
                                Self::NAME,
                                vec![format!("session repos allow {repo} --yes")],
                            ),
                            cx,
                        )
                    });
                    futures::select! {
                        result = approval.fuse() => {
                            if result.is_err() {
                                return Err(format!("The user declined to approve {repo} for Session."));
                            }
                        },
                        _ = event_stream.cancelled_by_user().fuse() => {
                            return Err(format!("The user declined to approve {repo} for Session."));
                        }
                    };

                    let allow_output = background_session_command(
                        cx,
                        workspace.clone(),
                        vec![
                            "repos".to_string(),
                            "allow".to_string(),
                            repo.clone(),
                            "--yes".to_string(),
                        ],
                    )
                    .await?;
                    if !allow_output.status.success() {
                        return Err(command_failure(&allow_output));
                    }

                    let retry_output = background_session_command(
                        cx,
                        workspace.clone(),
                        attach_repo_args(&input, &workspace.session_id),
                    )
                    .await?;
                    add_command_result(retry_output).map_err(|failure| {
                        truncate_output(
                            format!("Could not attach {repo} after approval: {}", failure.error)
                                .as_bytes(),
                        )
                    })?
                }
                Err(failure) => return Err(truncate_output(failure.error.as_bytes())),
            };

            let repository = attached
                .repos
                .first()
                .ok_or_else(|| "Session attached no repositories.".to_string())?;
            if attached.session.is_empty() {
                return Err("Session CLI returned an empty Session path.".to_string());
            }
            Ok(format!(
                "Attached {} at {} on branch {}.",
                repository.name, repository.path, repository.branch
            ))
        })
    }
}

fn attach_command_description(input: &SessionAttachRepoInput) -> String {
    let mut command = format!("session repos add {} --json", input.repo);
    if let Some(name) = &input.name {
        command.push_str(&format!(" --as {name}"));
    }
    if let Some(base) = &input.base {
        command.push_str(&format!(" --branch {base}"));
    }
    command
}

fn attach_repo_args(input: &SessionAttachRepoInput, session_id: &str) -> Vec<String> {
    let mut arguments = vec![
        "repos".to_string(),
        "add".to_string(),
        input.repo.clone(),
        "--json".to_string(),
    ];
    if let Some(name) = &input.name {
        arguments.extend(["--as".to_string(), name.clone()]);
    }
    if let Some(base) = &input.base {
        arguments.extend(["--branch".to_string(), base.clone()]);
    }
    arguments.extend(["--session".to_string(), session_id.to_string()]);
    arguments
}

fn add_command_result(output: Output) -> Result<AddRepoSuccess, AddRepoFailure> {
    match parse_add_json(&String::from_utf8_lossy(&output.stdout)) {
        Ok(success) if output.status.success() => Ok(success),
        Ok(_) => Err(AddRepoFailure {
            error: command_failure(&output),
            code: "error".to_string(),
            repo: String::new(),
        }),
        Err(failure) => {
            if !output.status.success()
                && failure.code == "error"
                && failure.repo.is_empty()
                && !String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .starts_with('{')
            {
                return Err(AddRepoFailure {
                    error: command_failure(&output),
                    code: "error".to_string(),
                    repo: String::new(),
                });
            }
            Err(failure)
        }
    }
}

/// Return the current Session id, title, path, and attached repository status.
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
        "Read Session status".into()
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let workspace = self.workspace.clone();
        cx.spawn(async move |cx| {
            input.recv().await.map_err(|error| error.to_string())?;
            let output = background_session_command(
                cx,
                workspace,
                vec!["status".to_string(), "--json".to_string()],
            )
            .await?;
            command_stdout(output)
        })
    }
}

async fn background_session_command(
    cx: &mut AsyncApp,
    workspace: SessionWorkspace,
    arguments: Vec<String>,
) -> Result<Output, String> {
    cx.background_spawn(async move { run_session_command(workspace, arguments).await })
        .await
}

async fn run_session_command(
    workspace: SessionWorkspace,
    arguments: Vec<String>,
) -> Result<Output, String> {
    let mut command = new_command("session");
    command.current_dir(&workspace.session_dir).args(arguments);
    command
        .output()
        .await
        .map_err(|error| format!("could not run the Session CLI: {error}"))
}

fn command_stdout(output: Output) -> Result<String, String> {
    if output.status.success() {
        Ok(truncate_output(&output.stdout))
    } else {
        Err(command_failure(&output))
    }
}

fn command_failure(output: &Output) -> String {
    let stderr = truncate_output(&output.stderr);
    if stderr.trim().is_empty() {
        format!("Session CLI exited with {}", output.status)
    } else {
        format!("Session CLI exited with {}: {stderr}", output.status)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn input(operation: SessionRepoOperation) -> SessionReadRepoInput {
        SessionReadRepoInput {
            repo: "maxnorth/session".to_string(),
            operation,
            path: None,
            pattern: None,
            git_ref: None,
        }
    }

    #[test]
    fn read_repo_args_maps_list_files() {
        let mut input = input(SessionRepoOperation::ListFiles);
        assert_eq!(
            read_repo_args(&input).expect("list_files arguments"),
            [
                "repos",
                "git",
                "maxnorth/session",
                "ls-tree",
                "-r",
                "--name-only",
                "HEAD",
            ]
        );

        input.path = Some("src".to_string());
        assert_eq!(
            read_repo_args(&input).expect("path-filtered list_files arguments"),
            [
                "repos",
                "git",
                "maxnorth/session",
                "ls-tree",
                "-r",
                "--name-only",
                "HEAD",
                "--",
                "src",
            ]
        );
    }

    #[test]
    fn read_repo_args_maps_show() {
        let mut input = input(SessionRepoOperation::Show);
        input.path = Some("main.go".to_string());
        assert_eq!(
            read_repo_args(&input).expect("show arguments"),
            ["repos", "git", "maxnorth/session", "show", "HEAD:main.go"]
        );
    }

    #[test]
    fn read_repo_args_maps_grep() {
        let mut input = input(SessionRepoOperation::Grep);
        input.pattern = Some("resolveSession".to_string());
        input.git_ref = Some("main".to_string());
        input.path = Some("main.go".to_string());
        assert_eq!(
            read_repo_args(&input).expect("grep arguments"),
            [
                "repos",
                "git",
                "maxnorth/session",
                "grep",
                "-n",
                "-e",
                "resolveSession",
                "main",
                "--",
                "main.go",
            ]
        );
    }

    #[test]
    fn read_repo_args_maps_log() {
        let mut input = input(SessionRepoOperation::Log);
        input.path = Some("main.go".to_string());
        assert_eq!(
            read_repo_args(&input).expect("log arguments"),
            [
                "repos",
                "git",
                "maxnorth/session",
                "log",
                "--oneline",
                "-n",
                "30",
                "HEAD",
                "--",
                "main.go",
            ]
        );
    }

    #[test]
    fn read_repo_args_rejects_missing_fields_and_option_like_values() {
        let mut input = input(SessionRepoOperation::Show);
        assert!(read_repo_args(&input).is_err());

        input.operation = SessionRepoOperation::Grep;
        assert!(read_repo_args(&input).is_err());
        input.pattern = Some("-n".to_string());
        assert!(read_repo_args(&input).is_err());

        input.pattern = Some("resolveSession".to_string());
        input.git_ref = Some("--all".to_string());
        assert!(read_repo_args(&input).is_err());
    }

    #[test]
    fn session_workspace_requires_a_session_root() {
        let temporary_directory = tempfile::tempdir().expect("temporary directory");
        let session_home = temporary_directory.path().join(".session");
        let session_dir = session_home.join("sessions/first");
        std::fs::create_dir_all(&session_dir).expect("create Session directory");
        let repository_root = session_dir.join("repo");
        std::fs::create_dir_all(&repository_root).expect("create repository root");
        let sessions_root = session_home.join("sessions");
        let outside_root = temporary_directory.path().join("project");
        std::fs::create_dir_all(&outside_root).expect("create outside root");

        let workspace = session_workspace_for_roots(&session_home, &[repository_root])
            .expect("Session root should register");
        assert_eq!(workspace.session_dir, session_dir);
        assert_eq!(workspace.session_id, "first");
        assert!(session_workspace_for_roots(&session_home, &[outside_root]).is_none());
        assert!(session_workspace_for_roots(&session_home, &[sessions_root]).is_none());
    }

    #[test]
    fn parses_add_json_success() {
        let result = parse_add_json(
            r#"{"session":"/home/user/.session/sessions/first","repos":[{"name":"session","path":"/home/user/.session/sessions/first/session","branch":"session/first","remote":"https://github.com/maxnorth/session.git","baseRef":"origin/main"}]}"#,
        )
        .expect("success response");
        assert_eq!(result.session, "/home/user/.session/sessions/first");
        assert_eq!(result.repos.len(), 1);
        assert_eq!(result.repos[0].name, "session");
        assert_eq!(
            result.repos[0].path,
            "/home/user/.session/sessions/first/session"
        );
        assert_eq!(result.repos[0].branch, "session/first");
    }

    #[test]
    fn parses_add_json_repo_not_approved_error() {
        let error = parse_add_json(
            r#"{"error":"maxnorth/session is not from an approved owner","code":"repo_not_approved","repo":"maxnorth/session"}"#,
        )
        .expect_err("approval error response");
        assert_eq!(error.code, "repo_not_approved");
        assert_eq!(error.repo, "maxnorth/session");
        assert!(error.error.contains("not from an approved owner"));
    }
}
