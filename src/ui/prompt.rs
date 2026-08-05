//! The prompt row: one drawing routine per prompt, plus the shared input line
//! with the cursor drawn inside the text (E7).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    app::{
        App, CommandPaletteEntry, ConfirmDeletePrompt, ConfirmRestorePrompt, OpenWorkspaceMatch,
        OpenWorkspaceMode, Prompt, PromptInput, SearchScope,
    },
    config,
    layout::MAX_LISTED_RESTORE_COMMANDS,
};

use super::{
    text::truncate_text,
    theme::{readable_fg, Palette},
};

pub(super) fn draw_prompt_area(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    palette: Palette,
    config: &config::Config,
) {
    let Some(prompt) = app.prompt() else {
        return;
    };

    match prompt {
        Prompt::OpenWorkspace(prompt) if prompt.mode == OpenWorkspaceMode::ConfiguredProjects => {
            draw_open_workspace_prompt(
                frame,
                area,
                palette,
                &prompt.input,
                prompt.selected.index(),
                prompt.error.as_deref(),
                app.open_workspace_matches(&config.projects),
            )
        }
        Prompt::OpenWorkspace(prompt) => draw_text_prompt(
            frame,
            area,
            palette,
            "Path: ",
            &prompt.input,
            prompt.error.as_deref(),
            "enter imports • esc/ctrl-c cancels",
        ),
        Prompt::NewTerminalCommand(prompt) => draw_text_prompt(
            frame,
            area,
            palette,
            "Command: ",
            &prompt.input,
            prompt.error.as_deref(),
            "enter adds command terminal • esc/ctrl-c cancels",
        ),
        Prompt::CommandPalette(prompt) => draw_command_palette_prompt(
            frame,
            area,
            palette,
            &prompt.input,
            prompt.selected.index(),
            app.active_command_palette_entries(),
        ),
        Prompt::Search(prompt) => draw_text_prompt(
            frame,
            area,
            palette,
            search_prompt_label(prompt.scope),
            &prompt.input,
            prompt.error.as_deref(),
            "enter applies filter • empty enter clears • esc/ctrl-c cancels",
        ),
        Prompt::ConfirmDelete(prompt) => draw_confirm_delete_prompt(frame, area, palette, prompt),
        Prompt::ConfirmRestore(prompt) => draw_confirm_restore_prompt(frame, area, palette, prompt),
        // Drawn over the whole frame by `draw`, not in the prompt row.
        Prompt::Help(_) => {}
    }
}

/// The confirmation in front of a delete (E3).
///
/// It names the item — including the command a command terminal runs and how
/// many messages a chat holds — and, when the parent workspace goes with it,
/// says so on its own line. That cascade used to happen silently.
pub(super) fn draw_confirm_delete_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    prompt: &ConfirmDeletePrompt,
) {
    let mut lines = vec![Line::from(vec![
        Span::styled("Delete ", Style::default().fg(palette.love)),
        Span::styled(
            prompt.summary.clone(),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("?", Style::default().fg(palette.love)),
    ])];

    if let Some(cascade) = &prompt.cascade {
        lines.push(Line::from(Span::styled(
            format!("! {cascade}"),
            Style::default().fg(palette.gold),
        )));
    }

    lines.push(Line::from(Span::styled(
        "y/enter deletes • esc/n/ctrl-c cancels",
        Style::default().fg(palette.muted),
    )));

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(palette.text).bg(palette.base)),
        area,
    );
}

