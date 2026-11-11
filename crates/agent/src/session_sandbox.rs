use http_proxy::HostPattern;
use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs,
    path::{Component, Path, PathBuf},
};
use util::command::new_command;

pub(crate) fn session_home() -> PathBuf {
    let session_home = env::var_os("SESSION_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".session"));
    expand_home(&session_home)
}

pub(crate) fn session_home_for_roots(session_home: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let sessions_root = session_home.join("sessions");
    let canonical_sessions_root = sessions_root.canonicalize().ok();

    roots
        .iter()
        .any(
            |root| match (root.canonicalize(), canonical_sessions_root.as_deref()) {
                (Ok(canonical_root), Some(canonical_sessions_root)) => {
                    path_has_session_id(&canonical_root, canonical_sessions_root)
                }
                _ => path_has_session_id(root, &sessions_root),
            },
        )
        .then(|| session_home.to_path_buf())
}

pub(crate) async fn session_network_hosts(session_home: &Path) -> Vec<String> {
    let git_environment = env::vars_os().collect::<Vec<_>>();
    session_network_hosts_with_env(session_home, &git_environment).await
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

fn path_has_session_id(root: &Path, sessions_root: &Path) -> bool {
    let Ok(relative_root) = root.strip_prefix(sessions_root) else {
        return false;
    };
    matches!(
        relative_root.components().next(),
        Some(Component::Normal(_))
    )
}

async fn session_network_hosts_with_env(
    session_home: &Path,
    git_environment: &[(OsString, OsString)],
) -> Vec<String> {
    let mut store_hosts = BTreeSet::new();
    let store_directory = session_home.join("store");
    match fs::read_dir(&store_directory) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => match entry.file_type() {
                        Ok(file_type) if file_type.is_dir() => {
                            match entry.file_name().into_string() {
                                Ok(host) if host != "local" && host != "github.com" => {
                                    store_hosts.insert(host);
                                }
                                Ok(_) => {}
                                Err(_) => {
                                    log::debug!("ignoring non-UTF-8 Session store directory name")
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(error) => log::debug!(
                            "could not inspect Session store entry {}: {error}",
                            entry.path().display()
                        ),
                    },
                    Err(error) => log::debug!("could not read Session store entry: {error}"),
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => log::debug!(
            "could not read Session store directory {}: {error}",
            store_directory.display()
        ),
    }

    let mut base_hosts = vec!["github.com".to_string()];
    base_hosts.extend(store_hosts);
    let mut hosts = Vec::new();
    for base_host in &base_hosts {
        insert_valid_host(&mut hosts, base_host);
    }
    for base_host in base_hosts {
        if let Some(rewritten_host) = rewritten_host(&base_host, git_environment).await {
            insert_valid_host(&mut hosts, &rewritten_host);
        }
    }
    hosts
}

fn insert_valid_host(hosts: &mut Vec<String>, host: &str) {
    match HostPattern::parse(host) {
        Ok(host_pattern) => {
            let host = host_pattern.to_string();
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        Err(error) => log::debug!("ignoring invalid Session sandbox host '{host}': {error}"),
    }
}

async fn rewritten_host(host: &str, git_environment: &[(OsString, OsString)]) -> Option<String> {
    let mut command = new_command("git");
    command
        .args(["ls-remote", "--get-url"])
        .arg(format!("https://{host}/"))
        .env_clear()
        .envs(
            git_environment
                .iter()
                .map(|(key, value)| (key.as_os_str(), value.as_os_str())),
        );
    let output = match command.output().await {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            log::debug!("git ls-remote --get-url exited with {}", output.status);
            return None;
        }
        Err(error) => {
            log::debug!("could not run git ls-remote --get-url: {error}");
            return None;
        }
    };
    let rewritten_url = String::from_utf8_lossy(&output.stdout);
    let parsed_url = match url::Url::parse(rewritten_url.trim()) {
        Ok(url) => url,
        Err(_) => {
            log::debug!("git ls-remote --get-url returned an invalid URL");
            return None;
        }
    };
    let Some(host) = parsed_url.host_str() else {
        log::debug!("git ls-remote --get-url returned a URL without a host");
        return None;
    };
    match HostPattern::parse(host) {
        Ok(host_pattern) => Some(host_pattern.to_string()),
        Err(error) => {
            log::debug!("ignoring invalid rewritten Session sandbox host '{host}': {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_roots_grant_the_session_home() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let session_home = temporary_directory.path().join(".session");
        let session_root = session_home.join("sessions/first");
        let repository_root = session_root.join("repo");
        fs::create_dir_all(&repository_root).unwrap();

        assert_eq!(
            session_home_for_roots(&session_home, &[session_root]),
            Some(session_home.clone())
        );
        assert_eq!(
            session_home_for_roots(&session_home, &[repository_root]),
            Some(session_home.clone())
        );

        fs::create_dir_all(session_home.join("outside")).unwrap();
        let canonical_session_root = session_home.join("outside/../sessions/first");
        assert_eq!(
            session_home_for_roots(&session_home, &[canonical_session_root]),
            Some(session_home)
        );
    }

    #[test]
    fn roots_outside_sessions_or_at_the_sessions_root_do_not_grant() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let session_home = temporary_directory.path().join(".session");
        let sessions_root = session_home.join("sessions");
        let outside_root = temporary_directory.path().join("workspace");
        fs::create_dir_all(&sessions_root).unwrap();
        fs::create_dir_all(&outside_root).unwrap();

        assert_eq!(
            session_home_for_roots(&session_home, &[sessions_root]),
            None
        );
        assert_eq!(session_home_for_roots(&session_home, &[outside_root]), None);
    }

    #[test]
    fn network_hosts_include_store_hosts_and_git_url_rewrites() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let session_home = temporary_directory.path().join(".session");
        fs::create_dir_all(session_home.join("store/github.com")).unwrap();
        fs::create_dir_all(session_home.join("store/local")).unwrap();
        let global_git_config = temporary_directory.path().join("gitconfig");
        fs::write(
            &global_git_config,
            "[url \"https://proxy.example/gh/\"]\n\tinsteadOf = https://github.com/\n",
        )
        .unwrap();

        assert_eq!(
            futures::executor::block_on(session_network_hosts_with_env(
                &session_home,
                &test_git_environment(&global_git_config),
            )),
            vec!["github.com", "proxy.example"]
        );
    }

    #[test]
    fn network_hosts_default_to_github_without_a_store_directory() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let session_home = temporary_directory.path().join(".session");
        let global_git_config = temporary_directory.path().join("gitconfig");
        fs::write(&global_git_config, "").unwrap();

        assert_eq!(
            futures::executor::block_on(session_network_hosts_with_env(
                &session_home,
                &test_git_environment(&global_git_config),
            )),
            vec!["github.com"]
        );
    }

    fn test_git_environment(global_git_config: &Path) -> Vec<(OsString, OsString)> {
        let mut environment = vec![
            (
                OsString::from("GIT_CONFIG_GLOBAL"),
                global_git_config.as_os_str().to_os_string(),
            ),
            (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
        ];
        if let Some(path) = env::var_os("PATH") {
            environment.push((OsString::from("PATH"), path));
        }
        environment
    }
}
