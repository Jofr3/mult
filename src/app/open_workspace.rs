//! The open-workspace prompt: fuzzy-matching configured projects, expanding a
//! typed path, and importing what the user chose.

use std::path::{Path, PathBuf};

use crate::{config::ConfiguredProject, model::RemoteTarget};

use super::*;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWorkspacePrompt {
    pub input: PromptInput,
    pub error: Option<String>,
    pub selected: ListSelection,
    pub mode: OpenWorkspaceMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenWorkspaceMode {
    Path,
    ConfiguredProjects,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenWorkspaceMatch {
    pub name: String,
    pub path: PathBuf,
    /// The `ssh` destination the project is opened through, when it is not on
    /// this machine. The prompt renders it in front of the path, which is the
    /// only place a user can tell the two kinds of shortcut apart before
    /// pressing enter.
    pub remote: Option<String>,
}

impl App {
    pub fn open_workspace_matches(
        &self,
        projects: &[ConfiguredProject],
    ) -> Vec<OpenWorkspaceMatch> {
        if let Some(Prompt::OpenWorkspace(prompt)) = self.prompt() {
            if prompt.mode == OpenWorkspaceMode::ConfiguredProjects {
                return open_workspace_matches_for(prompt.input.as_str(), projects);
            }
        }

        Vec::new()
    }

    pub fn select_next_open_workspace_match(&mut self, projects: &[ConfiguredProject]) {
        self.move_open_workspace_selection(1, projects);
    }

    pub fn select_previous_open_workspace_match(&mut self, projects: &[ConfiguredProject]) {
        self.move_open_workspace_selection(-1, projects);
    }

    fn move_open_workspace_selection(&mut self, delta: isize, projects: &[ConfiguredProject]) {
        let Some(Prompt::OpenWorkspace(prompt)) = self.prompt() else {
            return;
        };
        if prompt.mode != OpenWorkspaceMode::ConfiguredProjects {
            return;
        }
        let len = open_workspace_matches_for(prompt.input.as_str(), projects).len();
        if let Some(Prompt::OpenWorkspace(prompt)) = self.prompt_mut() {
            prompt.selected.step(delta, len);
        }
    }

    pub fn begin_open_workspace(&mut self, projects: &[ConfiguredProject]) {
        let has_configured_projects = !projects.is_empty();
        let input = if has_configured_projects {
            String::new()
        } else {
            std::env::current_dir()
                .ok()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        };

        self.set_prompt(Prompt::OpenWorkspace(OpenWorkspacePrompt {
            input: PromptInput::new(input),
            error: None,
            selected: ListSelection::default(),
            mode: if has_configured_projects {
                OpenWorkspaceMode::ConfiguredProjects
            } else {
                OpenWorkspaceMode::Path
            },
        }));
    }

    pub fn submit_open_workspace(&mut self, projects: &[ConfiguredProject]) {
        let Some(Prompt::OpenWorkspace(prompt)) = self.prompt() else {
            return;
        };
        let raw_input = prompt.input.as_str().trim().to_string();
        let selected = prompt.selected.index();
        let mode = prompt.mode;

        if mode == OpenWorkspaceMode::ConfiguredProjects {
            let matches = open_workspace_matches_for(&raw_input, projects);
            if let Some(project) = matches.get(selected.min(matches.len().saturating_sub(1))) {
                match project.remote.clone() {
                    // A remote path is not expanded and not canonicalized: it
                    // names a directory on the other machine, and the only
                    // process that can resolve it is the shell over there.
                    Some(host) => self.import_remote_workspace(
                        &project.name,
                        &host,
                        &project.path.to_string_lossy(),
                    ),
                    None => self.import_workspace_path(
                        expand_path(&project.path),
                        Some(project.name.clone()),
                    ),
                }
                return;
            }

            if raw_input.is_empty() {
                self.set_open_workspace_error("select a configured project");
                return;
            }
            if !looks_like_path(&raw_input) {
                self.set_open_workspace_error("no matching configured project");
                return;
            }
        } else if raw_input.is_empty() {
            self.set_open_workspace_error("enter a directory path");
            return;
        }

        self.import_workspace_path(expand_tilde(&raw_input), None);
    }

    fn import_workspace_path(&mut self, path: PathBuf, configured_name: Option<String>) {
        let Ok(cwd) = std::fs::canonicalize(&path) else {
            self.set_open_workspace_error("path does not exist");
            return;
        };

        if !cwd.is_dir() {
            self.set_open_workspace_error("path is not a directory");
            return;
        }

        if let Some(existing_workspace) = self
            .project
            .workspaces
            .iter()
            .find(|workspace| workspace.cwd.as_deref() == Some(cwd.as_path()))
        {
            let workspace = existing_workspace.id;
            self.clear_prompt();
            self.select_first_item_in_workspace(workspace);
            return;
        }

        let name = configured_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| workspace_name(&cwd));
        self.insert_workspace(name, Some(cwd), None);
    }

    /// Imports a project that lives on another machine.
    ///
    /// None of what [`Self::import_workspace_path`] does to a local path is
    /// right here: there is nothing on this filesystem to canonicalize, to
    /// check for existence, or to expand a `~` against, and doing any of it
    /// would either fail on a perfectly good project or silently open a local
    /// directory that happens to share the name. What can be checked without a
    /// network round trip is the destination, and that is checked now rather
    /// than at the first blank pane.
    fn import_remote_workspace(&mut self, name: &str, host: &str, path: &str) {
        let host = match crate::remote::check_destination(host) {
            Ok(host) => host.to_string(),
            Err(error) => {
                self.set_open_workspace_error(error.to_string());
                return;
            }
        };
        let path = path.trim();
        if path.is_empty() {
            self.set_open_workspace_error(crate::remote::RemoteError::EmptyPath.to_string());
            return;
        }

        // Identity is the machine plus the directory, the remote counterpart of
        // the canonical `cwd` a local import dedupes on: opening the same
        // shortcut twice must land on the workspace already attached to that
        // tmux session rather than start a second one beside it.
        if let Some(existing) = self.project.workspaces.iter().find(|workspace| {
            workspace
                .remote
                .as_ref()
                .is_some_and(|target| target.host == host && target.path == path)
        }) {
            let workspace = existing.id;
            self.clear_prompt();
            self.select_first_item_in_workspace(workspace);
            return;
        }

        let name = match name.trim() {
            "" => path
                .rsplit('/')
                .find(|part| !part.is_empty())
                .unwrap_or(path),
            name => name,
        };
        let target = RemoteTarget {
            host,
            path: path.to_string(),
            session: crate::remote::session_name(name),
        };
        self.insert_workspace(name.to_string(), None, Some(target));
    }

    /// The half of an import that both kinds share: allocate, add the pane the
    /// workspace opens on, and select it.
    fn insert_workspace(
        &mut self,
        name: String,
        cwd: Option<PathBuf>,
        remote: Option<RemoteTarget>,
    ) {
        // Stage both allocations so terminal-ID exhaustion cannot leave a
        // half-imported workspace in memory.
        let mut project = self.project.clone();
        let workspace = match remote {
            Some(target) => project.add_remote_workspace(name, target),
            None => project.add_workspace(name, cwd),
        };
        let workspace = match workspace {
            Ok(workspace) => workspace,
            Err(error) => {
                self.set_open_workspace_error(error.to_string());
                return;
            }
        };
        // A shell either way. On a remote workspace it is a shell over there,
        // which `runtime::session` arranges when the pane starts; the workspace
        // opens on the same thing whichever machine it is on.
        match project.add_terminal(workspace, "shell".to_string()) {
            Ok(Some(_)) => {}
            Ok(None) => {
                self.set_open_workspace_error("new workspace disappeared during import");
                return;
            }
            Err(error) => {
                self.set_open_workspace_error(error.to_string());
                return;
            }
        }
        self.project = project;

        self.clear_prompt();
        self.select_first_item_in_workspace(workspace);
        self.clear_operation_error();
        self.mark_structural_change();
    }

    fn set_open_workspace_error(&mut self, message: impl Into<String>) {
        if let Some(Prompt::OpenWorkspace(prompt)) = self.prompt_mut() {
            prompt.error = Some(message.into());
        }
    }
}

fn open_workspace_matches_for(
    query: &str,
    projects: &[ConfiguredProject],
) -> Vec<OpenWorkspaceMatch> {
    let query = query.trim();
    let mut matches = projects
        .iter()
        .enumerate()
        .filter_map(|(index, project)| {
            fuzzy_project_score(&project.name, query).map(|score| {
                (
                    score,
                    index,
                    OpenWorkspaceMatch {
                        name: project.name.clone(),
                        path: project.path.clone(),
                        remote: project.remote_destination().map(ToOwned::to_owned),
                    },
                )
            })
        })
        .collect::<Vec<_>>();

    matches.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    matches.into_iter().map(|(_, _, project)| project).collect()
}

fn fuzzy_project_score(name: &str, query: &str) -> Option<i64> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }

    let name_lower = name.to_lowercase();
    query.split_whitespace().try_fold(0, |score, term| {
        fuzzy_term_score(&name_lower, term).map(|term_score| score + term_score)
    })
}

