use editor::Editor;
use gpui::{App, Entity, EventEmitter, FocusHandle, Focusable, SharedString, prelude::*};
use project::Project;
use ui::{Headline, HeadlineSize, ListItem, ListItemSpacing, prelude::*};

use crate::Agent;
use crate::agent_panel::external_agent_entries;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AgentPickerEntry {
    Agent(Agent),
    Terminal,
    AddMoreAgents,
}

/// Emitted when the user picks an agent or a terminal, along with the name
/// they gave the new thread, if any.
pub(crate) struct AgentPickerConfirmed {
    pub entry: AgentPickerEntry,
    pub title: Option<SharedString>,
}

enum RowIcon {
    Builtin(IconName),
    External(SharedString),
}

struct Row {
    entry: AgentPickerEntry,
    label: SharedString,
    icon: RowIcon,
}

/// Lets the user choose the agent a new thread starts with, and optionally
/// name the thread, instead of starting it with the last used agent.
pub(crate) struct AgentPicker {
    project: Entity<Project>,
    title_editor: Entity<Editor>,
    selected_index: usize,
}

impl EventEmitter<AgentPickerConfirmed> for AgentPicker {}

impl AgentPicker {
    pub(crate) fn new(
        project: Entity<Project>,
        preselected_agent: Agent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Thread name (optional)", window, cx);
            editor
        });
        let mut picker = Self {
            project,
            title_editor,
            selected_index: 0,
        };
        let preselected_entry = AgentPickerEntry::Agent(preselected_agent);
        picker.selected_index = picker
            .rows(cx)
            .iter()
            .position(|row| row.entry == preselected_entry)
            .unwrap_or(0);
        picker
    }

    #[cfg(test)]
    pub(crate) fn title_editor(&self) -> &Entity<Editor> {
        &self.title_editor
    }

    #[cfg(test)]
    pub(crate) fn entries(&self, cx: &App) -> Vec<AgentPickerEntry> {
        self.rows(cx).into_iter().map(|row| row.entry).collect()
    }

    #[cfg(test)]
    pub(crate) fn selected_entry(&self, cx: &App) -> Option<AgentPickerEntry> {
        self.rows(cx)
            .into_iter()
            .nth(self.selected_index)
            .map(|row| row.entry)
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        let project = self.project.read(cx);
        let mut rows = vec![Row {
            entry: AgentPickerEntry::Agent(Agent::NativeAgent),
            label: "Zed Agent".into(),
            icon: RowIcon::Builtin(IconName::ZedAgent),
        }];
        if project.supports_terminal(cx) {
            rows.push(Row {
                entry: AgentPickerEntry::Terminal,
                label: "Terminal".into(),
                icon: RowIcon::Builtin(IconName::Terminal),
            });
        }
        // External agents can't run in a collab project, so they aren't offered.
        if !project.is_via_collab() {
            rows.extend(
                external_agent_entries(project.agent_server_store().read(cx), cx)
                    .into_iter()
                    .map(|agent| Row {
                        entry: AgentPickerEntry::Agent(Agent::Custom { id: agent.id }),
                        label: agent.display_name,
                        icon: agent
                            .icon_path
                            .map(RowIcon::External)
                            .unwrap_or(RowIcon::Builtin(IconName::Sparkle)),
                    }),
            );
        }
        rows.push(Row {
            entry: AgentPickerEntry::AddMoreAgents,
            label: "Add More Agents".into(),
            icon: RowIcon::Builtin(IconName::Plus),
        });
        rows
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        let row_count = self.rows(cx).len();
        if row_count > 0 {
            self.selected_index = (self.selected_index + 1) % row_count;
            cx.notify();
        }
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row_count = self.rows(cx).len();
        if row_count > 0 {
            self.selected_index = (self.selected_index + row_count - 1) % row_count;
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_index(self.selected_index, window, cx);
    }

    fn confirm_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows(cx).into_iter().nth(index) else {
            return;
        };
        self.selected_index = index;
        if row.entry == AgentPickerEntry::AddMoreAgents {
            window.dispatch_action(Box::new(zed_actions::AcpRegistry), cx);
            return;
        }
        let title = self.title_editor.read(cx).text(cx);
        let title = title.trim();
        cx.emit(AgentPickerConfirmed {
            entry: row.entry,
            title: (!title.is_empty()).then(|| SharedString::from(title.to_string())),
        });
    }
}

impl Focusable for AgentPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.title_editor.focus_handle(cx)
    }
}

impl Render for AgentPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows(cx);
        let selected_index = self.selected_index.min(rows.len().saturating_sub(1));
        let colors = cx.theme().colors();

        v_flex()
            .id("agent-picker")
            .key_context("AgentPicker")
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .size_full()
            .items_center()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(440.))
                    .p_4()
                    .gap_3()
                    .child(Headline::new("New Thread").size(HeadlineSize::Small))
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.editor_background)
                            .child(self.title_editor.clone()),
                    )
                    .child(
                        Label::new("Start with")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(v_flex().gap_px().children(rows.into_iter().enumerate().map(
                        |(index, row)| {
                            let icon = match row.icon {
                                RowIcon::Builtin(icon) => Icon::new(icon),
                                RowIcon::External(path) => Icon::from_external_svg(path),
                            };
                            ListItem::new(("agent-picker-row", index))
                                .inset(true)
                                .spacing(ListItemSpacing::Sparse)
                                .toggle_state(index == selected_index)
                                .start_slot(icon.color(Color::Muted).size(IconSize::Small))
                                .child(Label::new(row.label))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.confirm_index(index, window, cx);
                                }))
                        },
                    ))),
            )
    }
}