/// The startup confirmation in front of replaying persisted command terminals
/// (C1).
///
/// The commands are printed verbatim, one per line, because the whole point is
/// that the user sees what `state.json` is asking to run before it runs. Long
/// lists elide, but the count never lies about how many there are.
pub(super) fn draw_confirm_restore_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    prompt: &ConfirmRestorePrompt,
) {
    let count = prompt.entries.len();
    let plural = if count == 1 { "" } else { "s" };
    let mut lines = vec![Line::from(vec![
        Span::styled("Run ", Style::default().fg(palette.gold)),
        Span::styled(
            format!("{count} saved command terminal{plural}"),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" from state.json?", Style::default().fg(palette.gold)),
    ])];

    for entry in prompt.entries.iter().take(MAX_LISTED_RESTORE_COMMANDS) {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}: ", entry.name),
                Style::default().fg(palette.muted),
            ),
            Span::styled(
                truncate_text(&entry.command, usize::from(area.width).saturating_sub(4)),
                Style::default().fg(palette.text),
            ),
        ]));
    }
    if let Some(hidden) = count
        .checked_sub(MAX_LISTED_RESTORE_COMMANDS)
        .filter(|n| *n > 0)
    {
        lines.push(Line::from(Span::styled(
            format!("  ... and {hidden} more"),
            Style::default().fg(palette.muted),
        )));
    }

    lines.push(Line::from(Span::styled(
        "y/enter runs them • esc/n leaves them stopped",
        Style::default().fg(palette.muted),
    )));

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(palette.text).bg(palette.base)),
        area,
    );
}

/// A prompt's label, its text, and the cursor drawn *inside* the text (E7).
///
/// The cursor cell is the character it sits on, reversed; a block only when it
/// is past the end. Splitting on character boundaries keeps multi-byte text
/// intact, and giving each side its own span means a wide character before the
/// cursor moves it two columns, as it should.
pub(super) fn prompt_input_line(
    label: &str,
    input: &PromptInput,
    palette: Palette,
) -> Line<'static> {
    let (cursor_text, cursor_style) = match input.char_at_cursor() {
        Some(ch) if palette.monochrome => (
            ch.to_string(),
            Style::default().add_modifier(Modifier::REVERSED),
        ),
        Some(ch) => (
            ch.to_string(),
            Style::default()
                .bg(palette.cursor)
                .fg(readable_fg(palette.base, palette.cursor)),
        ),
        None => ("▌".to_string(), Style::default().fg(palette.cursor)),
    };

    Line::from(vec![
        Span::styled(label.to_string(), Style::default().fg(palette.muted)),
        Span::raw(input.before_cursor().to_string()),
        Span::styled(cursor_text, cursor_style),
        Span::raw(input.after_cursor().to_string()),
    ])
}

pub(super) fn search_prompt_label(scope: SearchScope) -> &'static str {
    match scope {
        SearchScope::Terminal(_) => "Search terminal: ",
        SearchScope::Chat(_) => "Search chat: ",
    }
}

pub(super) fn draw_open_workspace_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    input: &PromptInput,
    selected: usize,
    error: Option<&str>,
    entries: Vec<OpenWorkspaceMatch>,
) {
    let mut lines = vec![prompt_input_line("Project: ", input, palette)];

    if let Some(error) = error {
        lines.push(Line::from(Span::styled(
            error.to_string(),
            Style::default().fg(palette.love),
        )));
    }

    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "No matching configured projects".to_string(),
            Style::default().fg(palette.love),
        )));
    } else {
        let max_entries = usize::from(area.height.saturating_sub(lines.len() as u16)).max(1);
        let start = selected.saturating_sub(max_entries.saturating_sub(1));
        lines.extend(
            entries
                .into_iter()
                .enumerate()
                .skip(start)
                .take(max_entries)
                .map(|(index, entry)| open_workspace_match_line(entry, index == selected, palette)),
        );
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(palette.text).bg(palette.base)),
        area,
    );
}

pub(super) fn open_workspace_match_line(
    entry: OpenWorkspaceMatch,
    selected: bool,
    palette: Palette,
) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let style = if selected {
        palette.selection_style().fg(palette.text)
    } else {
        Style::default().fg(palette.text)
    };
    let path = entry.path.display().to_string();
    // A configured path that is not there is shown and marked rather than
    // hidden or rejected at load (E6): the entry is still the one the user
    // meant, and "missing" is the answer to why importing it will fail.
    let missing = (!entry.path_exists).then_some(Span::styled(
        " (missing)",
        Style::default().fg(palette.love),
    ));

    Line::from(
        [
            Span::styled(marker, style),
            Span::styled(entry.name, style),
            Span::styled(" ", Style::default().fg(palette.muted)),
            Span::styled(path, Style::default().fg(palette.muted)),
        ]
        .into_iter()
        .chain(missing)
        .collect::<Vec<_>>(),
    )
}