fn fuzzy_term_score(name: &str, term: &str) -> Option<i64> {
    if term.is_empty() {
        return Some(0);
    }

    let name_chars = name.chars().collect::<Vec<_>>();
    let term_chars = term.chars().collect::<Vec<_>>();
    let mut score = if name.contains(term) { 20 } else { 0 };
    let mut position = 0;
    let mut last_match: Option<usize> = None;

    for ch in term_chars {
        while position < name_chars.len() && name_chars[position] != ch {
            position += 1;
        }
        if position == name_chars.len() {
            return None;
        }

        score += 10;
        if position == 0 {
            score += 8;
        } else if is_name_boundary(name_chars[position.saturating_sub(1)]) {
            score += 6;
        }
        if let Some(previous) = last_match {
            if position == previous + 1 {
                score += 5;
            } else {
                score -= (position - previous - 1).min(8) as i64;
            }
        }

        last_match = Some(position);
        position += 1;
    }

    score -= name_chars.len().saturating_sub(term.len()).min(16) as i64;
    Some(score)
}

fn is_name_boundary(ch: char) -> bool {
    matches!(ch, '-' | '_' | ' ' | '/' | '.')
}

fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return std::env::var_os("HOME").map_or_else(|| PathBuf::from(input), PathBuf::from);
    }

    if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }

    PathBuf::from(input)
}

