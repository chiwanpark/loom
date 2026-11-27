use gpui::{
    AnyElement, App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    SharedString, Styled, Window, div, prelude::FluentBuilder,
};
use gpui_component::button::ButtonVariants;
use gpui_component::{Disableable, Selectable, h_flex, v_flex};
use loom_client::import_local_preset;
use loom_client::oracle_proto::{PresetDetail, PresetFile, SKILLS_DIR, valid_name};
use loom_config::AgentConfig;
use loom_view_model::settings::{age, copy_name};

use super::agent_form::AgentForm;
use super::editor::TextFile;
use super::form::{self, Field};
use super::{SettingsView, card, danger, detail, empty, header, now_unix, small_button, warning};
use crate::ui::notify;

pub(super) const AGENT_YAML: &str = "agent.yaml";
const AGENTS_MD: &str = "AGENTS.md";

pub struct PresetDraft {
    pub name: String,
    pub new: bool,
    pub files: Vec<TextFile>,
    pub removed: Vec<String>,
    pub selected: usize,
    pub notice: Option<String>,
    pub form: Entity<AgentForm>,
    pub yaml_view: bool,
}

impl PresetDraft {
    pub fn is_dirty(&self, cx: &App) -> bool {
        self.new || !self.removed.is_empty() || self.files.iter().any(|f| f.is_dirty(cx))
    }

    pub(super) fn text_of(&self, path: &str, cx: &App) -> String {
        self.files
            .iter()
            .find(|f| f.path == path)
            .map(|f| f.text(cx))
            .unwrap_or_default()
    }

    fn skill_files(&self, cx: &App) -> Vec<PresetFile> {
        self.files
            .iter()
            .filter(|f| f.path != AGENT_YAML && f.path != AGENTS_MD)
            .map(|f| PresetFile {
                path: f.path.clone(),
                content: f.text(cx),
            })
            .collect()
    }
}

fn valid_skill_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix(&format!("{SKILLS_DIR}/")) else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && *p != "." && *p != ".." && !p.starts_with('.'))
}

struct DraftSource {
    name: String,
    new: bool,
    yaml: String,
    agents_md: String,
    files: Vec<PresetFile>,
    notice: Option<String>,
}

impl SettingsView {
    fn open_draft(&mut self, source: DraftSource, window: &mut Window, cx: &mut Context<Self>) {
        let DraftSource {
            name,
            new,
            yaml,
            agents_md,
            files,
            notice,
        } = source;
        let saved = |text: &str| if new { String::new() } else { text.to_owned() };
        let mut texts = vec![
            TextFile::new(AGENT_YAML, &yaml, window, cx),
            TextFile::new(AGENTS_MD, &agents_md, window, cx),
        ];
        for file in files {
            texts.push(TextFile::new(file.path, &file.content, window, cx));
        }
        for file in &mut texts {
            file.saved = saved(&file.saved);
        }
        let (link, yaml) = (self.link.clone(), texts[0].editor.clone());
        let form = cx.new(|cx| AgentForm::new(link, yaml, window, cx));
        self.preset = Some(PresetDraft {
            name,
            new,
            files: texts,
            removed: Vec::new(),
            selected: 0,
            notice,
            form,
            yaml_view: false,
        });
        cx.notify();
    }

    pub(super) fn show_agent_yaml(
        &mut self,
        yaml: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = &mut self.preset else {
            return;
        };
        draft.yaml_view = yaml;
        if !yaml {
            let form = draft.form.clone();
            form.update(cx, |form, cx| form.reload(window, cx));
        }
        cx.notify();
    }

    fn with_draft_closed(
        &mut self,
        run: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.preset.as_ref().is_some_and(|p| p.is_dirty(cx)) {
            self.confirm_then(
                "Discard your changes?".to_owned(),
                "The changes to the open preset are not saved.".to_owned(),
                "Discard",
                move |this, window, cx| {
                    this.preset = None;
                    run(this, window, cx);
                },
                window,
                cx,
            );
        } else {
            self.preset = None;
            run(self, window, cx);
        }
    }

    fn edit_preset(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.with_draft_closed(
            move |this, window, cx| {
                let preset = name.clone();
                let task = this.call(
                    cx,
                    move |client| async move { client.preset(&preset).await },
                );
                this.act(
                    Some(format!("preset:{name}")),
                    task,
                    |this, detail: PresetDetail, window, cx| {
                        let p = detail.preset;
                        this.open_draft(
                            DraftSource {
                                name: p.name,
                                new: false,
                                yaml: p.yaml,
                                agents_md: p.agents_md,
                                files: detail.files,
                                notice: None,
                            },
                            window,
                            cx,
                        );
                    },
                    window,
                    cx,
                );
            },
            window,
            cx,
        );
    }