pub(super) fn draw_command_palette_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    input: &PromptInput,
    selected: usize,
    entries: Vec<CommandPaletteEntry>,
) {
    let mut lines = vec![
        prompt_input_line("Command: ", input, palette),
        Line::from(Span::styled(
            "type to filter • ↑/↓ select • enter runs • esc cancels".to_string(),
            Style::default().fg(palette.muted),
        )),
    ];

    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "No matching commands".to_string(),
            Style::default().fg(palette.love),
        )));
    } else {
        let max_entries = usize::from(area.height.saturating_sub(2)).max(1);
        let start = selected.saturating_sub(max_entries.saturating_sub(1));
        lines.extend(
            entries
                .into_iter()
                .enumerate()
                .skip(start)
                .take(max_entries)
                .map(|(index, entry)| command_palette_line(entry, index == selected, palette)),
        );
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(palette.text).bg(palette.base)),
        area,
    );
}

pub(super) fn command_palette_line(
    entry: CommandPaletteEntry,
    selected: bool,
    palette: Palette,
) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let style = if selected {
        palette.selection_style().fg(palette.text)
    } else {
        Style::default().fg(palette.text)
    };

    Line::from(vec![
        Span::styled(marker, style),
        Span::styled(entry.label, style),
        Span::styled(" — ", Style::default().fg(palette.muted)),
        Span::styled(entry.help, Style::default().fg(palette.muted)),
    ])
}

pub(super) fn draw_text_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    label: &'static str,
    input: &PromptInput,
    error: Option<&str>,
    help: &'static str,
) {
    let message = error.unwrap_or(help);
    let message_style = if error.is_some() {
        Style::default().fg(palette.love)
    } else {
        Style::default().fg(palette.muted)
    };
    let prompt = Paragraph::new(vec![
        prompt_input_line(label, input, palette),
        Line::from(Span::styled(message.to_string(), message_style)),
    ])
    .style(Style::default().fg(palette.text).bg(palette.base));
    frame.render_widget(prompt, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::AppLayout;
    use std::path::PathBuf;

    use ratatui::{backend::TestBackend, Terminal};

    use crate::{app::App, pty::PtyRuntime};

    use super::super::{draw, test_support::test_palette};

    #[test]
    fn the_prompt_cursor_sits_on_the_character_it_is_on() {
        let mut app = App::two_workspaces();
        app.begin_open_workspace(&[]);
        // The prompt pre-fills the working directory; start from an empty one.
        if let Some(Prompt::OpenWorkspace(prompt)) = app.prompt_mut() {
            prompt.input = PromptInput::default();
        }
        for ch in "aé漢z".chars() {
            app.push_prompt_char(ch);
        }
        app.move_prompt_cursor_left();

        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).expect("create test terminal");
        terminal
            .draw(|frame| {
                let layout = AppLayout::compute(&app, frame.area());
                draw(
                    frame,
                    &layout,
                    &app,
                    &PtyRuntime::new_offline(),
                    &config::Config::default(),
                )
            })
            .expect("draw app");

        // "Path: " is 6 columns; `a` + `é` are one each and `漢` is two, so the
        // cursor lands on column 6 + 4 — a byte index would have put it at 6 + 7
        // and split the multi-byte characters.
        let palette = test_palette();
        let cursor_cell = terminal
            .backend()
            .buffer()
            .cell((10, 3))
            .expect("cursor cell is in bounds");
        assert_eq!(cursor_cell.symbol(), "z");
        assert_eq!(cursor_cell.bg, palette.cursor);
    }
    #[test]
    fn a_missing_configured_project_is_marked_in_the_prompt() {
        let entry = OpenWorkspaceMatch {
            name: "orbit".to_string(),
            path: PathBuf::from("/does/not/exist"),
            path_exists: false,
        };

        let line = open_workspace_match_line(entry.clone(), false, test_palette());
        assert!(
            line.spans.iter().any(|span| span.content == " (missing)"),
            "{line:?}"
        );

        let present = open_workspace_match_line(
            OpenWorkspaceMatch {
                path_exists: true,
                ..entry
            },
            false,
            test_palette(),
        );
        assert!(
            !present
                .spans
                .iter()
                .any(|span| span.content == " (missing)"),
            "{present:?}"
        );
    }
}