fn expand_path(path: &Path) -> PathBuf {
    path.to_str()
        .map(expand_tilde)
        .unwrap_or_else(|| path.to_path_buf())
}

fn looks_like_path(input: &str) -> bool {
    let input = input.trim();
    Path::new(input).is_absolute()
        || input.starts_with('~')
        || input.starts_with('.')
        || input.contains(std::path::MAIN_SEPARATOR)
}

fn workspace_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::model::TerminalLaunch;

    #[test]
    fn importing_workspace_adds_terminal_without_agent_chat() {
        let path = unique_temp_dir();
        let mut app = App::default();
        app.begin_open_workspace(&[]);
        if let Some(Prompt::OpenWorkspace(prompt)) = app.prompt_mut_for_test() {
            prompt.input = PromptInput::new(path.display().to_string());
        }

        app.submit_open_workspace(&[]);

        let imported = app.project.workspaces.last().unwrap();
        assert_eq!(imported.cwd.as_deref(), Some(path.as_path()));
        assert_eq!(imported.chats.len(), 0);
        assert_eq!(imported.terminals.len(), 1);
        assert_eq!(
            app.selected_item(),
            Some(NavItem::Terminal {
                workspace: imported.id,
                terminal: imported.terminals[0].id,
            })
        );
        assert_eq!(app.prompt(), None);
        assert!(app.is_dirty());
    }

    #[test]
    fn configured_workspace_prompt_fuzzy_filters_by_name_and_uses_configured_name() {
        let selected_path = unique_temp_dir();
        let other_path = unique_temp_dir();
        let projects = vec![
            ConfiguredProject {
                name: "frontend".to_string(),
                path: other_path,
                remote: None,
            },
            ConfiguredProject {
                name: "mult".to_string(),
                path: selected_path.clone(),
                remote: None,
            },
        ];
        let mut app = App::default();

        app.begin_open_workspace(&projects);
        for ch in "mlt".chars() {
            app.push_prompt_char(ch);
        }

        let matches = app.open_workspace_matches(&projects);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "mult");

        app.submit_open_workspace(&projects);

        let imported = app.project.workspaces.last().unwrap();
        assert_eq!(imported.name, "mult");
        assert_eq!(imported.cwd.as_deref(), Some(selected_path.as_path()));
        assert_eq!(
            app.selected_item(),
            Some(NavItem::Terminal {
                workspace: imported.id,
                terminal: imported.terminals[0].id,
            })
        );
        assert_eq!(app.prompt(), None);
        assert!(app.is_dirty());
    }

    #[test]
    fn configured_workspace_prompt_arrow_selects_match() {
        let first_path = unique_temp_dir();
        let second_path = unique_temp_dir();
        let projects = vec![
            ConfiguredProject {
                name: "first".to_string(),
                path: first_path,
                remote: None,
            },
            ConfiguredProject {
                name: "second".to_string(),
                path: second_path.clone(),
                remote: None,
            },
        ];
        let mut app = App::default();

        app.begin_open_workspace(&projects);
        app.select_next_open_workspace_match(&projects);
        app.submit_open_workspace(&projects);

        let imported = app.project.workspaces.last().unwrap();
        assert_eq!(imported.name, "second");
        assert_eq!(imported.cwd.as_deref(), Some(second_path.as_path()));
    }

    /// Opening a remote project must not look at this filesystem at all: the
    /// path below exists on no machine here, and the import still succeeds.
    #[test]
    fn a_remote_project_imports_without_touching_the_local_filesystem() {
        let projects = vec![ConfiguredProject {
            name: "mult".to_string(),
            path: PathBuf::from("~/projects/mult"),
            remote: Some("user@hostname".to_string()),
        }];
        let mut app = App::default();

        app.begin_open_workspace(&projects);
        app.submit_open_workspace(&projects);

        let imported = app.project.workspaces.last().unwrap();
        assert_eq!(imported.name, "mult");
        assert_eq!(imported.cwd, None, "a remote workspace has no local root");
        assert_eq!(
            imported.remote,
            Some(RemoteTarget {
                host: "user@hostname".to_string(),
                path: "~/projects/mult".to_string(),
                session: "mult".to_string(),
            })
        );
        assert_eq!(imported.chats.len(), 0);
        assert_eq!(imported.terminals.len(), 1);
        assert_eq!(
            imported.terminals[0].launch,
            TerminalLaunch::Shell,
            "a terminal is a terminal; what makes it remote is where it starts"
        );
        assert_eq!(app.prompt(), None);
        assert!(app.is_dirty());
    }

    /// The remote counterpart of the local import's canonical-`cwd` check: the
    /// second open lands on the workspace already attached to that session.
    #[test]
    fn opening_the_same_remote_project_twice_selects_the_first_workspace() {
        let projects = vec![ConfiguredProject {
            name: "mult".to_string(),
            path: PathBuf::from("~/projects/mult"),
            remote: Some("user@hostname".to_string()),
        }];
        let mut app = App::default();
        app.begin_open_workspace(&projects);
        app.submit_open_workspace(&projects);
        let first = app.project.workspaces.last().unwrap().id;
        let count = app.project.workspaces.len();

        app.begin_open_workspace(&projects);
        app.submit_open_workspace(&projects);

        assert_eq!(app.project.workspaces.len(), count);
        assert_eq!(app.selected_workspace_id(), Some(first));
    }

    /// The same project name on two machines is two projects, and the same
    /// machine twice at different paths is two workspaces.
    #[test]
    fn a_remote_workspace_is_identified_by_machine_and_directory() {
        let projects = vec![
            ConfiguredProject {
                name: "mult".to_string(),
                path: PathBuf::from("~/projects/mult"),
                remote: Some("user@one".to_string()),
            },
            ConfiguredProject {
                name: "mult-two".to_string(),
                path: PathBuf::from("~/projects/mult"),
                remote: Some("user@two".to_string()),
            },
            ConfiguredProject {
                name: "docs".to_string(),
                path: PathBuf::from("~/projects/docs"),
                remote: Some("user@one".to_string()),
            },
        ];
        let mut app = App::default();
        let before = app.project.workspaces.len();

        for index in 0..projects.len() {
            app.begin_open_workspace(&projects);
            for _ in 0..index {
                app.select_next_open_workspace_match(&projects);
            }
            app.submit_open_workspace(&projects);
        }

        assert_eq!(app.project.workspaces.len(), before + 3);
    }

    #[test]
    fn a_remote_project_ssh_could_not_use_stays_in_the_prompt() {
        let projects = vec![ConfiguredProject {
            name: "mult".to_string(),
            path: PathBuf::from("~/projects/mult"),
            remote: Some("-oProxyCommand=id".to_string()),
        }];
        let mut app = App::default();

        app.begin_open_workspace(&projects);
        app.submit_open_workspace(&projects);

        let Some(Prompt::OpenWorkspace(prompt)) = app.prompt() else {
            panic!("expected prompt to remain open");
        };
        assert!(
            prompt
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh would read as an option")),
            "{:?}",
            prompt.error
        );
        assert!(!app.is_dirty());
    }

    /// A `tmux` session name is not a free-form string, so the workspace name
    /// and the session it opens are allowed to differ.
    #[test]
    fn a_project_name_tmux_would_refuse_still_opens() {
        let projects = vec![ConfiguredProject {
            name: "docs.site".to_string(),
            path: PathBuf::from("/srv/docs"),
            remote: Some("host".to_string()),
        }];
        let mut app = App::default();

        app.begin_open_workspace(&projects);
        app.submit_open_workspace(&projects);

        let imported = app.project.workspaces.last().unwrap();
        assert_eq!(imported.name, "docs.site", "the row keeps the user's name");
        assert_eq!(imported.remote.as_ref().unwrap().session, "docs-site");
    }

    #[test]
    fn invalid_import_stays_in_prompt() {
        let mut app = App::default();
        app.begin_open_workspace(&[]);
        if let Some(Prompt::OpenWorkspace(prompt)) = app.prompt_mut_for_test() {
            prompt.input = PromptInput::new("/this/path/should/not/exist");
        }

        app.submit_open_workspace(&[]);

        let Some(Prompt::OpenWorkspace(prompt)) = app.prompt() else {
            panic!("expected prompt to remain open");
        };
        assert_eq!(prompt.error.as_deref(), Some("path does not exist"));
        assert!(!app.is_dirty());
    }

    fn unique_temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mult-test-{unique}"));
        fs::create_dir(&path).expect("create temp workspace");
        path.canonicalize().expect("canonicalize temp workspace")
    }
}