    fn taken_names(&self, cx: &App) -> Vec<String> {
        self.link
            .read(cx)
            .presets
            .iter()
            .map(|p| p.name.clone())
            .collect()
    }

    fn ask_name(
        &mut self,
        title: &str,
        description: Option<String>,
        suggested: String,
        then: impl Fn(&mut Self, String, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let taken = self.taken_names(cx);
        let then = std::rc::Rc::new(then);
        form::open(
            title.to_owned(),
            description,
            vec![Field::text("Name").value(suggested).placeholder("work")],
            "Continue",
            move |values, done, window, cx| {
                let name = values.text(0);
                if !valid_name(&name) {
                    return done.fail("Use lowercase letters, digits, and dashes.", window, cx);
                }
                if taken.contains(&name) {
                    return done.fail(format!("A preset {name} is there already."), window, cx);
                }
                done.finish(Ok(()), window, cx);
                let then = then.clone();
                let _ = this.update(cx, |view, cx| then(view, name, window, cx));
            },
            window,
            cx,
        );
    }

    fn new_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.with_draft_closed(
            |this, window, cx| {
                this.ask_name(
                    "New preset",
                    None,
                    String::new(),
                    |this, name, window, cx| {
                        this.open_draft(
                            DraftSource {
                                name,
                                new: true,
                                yaml: String::new(),
                                agents_md: String::new(),
                                files: Vec::new(),
                                notice: None,
                            },
                            window,
                            cx,
                        )
                    },
                    window,
                    cx,
                )
            },
            window,
            cx,
        );
    }

