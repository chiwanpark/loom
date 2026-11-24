use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription,
    WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::{ActiveTheme, Disableable, Sizable, WindowExt, h_flex, v_flex};
use loom_client::oracle_proto::{
    RemoteRepository, Repository, VolumeInput, VolumeKind, Workspace, WorkspaceVolume, valid_name,
};
use loom_view_model::codespaces::parse_memory;
use loom_view_model::settings::{age, mount_label, repository_label, suggest_name, volume_label};

use super::form::{self, Field};
use super::{SettingsView, card, danger, detail, empty, header, now_unix, small_button, subheader};
use crate::oracle::OracleLink;
use crate::ui::notify;

pub struct WorkspaceDraft {
    pub name: String,
    pub host: String,
    pub repositories: Vec<Repository>,
    pub volumes: Vec<WorkspaceVolume>,
    pub original: Workspace,
}

impl WorkspaceDraft {
    pub fn is_dirty(&self) -> bool {
        let w = &self.original;
        w.host != self.host || w.repositories != self.repositories || w.volumes != self.volumes
    }
}

type AddRepository = Rc<dyn Fn(Repository, &mut Window, &mut App)>;

pub struct RepositorySearch {
    link: Entity<OracleLink>,
    providers: Vec<String>,
    provider: String,
    query: Entity<InputState>,
    results: Vec<RemoteRepository>,
    errors: Vec<String>,
    more: bool,
    searching: bool,
    searched: bool,
    add: AddRepository,
    _subscription: Subscription,
}

impl RepositorySearch {
    fn search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query.read(cx).value().trim().to_owned();
        if query.starts_with("https://") || query.starts_with("git@") || query.starts_with("ssh://")
        {
            window.close_dialog(cx);
            let add = self.add.clone();
            add(
                Repository {
                    url: query,
                    provider: None,
                    directory: None,
                    base_branch: None,
                },
                window,
                cx,
            );
            return;
        }
        if self.provider.is_empty() {
            self.errors =
                vec!["Add a Git provider first, or give the URL of the repository.".to_owned()];
            cx.notify();
            return;
        }
        self.searching = true;
        cx.notify();
        let provider = self.provider.clone();
        let task = super::call(&self.link, cx, move |client| async move {
            client.search_repositories(&provider, &query).await
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.searching = false;
                this.searched = true;
                match result {
                    Ok(found) => {
                        this.results = found.repositories;
                        this.errors = found.errors;
                        this.more = found.more;
                    }
                    Err(error) => {
                        this.results.clear();
                        this.errors = vec![error];
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for RepositorySearch {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let providers = self.providers.clone();
        let current = self.provider.clone();
        let this = cx.entity().downgrade();
        let mut list = v_flex().gap_1();
        for (ix, repository) in self.results.iter().enumerate() {
            let add = self.add.clone();
            let picked = Repository {
                url: repository.url.clone(),
                provider: Some(self.provider.clone()),
                directory: None,
                base_branch: None,
            };
            list = list.child(
                h_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().child(format!(
                                "{}/{}{}",
                                repository.owner,
                                repository.name,
                                if repository.private { " (private)" } else { "" }
                            )))
                            .when_some(repository.description.clone(), |el, text| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(text),
                                )
                            }),
                    )
                    .child(
                        small_button(SharedString::from(format!("search-add-{ix}")), "Add")
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                add(picked.clone(), window, cx);
                            }),
                    ),
            );
        }
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("search-provider")
                            .outline()
                            .small()
                            .label(if current.is_empty() { "No provider".to_owned() } else { current.clone() })
                            .dropdown_menu(move |mut menu, _, _| {
                                for provider in &providers {
                                    let this = this.clone();
                                    let picked = provider.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(provider.clone())
                                            .checked(*provider == current)
                                            .on_click(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                                                let _ = this.update(cx, |this, cx| {
                                                    this.provider = picked.clone();
                                                    cx.notify();
                                                });
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                    .child(div().flex_1().child(Input::new(&self.query)))
                    .child(
                        Button::new("search-run")
                            .primary()
                            .small()
                            .label("Search")
                            .loading(self.searching)
                            .disabled(self.searching)
                            .on_click(cx.listener(|this, _, window, cx| this.search(window, cx))),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Search the repositories that the Git credentials of the provider can read, or give the URL of a repository."),
            )
            .children(self.errors.iter().map(|e| {
                div().text_xs().text_color(theme.danger).whitespace_normal().child(e.clone())
            }))
            .when(self.searched && self.results.is_empty() && self.errors.is_empty(), |el| {
                el.child(div().text_sm().child("No repositories found."))
            })
            .child(
                div()
                    .id("search-results")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .child(list),
            )
            .when(self.more, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("There are more results. Give a longer query."),
                )
            })
    }
}

