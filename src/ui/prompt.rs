//! The prompt surface at the bottom of the frame: the text prompts, the
//! command palette and the workspace picker.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    app::{App, CommandPaletteEntry, OpenWorkspaceMatch, OpenWorkspaceMode, Prompt, SearchScope},
    config::{self},
};

use super::theme::Palette;

pub(super) fn draw_prompt_area(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    palette: Palette,
    config: &config::Config,
) {
    let prompt_area = super::status::draw_notice_surface(frame, app, area, palette);

    let Some(prompt) = app.prompt() else {
        return;
    };
    let area = prompt_area;

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
    }
}

fn search_prompt_label(scope: SearchScope) -> &'static str {
    match scope {
        SearchScope::Terminal(_) => "Search terminal: ",
        SearchScope::Chat(_) => "Search chat: ",
    }
}

/// The prompt's label, its text and its cursor, as spans.
///
/// The text is emitted verbatim in up to three spans — before the cursor, the
/// single character under it, and after it — so concatenating them reproduces
/// the stored string exactly. That is what keeps the cursor at the right
/// *display* column for wide and combining characters: the character under the
/// cursor is styled, never substituted, so it still occupies its own width.
/// The only span that is not part of the text is the block drawn when the
/// cursor sits past the last character, where there is no cell to style (E7).
fn prompt_input_spans(
    label: &'static str,
    input: &crate::app::PromptInput,
    palette: Palette,
) -> Vec<Span<'static>> {
    let (before, at, after) = input.split_at_cursor();
    let mut spans = vec![
        Span::styled(label, palette.accent(palette.muted, false)),
        Span::raw(before.to_string()),
    ];
    if at.is_empty() {
        spans.push(Span::styled(
            "▌",
            palette.accent(palette.cursor, palette.monochrome),
        ));
    } else {
        spans.push(Span::styled(
            at.to_string(),
            palette.emphasis(palette.nc, palette.cursor),
        ));
        spans.push(Span::raw(after.to_string()));
    }
    spans
}

fn draw_open_workspace_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    input: &crate::app::PromptInput,
    selected: usize,
    error: Option<&str>,
    entries: Vec<OpenWorkspaceMatch>,
) {
    let mut lines = vec![Line::from(prompt_input_spans("Project: ", input, palette))];

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

fn open_workspace_match_line(
    entry: OpenWorkspaceMatch,
    selected: bool,
    palette: Palette,
) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let style = if selected {
        palette.selected_row()
    } else {
        Style::default().fg(palette.text)
    };
    let path = entry.path.display().to_string();

    Line::from(vec![
        Span::styled(marker, style),
        Span::styled(entry.name, style),
        Span::styled(" ", Style::default().fg(palette.muted)),
        Span::styled(path, Style::default().fg(palette.muted)),
    ])
}

fn draw_command_palette_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    input: &crate::app::PromptInput,
    selected: usize,
    entries: Vec<CommandPaletteEntry>,
) {
    let mut lines = vec![
        Line::from(prompt_input_spans("Command: ", input, palette)),
        Line::from(Span::styled(
            "type to filter • ↑/↓ select • enter runs • esc cancels".to_string(),
            palette.accent(palette.muted, false),
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

fn command_palette_line(
    entry: CommandPaletteEntry,
    selected: bool,
    palette: Palette,
) -> Line<'static> {
    let marker = if selected { "› " } else { "  " };
    let style = if selected {
        palette.selected_row()
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

fn draw_text_prompt(
    frame: &mut Frame,
    area: Rect,
    palette: Palette,
    label: &'static str,
    input: &crate::app::PromptInput,
    error: Option<&str>,
    help: &'static str,
) {
    let message = error.unwrap_or(help);
    let message_style = if error.is_some() {
        palette.accent(palette.love, true)
    } else {
        palette.accent(palette.muted, false)
    };
    let prompt = Paragraph::new(vec![
        Line::from(prompt_input_spans(label, input, palette)),
        Line::from(Span::styled(message.to_string(), message_style)),
    ])
    .style(Style::default().fg(palette.text).bg(palette.base));
    frame.render_widget(prompt, area);
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::ui::test_support::*;
    use crate::ui::text::text_width;

    #[test]
    fn a_prompt_cursor_styles_a_wide_character_without_rewriting_the_text() {
        // E7: the spans that carry text must concatenate back to the stored
        // string exactly, so the cursor lands on the right display column even
        // when the character under it is double-width or carries a combining
        // mark.
        let palette = test_palette();
        let mut input = crate::app::PromptInput::new("a日e\u{0301}b");
        assert!(input.apply(crate::app::PromptEdit::MoveHome));
        assert!(input.apply(crate::app::PromptEdit::MoveRight));

        let spans = prompt_input_spans("Path: ", &input, palette);
        // The label is span 0; the rest is the text, verbatim.
        assert_eq!(spans[0].content, "Path: ");
        let text = spans[1..]
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(text, input.as_str());
        // The cursor is on the wide character itself, styled rather than
        // replaced, so it keeps its two columns.
        assert_eq!(spans[2].content, "日");
        assert_eq!(text_width(&spans[1].content), 1);
        assert_eq!(text_width(&spans[2].content), 2);

        // Past the last character there is no cell to style, so the block is
        // appended — the one span that is not part of the text.
        assert!(input.apply(crate::app::PromptEdit::MoveEnd));
        let spans = prompt_input_spans("Path: ", &input, palette);
        assert_eq!(spans[1].content, input.as_str());
        assert_eq!(spans[2].content, "▌");
    }
}