    pub(super) fn import_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let imported = match import_local_preset(&self.dirs) {
            Ok(imported) => imported,
            Err(err) => return notify::error(window, cx, err),
        };
        let source = self.dirs.config_dir().display().to_string();
        let imported = std::rc::Rc::new(imported);
        self.with_draft_closed(
            move |this, window, cx| {
                let imported = imported.clone();
                let source = source.clone();
                this.ask_name(
                    "Import preset",
                    Some(format!(
                        "Loom reads agent.yaml, AGENTS.md, and the skills in {source}. You can look at the preset before you save it."
                    )),
                    String::from("local"),
                    move |this, name, window, cx| {
                        let notice = (!imported.removed.is_empty()).then(|| {
                            format!(
                                "Loom removed the sections that the codespace sets itself: {}.",
                                imported.removed.join(", ")
                            )
                        });
                        this.open_draft(
                            DraftSource {
                                name,
                                new: true,
                                yaml: imported.yaml.clone(),
                                agents_md: imported.agents_md.clone(),
                                files: imported.files.clone(),
                                notice,
                            },
                            window,
                            cx,
                        )
                    },
                    window,
                    cx,
                )
            },
            window,
            cx,
        );
    }

    fn duplicate_preset(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let suggested = copy_name(&name, &self.taken_names(cx));
        self.ask_name(
            &format!("Duplicate {name}"),
            None,
            suggested,
            move |this, copy, window, cx| {
                let source = name.clone();
                let task = this.call(cx, move |client| async move {
                    let detail = client.preset(&source).await?;
                    let p = detail.preset;
                    client
                        .set_full_preset(&copy, &p.yaml, Some(&p.agents_md), Some(detail.files))
                        .await
                });
                this.act(
                    None,
                    task,
                    |_, preset, window, cx| {
                        notify::success(window, cx, format!("Made the preset {}.", preset.name))
                    },
                    window,
                    cx,
                );
            },
            window,
            cx,
        );
    }

    fn delete_preset(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_then(
            format!("Delete the preset {name}?"),
            "The oracle refuses this while an open codespace uses the preset.".to_owned(),
            "Delete",
            move |this, window, cx| {
                let preset = name.clone();
                let task = this.call(cx, move |client| async move {
                    client.delete_preset(&preset).await
                });
                let name = name.clone();
                this.act(
                    None,
                    task,
                    move |this, _, _, cx| {
                        if this.preset.as_ref().is_some_and(|p| p.name == name) {
                            this.preset = None;
                            cx.notify();
                        }
                    },
                    window,
                    cx,
                );
            },
            window,
            cx,
        );
    }

    pub(super) fn save_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.preset else {
            return;
        };
        let name = draft.name.clone();
        let yaml = draft.text_of(AGENT_YAML, cx);
        let agents_md = draft.text_of(AGENTS_MD, cx);
        let files = draft.skill_files(cx);
        let preset = name.clone();
        let task = self.call(cx, move |client| async move {
            client
                .set_full_preset(&preset, &yaml, Some(&agents_md), Some(files))
                .await
        });
        self.act(
            Some(format!("save-preset:{name}")),
            task,
            |this, preset, window, cx| {
                if let Some(draft) = &mut this.preset
                    && draft.name == preset.name
                {
                    draft.new = false;
                    draft.removed.clear();
                    draft.notice = None;
                    for file in &mut draft.files {
                        file.mark_saved(cx);
                    }
                }
                notify::success(window, cx, format!("Saved the preset {}.", preset.name));
            },
            window,
            cx,
        );
    }

    fn close_preset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.with_draft_closed(|_, _, cx| cx.notify(), window, cx);
    }

    fn add_skill_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        form::open(
            "Add skill file",
            Some("Each skill directory must have a SKILL.md file.".to_owned()),
            vec![
                Field::text("Path")
                    .value(format!("{SKILLS_DIR}/"))
                    .placeholder("skills/review/SKILL.md"),
            ],
            "Add",
            move |values, done, window, cx| {
                let path = values.text(0);
                if !valid_skill_path(&path) {
                    return done.fail("Give a path such as skills/review/SKILL.md.", window, cx);
                }
                let mut fail = None;
                let _ = this.update(cx, |view, cx| {
                    let Some(draft) = &mut view.preset else {
                        return;
                    };
                    if draft.files.iter().any(|f| f.path == path) {
                        fail = Some(format!("The preset has {path} already."));
                        return;
                    }
                    let mut file = TextFile::new(path.clone(), "", window, cx);
                    file.saved = "\u{0}".to_owned();
                    let Some(draft) = &mut view.preset else {
                        return;
                    };
                    draft.files.push(file);
                    draft.selected = draft.files.len() - 1;
                    cx.notify();
                });
                done.finish(fail.map_or(Ok(()), Err), window, cx);
            },
            window,
            cx,
        );
    }

    fn remove_skill_file(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = &mut self.preset else {
            return;
        };
        if draft.selected < 2 || draft.selected >= draft.files.len() {
            return;
        }
        let file = draft.files.remove(draft.selected);
        draft.removed.push(file.path);
        draft.selected = draft.selected.min(draft.files.len() - 1);
        cx.notify();
    }

    fn render_draft(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let draft = self.preset.as_ref()?;
        let link = self.link.read(cx);
        let saving = self.is_busy(&format!("save-preset:{}", draft.name));
        let dirty = draft.is_dirty(cx);
        let mut tabs = h_flex().gap_1().flex_wrap();
        for (ix, file) in draft.files.iter().enumerate() {
            let label = if file.is_dirty(cx) {
                format!("{} •", file.path)
            } else {
                file.path.clone()
            };
            tabs = tabs.child(
                small_button(SharedString::from(format!("preset-file-{ix}")), label)
                    .selected(ix == draft.selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(draft) = &mut this.preset {
                            draft.selected = ix;
                        }
                        cx.notify();
                    })),
            );
        }
        tabs = tabs
            .child(
                small_button("preset-add-file", "Add skill file…")
                    .on_click(cx.listener(|this, _, window, cx| this.add_skill_file(window, cx))),
            )
            .when(draft.selected >= 2, |el| {
                el.child(
                    small_button("preset-remove-file", "Remove file")
                        .danger()
                        .on_click(cx.listener(|this, _, _, cx| this.remove_skill_file(cx))),
                )
            });
        let names = |items: Vec<String>| {
            if items.is_empty() {
                "none".to_owned()
            } else {
                items.join(", ")
            }
        };
        let hint = format!(
            "Model accounts: {}. Git credentials: {}. Secrets: {}.",
            names(
                link.config
                    .accounts
                    .iter()
                    .map(|a| format!("{} ({})", a.name, a.provider))
                    .collect()
            ),
            names(
                link.config
                    .credentials
                    .iter()
                    .map(|c| c.name.clone())
                    .collect()
            ),
            names(link.config.secrets.iter().map(|s| s.name.clone()).collect()),
        );
        let agent_yaml = draft.selected == 0;
        let form_view = agent_yaml && !draft.yaml_view;
        let modes = agent_yaml.then(|| {
            h_flex()
                .gap_1()
                .child(
                    small_button("preset-agent-form", "Form")
                        .selected(!draft.yaml_view)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.show_agent_yaml(false, window, cx)
                        })),
                )
                .child(
                    small_button("preset-agent-yaml", "YAML")
                        .selected(draft.yaml_view)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.show_agent_yaml(true, window, cx)
                        })),
                )
        });
        let editor = if form_view {
            Some(draft.form.clone().into_any_element())
        } else {
            draft
                .files
                .get(draft.selected)
                .map(|f| f.render(420.).into_any_element())
        };
        let problem = agent_yaml
            .then(|| {
                AgentConfig::parse_preset(&draft.text_of(AGENT_YAML, cx), AGENT_YAML, &self.dirs)
                    .err()
            })
            .flatten()
            .map(|error| danger(error.to_string(), cx));
        Some(
            card(cx)
                .debug_selector(|| "settings-preset-editor".into())
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().text_sm().child(if draft.new {
                            format!("New preset {}", draft.name)
                        } else {
                            format!("Preset {}", draft.name)
                        }))
                        .child(
                            small_button("preset-save", "Save")
                                .primary()
                                .loading(saving)
                                .disabled(!dirty || saving)
                                .on_click(cx.listener(|this, _, window, cx| this.save_preset(window, cx))),
                        )
                        .child(
                            small_button("preset-close", "Close")
                                .on_click(cx.listener(|this, _, window, cx| this.close_preset(window, cx))),
                        ),
                )
                .when_some(draft.notice.clone(), |el, notice| el.child(warning(notice, cx)))
                .child(tabs)
                .children(modes)
                .children(editor)
                .children(problem)
                .when(!form_view, |el| {
                    el.child(detail(
                        "agent.yaml can name the oracle's model accounts in llm.accounts, Git credentials in git.credentials, and secrets in environment.secrets. The oracle checks the preset when you save it.",
                        cx,
                    ))
                    .child(detail(hint, cx))
                })
                .into_any_element(),
        )
    }

    pub(super) fn render_presets(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let now = now_unix();
        let presets = self.link.read(cx).presets.clone();
        let import = small_button("import-preset", "Import from this computer…")
            .on_click(cx.listener(|this, _, window, cx| this.import_preset(window, cx)))
            .into_any_element();
        let add = small_button("new-preset", "New preset…")
            .on_click(cx.listener(|this, _, window, cx| this.new_preset(window, cx)))
            .into_any_element();
        let mut page = v_flex().gap_2().child(header(
            "Presets",
            "Agent configurations for codespaces: an agent.yaml file, an AGENTS.md file, and skills. You select a preset when you make a codespace.",
            vec![import, add],
            cx,
        ));
        if let Some(draft) = self.render_draft(cx) {
            page = page.child(draft);
        }
        if presets.is_empty() {
            page = page.child(empty(
                "No presets. Codespaces use the default configuration.",
                cx,
            ));
        }
        for (ix, preset) in presets.iter().enumerate() {
            let mut facts = Vec::new();
            if !preset.model_accounts.is_empty() {
                facts.push(format!(
                    "accounts {}",
                    preset
                        .model_accounts
                        .iter()
                        .map(|(p, a)| format!("{p}: {a}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !preset.git_credentials.is_empty() {
                facts.push(format!("credentials {}", preset.git_credentials.join(", ")));
            }
            if !preset.secrets.is_empty() {
                facts.push(format!("{} secrets", preset.secrets.len()));
            }
            if !preset.files.is_empty() {
                facts.push(format!("{} skill files", preset.files.len()));
            }
            facts.push(format!("changed {}", age(now, preset.updated_at)));
            let loading = self.is_busy(&format!("preset:{}", preset.name));
            let (edit, duplicate, delete) = (
                preset.name.clone(),
                preset.name.clone(),
                preset.name.clone(),
            );
            page = page.child(
                card(cx)
                    .debug_selector(move || format!("settings-preset-{ix}"))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().text_sm().child(preset.name.clone()))
                            .child(
                                small_button(
                                    SharedString::from(format!("edit-preset-{ix}")),
                                    "Edit",
                                )
                                .loading(loading)
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.edit_preset(edit.clone(), window, cx)
                                    },
                                )),
                            )
                            .child(
                                small_button(
                                    SharedString::from(format!("duplicate-preset-{ix}")),
                                    "Duplicate…",
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.duplicate_preset(duplicate.clone(), window, cx)
                                    },
                                )),
                            )
                            .child(
                                small_button(
                                    SharedString::from(format!("delete-preset-{ix}")),
                                    "Delete…",
                                )
                                .danger()
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.delete_preset(delete.clone(), window, cx)
                                    },
                                )),
                            ),
                    )
                    .child(detail(facts.join(" · "), cx)),
            );
        }
        page.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_paths_are_inside_a_skill_directory() {
        assert!(valid_skill_path("skills/review/SKILL.md"));
        assert!(valid_skill_path("skills/review/scripts/run.sh"));
        assert!(!valid_skill_path("skills/SKILL.md"));
        assert!(!valid_skill_path("other/review/SKILL.md"));
        assert!(!valid_skill_path("skills/../x/SKILL.md"));
        assert!(!valid_skill_path("skills/review/"));
    }
}