fn open_search(link: Entity<OracleLink>, add: AddRepository, window: &mut Window, cx: &mut App) {
    let providers: Vec<String> = link
        .read(cx)
        .config
        .providers
        .iter()
        .map(|p| p.name.clone())
        .collect();
    let view = cx.new(|cx| {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("owner/name, or a URL"));
        let subscription = cx.subscribe_in(
            &query,
            window,
            |this: &mut RepositorySearch, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.search(window, cx);
                }
            },
        );
        RepositorySearch {
            link,
            provider: providers.first().cloned().unwrap_or_default(),
            providers,
            query,
            results: Vec::new(),
            errors: Vec::new(),
            more: false,
            searching: false,
            searched: false,
            add,
            _subscription: subscription,
        }
    });
    let query = view.read(cx).query.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Add repository")
            .w(px(640.))
            .child(view.clone())
    });
    window.defer(cx, move |window, cx| {
        query.update(cx, |input, cx| input.focus(window, cx));
    });
}

fn draft_update(
    this: &WeakEntity<SettingsView>,
    cx: &mut App,
    f: impl FnOnce(&mut WorkspaceDraft),
) {
    let _ = this.update(cx, |view, cx| {
        if let Some(draft) = &mut view.workspace {
            f(draft);
        }
        cx.notify();
    });
}

impl SettingsView {
    fn with_workspace_closed(
        &mut self,
        run: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .workspace
            .as_ref()
            .is_some_and(WorkspaceDraft::is_dirty)
        {
            self.confirm_then(
                "Discard your changes?".to_owned(),
                "The changes to the open workspace are not saved.".to_owned(),
                "Discard",
                move |this, window, cx| {
                    this.workspace = None;
                    run(this, window, cx);
                },
                window,
                cx,
            );
        } else {
            self.workspace = None;
            run(self, window, cx);
        }
    }

