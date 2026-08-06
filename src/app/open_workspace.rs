//! The open-workspace prompt: fuzzy-matching configured projects, expanding a
//! typed path, and importing what the user chose.

use std::path::{Path, PathBuf};

use crate::config::ConfiguredProject;

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
                self.import_workspace_path(expand_path(&project.path), Some(project.name.clone()));
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
        // Stage both allocations so terminal-ID exhaustion cannot leave a
        // half-imported workspace in memory.
        let mut project = self.project.clone();
        let workspace = match project.add_workspace(name, Some(cwd)) {
            Ok(workspace) => workspace,
            Err(error) => {
                self.set_open_workspace_error(error.to_string());
                return;
            }
        };
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
            },
            ConfiguredProject {
                name: "mult".to_string(),
                path: selected_path.clone(),
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
            },
            ConfiguredProject {
                name: "second".to_string(),
                path: second_path.clone(),
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