    fn new_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.with_workspace_closed(
            |this, window, cx| {
                if this.link.read(cx).hosts.is_empty() {
                    return notify::error(window, cx, "Add a host first.");
                }
                let view = cx.entity().downgrade();
                open_search(
                    this.link.clone(),
                    Rc::new(move |repository, window, cx| {
                        let _ = view.update(cx, |view, cx| {
                            view.create_workspace_for(repository, window, cx)
                        });
                    }),
                    window,
                    cx,
                );
            },
            window,
            cx,
        );
    }

    pub(super) fn create_workspace_for(
        &mut self,
        repository: Repository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let link = self.link.read(cx);
        let mut hosts: Vec<&loom_client::oracle_proto::Host> = link.hosts.iter().collect();
        hosts.sort_by_key(|h| !h.connected);
        let hosts: Vec<String> = hosts.into_iter().map(|h| h.name.clone()).collect();
        let taken: Vec<String> = link.workspaces.iter().map(|w| w.name.clone()).collect();
        let suggested = suggest_name(&loom_view_model::codespaces::directory(&repository), &taken);
        let link = self.link.clone();
        form::open(
            "New workspace",
            Some(format!(
                "A workspace with {}. Each codespace of the workspace clones it. To add more repositories or volumes, edit the workspace later.",
                repository.url
            )),
            vec![
                Field::text("Name").value(suggested),
                Field::choice("Host", hosts),
            ],
            "Create",
            move |values, done, window, cx| {
                let name = values.text(0);
                if !valid_name(&name) {
                    return done.fail("Use lowercase letters, digits, and dashes.", window, cx);
                }
                if taken.contains(&name) {
                    return done.fail(format!("A workspace {name} is there already."), window, cx);
                }
                let host = values.text(1);
                let repositories = vec![repository.clone()];
                let task = super::call(&link, cx, move |client| async move {
                    client.create_workspace(&name, &host, repositories).await
                });
                done.wait(
                    task,
                    |workspace, window, cx| {
                        notify::success(
                            window,
                            cx,
                            format!("Made the workspace {}.", workspace.name),
                        )
                    },
                    window,
                    cx,
                );
            },
            window,
            cx,
        );
    }

    fn edit_workspace(
        &mut self,
        workspace: Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_workspace_closed(
            move |this, _, cx| {
                this.workspace = Some(WorkspaceDraft {
                    name: workspace.name.clone(),
                    host: workspace.host.clone(),
                    repositories: workspace.repositories.clone(),
                    volumes: workspace.volumes.clone(),
                    original: workspace.clone(),
                });
                cx.notify();
            },
            window,
            cx,
        );
    }

    fn save_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.workspace else {
            return;
        };
        if draft.repositories.is_empty() {
            return notify::error(window, cx, "Add a repository.");
        }
        let (name, host) = (draft.name.clone(), draft.host.clone());
        let (repositories, volumes) = (draft.repositories.clone(), draft.volumes.clone());
        let workspace = name.clone();
        let task = self.call(cx, move |client| async move {
            client
                .update_workspace(&workspace, Some(&host), Some(repositories), Some(volumes))
                .await
        });
        self.act(
            Some(format!("save-workspace:{name}")),
            task,
            |this, workspace, window, cx| {
                if this
                    .workspace
                    .as_ref()
                    .is_some_and(|d| d.name == workspace.name)
                {
                    this.workspace = None;
                }
                notify::success(
                    window,
                    cx,
                    format!("Saved the workspace {}.", workspace.name),
                );
            },
            window,
            cx,
        );
    }

    fn delete_workspace(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_then(
            format!("Delete the workspace {name}?"),
            "The oracle refuses this while the workspace has open codespaces.".to_owned(),
            "Delete",
            move |this, window, cx| {
                let workspace = name.clone();
                let task = this.call(cx, move |client| async move {
                    client.delete_workspace(&workspace).await
                });
                this.act(None, task, |_, _, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn add_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        open_search(
            self.link.clone(),
            Rc::new(move |repository, _, cx| {
                draft_update(&this, cx, |draft| {
                    if !draft.repositories.iter().any(|r| r.url == repository.url) {
                        draft.repositories.push(repository);
                    }
                })
            }),
            window,
            cx,
        );
    }

    fn edit_repository(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self
            .workspace
            .as_ref()
            .and_then(|d| d.repositories.get(ix))
            .cloned()
        else {
            return;
        };
        let this = cx.entity().downgrade();
        form::open(
            "Repository",
            Some(repository.url.clone()),
            vec![
                Field::text("Directory")
                    .value(repository.directory.clone().unwrap_or_default())
                    .placeholder(loom_view_model::codespaces::directory(&repository))
                    .help("The directory of the clone in /workspaces. Empty for the name of the repository."),
                Field::text("Base branch")
                    .value(repository.base_branch.clone().unwrap_or_default())
                    .placeholder("main")
                    .help("Empty for the default branch of the repository."),
            ],
            "Save",
            move |values, done, window, cx| {
                let (directory, branch) = (values.optional(0), values.optional(1));
                draft_update(&this, cx, |draft| {
                    if let Some(repository) = draft.repositories.get_mut(ix) {
                        repository.directory = directory;
                        repository.base_branch = branch;
                    }
                });
                done.finish(Ok(()), window, cx);
            },
            window,
            cx,
        );
    }

    fn add_mount(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let volumes: Vec<String> = self
            .link
            .read(cx)
            .config
            .volumes
            .iter()
            .filter(|v| !v.deleting)
            .map(|v| v.name.clone())
            .collect();
        if volumes.is_empty() {
            return notify::error(window, cx, "Make a volume first.");
        }
        let this = cx.entity().downgrade();
        form::open(
            "Add volume",
            None,
            vec![
                Field::choice("Volume", volumes),
                Field::text("Path")
                    .placeholder("/home/dev/.cache")
                    .help("An absolute path in the codespace. It cannot be in /workspaces."),
                Field::check("Read-only", false),
            ],
            "Add",
            move |values, done, window, cx| {
                let path = values.text(1);
                if !path.starts_with('/') {
                    return done.fail("Give an absolute path.", window, cx);
                }
                let mount = WorkspaceVolume {
                    volume: values.text(0),
                    path,
                    read_only: values.checked(2),
                };
                draft_update(&this, cx, |draft| draft.volumes.push(mount));
                done.finish(Ok(()), window, cx);
            },
            window,
            cx,
        );
    }

    fn render_workspace_draft(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let draft = self.workspace.as_ref()?;
        let hosts: Vec<String> = self
            .link
            .read(cx)
            .hosts
            .iter()
            .map(|h| h.name.clone())
            .collect();
        let saving = self.is_busy(&format!("save-workspace:{}", draft.name));
        let current = draft.host.clone();
        let this = cx.entity().downgrade();
        let mut el = card(cx)
            .debug_selector(|| "settings-workspace-editor".into())
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .child(format!("Workspace {}", draft.name)),
                    )
                    .child(
                        small_button("workspace-save", "Save")
                            .primary()
                            .loading(saving)
                            .disabled(saving || !draft.is_dirty())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_workspace(window, cx)),
                            ),
                    )
                    .child(
                        small_button("workspace-close", "Close").on_click(cx.listener(
                            |this, _, window, cx| {
                                this.with_workspace_closed(|_, _, cx| cx.notify(), window, cx)
                            },
                        )),
                    ),
            )
            .child(
                h_flex().gap_2().child(div().text_sm().child("Host")).child(
                    Button::new("workspace-host")
                        .outline()
                        .xsmall()
                        .label(current.clone())
                        .dropdown_menu(move |mut menu, _, _| {
                            for host in &hosts {
                                let this = this.clone();
                                let picked = host.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(host.clone())
                                        .checked(*host == current)
                                        .on_click(
                                            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                                                let picked = picked.clone();
                                                draft_update(&this, cx, |draft| draft.host = picked)
                                            },
                                        ),
                                );
                            }
                            menu
                        }),
                ),
            )
            .child(subheader(
                "Repositories",
                vec![
                    small_button("workspace-add-repository", "Add repository…")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_repository(window, cx)),
                        )
                        .into_any_element(),
                ],
            ));
        if draft.repositories.is_empty() {
            el = el.child(detail(
                "No repositories. A workspace needs at least one.",
                cx,
            ));
        }
        for (ix, repository) in draft.repositories.iter().enumerate() {
            el = el.child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .whitespace_normal()
                            .child(repository_label(repository)),
                    )
                    .child(
                        small_button(
                            SharedString::from(format!("workspace-edit-repository-{ix}")),
                            "Edit…",
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.edit_repository(ix, window, cx),
                        )),
                    )
                    .child(
                        small_button(
                            SharedString::from(format!("workspace-remove-repository-{ix}")),
                            "Remove",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(draft) = &mut this.workspace
                                && ix < draft.repositories.len()
                            {
                                draft.repositories.remove(ix);
                            }
                            cx.notify();
                        })),
                    ),
            );
        }
        el = el.child(subheader(
            "Volumes",
            vec![
                small_button("workspace-add-volume", "Add volume…")
                    .on_click(cx.listener(|this, _, window, cx| this.add_mount(window, cx)))
                    .into_any_element(),
            ],
        ));
        if draft.volumes.is_empty() {
            el = el.child(detail("No volumes.", cx));
        }
        for (ix, mount) in draft.volumes.iter().enumerate() {
            el = el.child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().text_sm().child(mount_label(mount)))
                    .child(
                        small_button(
                            SharedString::from(format!("workspace-remove-volume-{ix}")),
                            "Remove",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(draft) = &mut this.workspace
                                && ix < draft.volumes.len()
                            {
                                draft.volumes.remove(ix);
                            }
                            cx.notify();
                        })),
                    ),
            );
        }
        el = el.child(detail(
            "A change applies to new codespaces. Open codespaces keep their repositories and volumes.",
            cx,
        ));
        Some(el.into_any_element())
    }

    fn new_volume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let link = self.link.clone();
        form::open(
            "New volume",
            Some("A volume keeps its files when codespaces close. An Incus volume is a custom storage volume on the host. A host directory is a directory on the host.".to_owned()),
            vec![
                Field::text("Name").placeholder("cache"),
                Field::choice("Kind", vec!["incus".to_owned(), "directory".to_owned()]),
                Field::text("Size limit")
                    .placeholder("20GiB")
                    .help("Only for an Incus volume. Empty for no limit."),
                Field::text("Host path")
                    .placeholder("/srv/data")
                    .help("Only for a host directory: an absolute path on the host. Empty for a directory that loom-host makes."),
            ],
            "Create",
            move |values, done, window, cx| {
                let name = values.text(0);
                if !valid_name(&name) {
                    return done.fail("Use lowercase letters, digits, and dashes.", window, cx);
                }
                let kind = VolumeKind::parse(&values.text(1));
                let size_mib = match values.optional(2) {
                    None => None,
                    Some(text) => match parse_memory(&text) {
                        Some(mib) => Some(mib),
                        None => return done.fail(format!("{text:?} is not a size such as 20GiB."), window, cx),
                    },
                };
                let input = VolumeInput {
                    name,
                    kind,
                    size_mib: size_mib.filter(|_| kind == VolumeKind::Incus),
                    path: values.optional(3).filter(|_| kind == VolumeKind::Directory),
                };
                let task = super::call(&link, cx, move |client| async move {
                    client.create_volume(input).await
                });
                done.wait(task, |_, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn delete_volume(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_then(
            format!("Delete the volume {name}?"),
            "The hosts delete the files of the volume. The oracle refuses this while a workspace uses the volume.".to_owned(),
            "Delete",
            move |this, window, cx| {
                let volume = name.clone();
                let task = this.call(cx, move |client| async move { client.delete_volume(&volume).await });
                this.act(None, task, |_, _, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    pub(super) fn render_workspaces(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = now_unix();
        let link = self.link.read(cx);
        let workspaces = link.workspaces.clone();
        let volumes = link.config.volumes.clone();
        let hosts = link.hosts.clone();
        let codespaces = link.codespaces.clone();
        let add = small_button("new-workspace", "New workspace…")
            .on_click(cx.listener(|this, _, window, cx| this.new_workspace(window, cx)))
            .into_any_element();
        let mut page = v_flex().gap_2().child(header(
            "Workspaces",
            "The repositories and volumes of each workspace, and its host.",
            vec![add],
            cx,
        ));
        if let Some(draft) = self.render_workspace_draft(cx) {
            page = page.child(draft);
        }
        if workspaces.is_empty() {
            page = page.child(empty("No workspaces.", cx));
        }
        for (ix, workspace) in workspaces.iter().enumerate() {
            let open = codespaces
                .iter()
                .filter(|c| {
                    c.workspace == workspace.name && loom_view_model::codespaces::is_listed(c)
                })
                .count();
            let online = hosts
                .iter()
                .any(|h| h.name == workspace.host && h.connected);
            let (edit, delete) = (workspace.clone(), workspace.name.clone());
            let mut el = card(cx)
                .debug_selector(move || format!("settings-workspace-{ix}"))
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().text_sm().child(workspace.name.clone()))
                        .child(
                            small_button(
                                SharedString::from(format!("edit-workspace-{ix}")),
                                "Edit",
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.edit_workspace(edit.clone(), window, cx)
                                },
                            )),
                        )
                        .child(
                            small_button(
                                SharedString::from(format!("delete-workspace-{ix}")),
                                "Delete…",
                            )
                            .danger()
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.delete_workspace(delete.clone(), window, cx)
                                },
                            )),
                        ),
                )
                .child(detail(
                    format!(
                        "host {} · {open} open codespaces · made {}",
                        workspace.host,
                        age(now, workspace.created_at)
                    ),
                    cx,
                ));
            if !online {
                el = el.child(danger(
                    format!("The host {} is offline.", workspace.host),
                    cx,
                ));
            }
            for repository in &workspace.repositories {
                el = el.child(detail(repository_label(repository), cx));
            }
            for mount in &workspace.volumes {
                el = el.child(detail(format!("volume {}", mount_label(mount)), cx));
            }
            page = page.child(el);
        }
        page = page.child(subheader(
            "Volumes",
            vec![
                small_button("new-volume", "New volume…")
                    .on_click(cx.listener(|this, _, window, cx| this.new_volume(window, cx)))
                    .into_any_element(),
            ],
        ));
        if volumes.is_empty() {
            page = page.child(empty("No volumes.", cx));
        }
        for (ix, volume) in volumes.iter().enumerate() {
            let used: Vec<String> = workspaces
                .iter()
                .filter(|w| w.volumes.iter().any(|m| m.volume == volume.name))
                .map(|w| w.name.clone())
                .collect();
            let delete = volume.name.clone();
            let mut facts = vec![volume_label(volume)];
            facts.push(if volume.hosts.is_empty() {
                "on no host yet".to_owned()
            } else {
                format!("on {}", volume.hosts.join(", "))
            });
            facts.push(if used.is_empty() {
                "no workspaces".to_owned()
            } else {
                format!("workspaces {}", used.join(", "))
            });
            page = page.child(
                card(cx)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().text_sm().child(volume.name.clone()))
                            .child(
                                small_button(
                                    SharedString::from(format!("delete-volume-{ix}")),
                                    "Delete…",
                                )
                                .danger()
                                .disabled(volume.deleting)
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.delete_volume(delete.clone(), window, cx)
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
