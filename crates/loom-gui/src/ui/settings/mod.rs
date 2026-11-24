mod audit;
mod devices;
mod editor;
mod form;
mod git;
mod hosts;
mod models;
mod presets;
mod qr;
mod workspaces;

use std::collections::{HashMap, HashSet};
use std::future::Future;

use gpui::{
    AnyElement, App, AsyncWindowContext, Context, Div, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Selectable, Sizable, h_flex, v_flex};
use loom_client::OracleClient;
use loom_client::oracle_proto::VolumeUsage;
use loom_config::Dirs;

use crate::oracle::{OracleEvent, OracleLink, OracleStatus};
use crate::ui::notify;

pub use form::{Done, Field, Values};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Devices,
    Hosts,
    GitProviders,
    Models,
    Secrets,
    Presets,
    Workspaces,
    Audit,
}

impl Page {
    pub const ALL: [Page; 8] = [
        Page::Devices,
        Page::Hosts,
        Page::GitProviders,
        Page::Models,
        Page::Secrets,
        Page::Presets,
        Page::Workspaces,
        Page::Audit,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Page::Devices => "Devices",
            Page::Hosts => "Hosts",
            Page::GitProviders => "Git providers",
            Page::Models => "Models",
            Page::Secrets => "Secrets",
            Page::Presets => "Presets",
            Page::Workspaces => "Workspaces",
            Page::Audit => "Audit log",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Page::Devices => "settings-page-devices",
            Page::Hosts => "settings-page-hosts",
            Page::GitProviders => "settings-page-git",
            Page::Models => "settings-page-models",
            Page::Secrets => "settings-page-secrets",
            Page::Presets => "settings-page-presets",
            Page::Workspaces => "settings-page-workspaces",
            Page::Audit => "settings-page-audit",
        }
    }
}

pub struct SettingsView {
    link: Entity<OracleLink>,
    dirs: Dirs,
    page: Page,
    focus: FocusHandle,
    pairing: Option<devices::ShownPairing>,
    host_sizes: HashMap<String, Result<Vec<VolumeUsage>, String>>,
    scripts: HashMap<String, editor::TextFile>,
    busy: HashSet<String>,
    login: Option<models::PendingLogin>,
    preset: Option<presets::PresetDraft>,
    workspace: Option<workspaces::WorkspaceDraft>,
    audit: audit::AuditState,
    _subscriptions: Vec<Subscription>,
}

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn call<T, F, Fut>(
    link: &Entity<OracleLink>,
    cx: &App,
    f: F,
) -> impl Future<Output = Result<T, String>> + 'static
where
    F: FnOnce(OracleClient) -> Fut,
    Fut: Future<Output = loom_client::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let link = link.read(cx);
    let task = link.client().map(|client| link.backend().run(f(client)));
    async move {
        let Some(task) = task else {
            return Err("The oracle is not connected.".to_owned());
        };
        match task.await {
            Some(Ok(value)) => Ok(value),
            Some(Err(err)) => Err(err.to_string()),
            None => Err("The request stopped.".to_owned()),
        }
    }
}

impl SettingsView {
    pub fn new(
        link: Entity<OracleLink>,
        dirs: Dirs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&link, |_, _, cx| cx.notify()),
            cx.subscribe_in(&link, window, |this, _, event: &OracleEvent, window, cx| {
                if let OracleEvent::LoginFinished {
                    login,
                    account,
                    error,
                } = event
                {
                    this.login_finished(login, account.as_ref(), error.as_deref(), window, cx);
                }
            }),
        ];
        Self {
            link,
            dirs,
            page: Page::Devices,
            focus: cx.focus_handle(),
            pairing: None,
            host_sizes: HashMap::new(),
            scripts: HashMap::new(),
            busy: HashSet::new(),
            login: None,
            preset: None,
            workspace: None,
            audit: audit::AuditState::default(),
            _subscriptions: subscriptions,
        }
    }

    pub fn oracle(&self) -> &Entity<OracleLink> {
        &self.link
    }

    pub fn oracle_name(&self, cx: &App) -> String {
        self.link.read(cx).name().to_owned()
    }

    pub fn page(&self) -> Page {
        self.page
    }

    pub fn show(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        self.page = page;
        if page == Page::Hosts {
            self.load_host_sizes(window, cx);
        }
        if page == Page::Audit {
            self.load_audit(false, window, cx);
        }
        cx.notify();
    }

    pub fn has_unsaved(&self, cx: &App) -> bool {
        self.preset.as_ref().is_some_and(|p| p.is_dirty(cx))
            || self.scripts.values().any(|s| s.is_dirty(cx))
            || self.workspace.as_ref().is_some_and(|w| w.is_dirty(cx))
    }

    fn call<T, F, Fut>(&self, cx: &App, f: F) -> impl Future<Output = Result<T, String>> + 'static
    where
        F: FnOnce(OracleClient) -> Fut,
        Fut: Future<Output = loom_client::Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        call(&self.link, cx, f)
    }

    fn is_busy(&self, key: &str) -> bool {
        self.busy.contains(key)
    }

    fn act<T: 'static>(
        &mut self,
        key: Option<String>,
        task: impl Future<Output = Result<T, String>> + 'static,
        then: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(key) = &key
            && !self.busy.insert(key.clone())
        {
            return;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(key) = &key {
                    this.busy.remove(key);
                }
                match result {
                    Ok(value) => then(this, value, window, cx),
                    Err(error) => notify::error(window, cx, error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_then(
        &self,
        title: String,
        description: String,
        ok: &'static str,
        run: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        crate::ui::dialogs::confirm(
            title,
            description,
            ok,
            move |window, cx| {
                let _ = this.update(cx, |view, cx| run(view, window, cx));
            },
            window,
            cx,
        );
    }

    fn render_nav(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let link = self.link.read(cx);
        let status = match &link.status {
            OracleStatus::Connecting => "connecting…".to_owned(),
            OracleStatus::Connected => "connected".to_owned(),
            OracleStatus::Offline(reason) => format!("offline: {reason}"),
        };
        let mut nav = v_flex()
            .w(px(190.))
            .flex_none()
            .h_full()
            .gap_0p5()
            .p_2()
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .px_2()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(link.name().to_owned()),
            )
            .child(
                div()
                    .px_2()
                    .pb_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child(status),
            );
        for page in Page::ALL {
            nav = nav.child(
                div().debug_selector(move || page.id().into()).child(
                    Button::new(page.id())
                        .ghost()
                        .small()
                        .w_full()
                        .justify_start()
                        .label(page.label())
                        .selected(self.page == page)
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.show(page, window, cx)),
                        ),
                ),
            );
        }
        nav.into_any_element()
    }
}

pub(crate) fn header(
    title: impl Into<SharedString>,
    description: impl Into<SharedString>,
    actions: Vec<AnyElement>,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    v_flex()
        .gap_1()
        .pb_2()
        .child(
            h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.into()),
                )
                .children(actions),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .whitespace_normal()
                .child(description.into()),
        )
}

pub(crate) fn subheader(title: impl Into<SharedString>, actions: Vec<AnyElement>) -> Div {
    h_flex()
        .pt_3()
        .gap_2()
        .child(
            div()
                .flex_1()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.into()),
        )
        .children(actions)
}

pub(crate) fn card(cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .w_full()
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
}

pub(crate) fn detail(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .whitespace_normal()
        .child(text.into())
}

pub(crate) fn warning(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().warning)
        .whitespace_normal()
        .child(text.into())
}

pub(crate) fn danger(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().danger)
        .whitespace_normal()
        .child(text.into())
}

pub(crate) fn empty(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .py_2()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .whitespace_normal()
        .child(text.into())
}

pub(crate) fn small_button(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Button {
    let id: SharedString = id.into();
    Button::new(id).outline().xsmall().label(label.into())
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let content = match self.page {
            Page::Devices => self.render_devices(window, cx),
            Page::Hosts => self.render_hosts(window, cx),
            Page::GitProviders => self.render_git(window, cx),
            Page::Models => self.render_models(window, cx),
            Page::Secrets => self.render_secrets(window, cx),
            Page::Presets => self.render_presets(window, cx),
            Page::Workspaces => self.render_workspaces(window, cx),
            Page::Audit => self.render_audit(window, cx),
        };
        h_flex()
            .id("settings")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .child(self.render_nav(cx))
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(960.))
                            .gap_2()
                            .p_4()
                            .child(content),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, TestAppContext, VisualTestContext};
    use gpui_component::{Root, WindowExt};
    use loom_client::oracle_proto::{HostCall, HostReport, Role, VolumeInput, VolumeKind};
    use loom_client::{ClientIdentity, OracleClient, pair_oracle};
    use serde_json::json;

    use super::*;
    use crate::oracle::tests::{TestOracle, wait};

    fn click(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("no {selector}"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
    }

    fn connect_host(test: &TestOracle) -> OracleClient {
        let link = test
            .oracle
            .create_pairing(Role::Host, std::time::Duration::from_secs(60), None)
            .unwrap()
            .link;
        futures::executor::block_on(test.backend.run(async move {
            let identity = ClientIdentity::generate().unwrap();
            let endpoint = pair_oracle(&link, &identity, "box", None).await.unwrap();
            let (host, _events, calls) =
                OracleClient::connect_with_calls(&endpoint, &identity, "test-host")
                    .await
                    .unwrap();
            host.host_report(HostReport {
                version: "test".into(),
                driver: "process".into(),
                cpus: 4,
                memory_mib: 8192,
                ..Default::default()
            })
            .await
            .unwrap();
            let mut calls = calls;
            let answer = host.clone();
            tokio::spawn(async move {
                while let Some((id, call)) = calls.recv().await {
                    let reply = match call {
                        HostCall::Check { docker } => Ok(json!({"checks": [
                            {"name": "driver", "status": "ok", "detail": "process"},
                            {"name": "docker", "status": if docker { "ok" } else { "warning" }, "detail": "not tested"},
                        ]})),
                        HostCall::CheckWorkspace(_) => Ok(json!({"volumes": true})),
                        _ => Ok(json!({})),
                    };
                    let _ = answer.reply(id, reply).await;
                }
            });
            host
        }))
        .unwrap()
    }

    #[gpui::test]
    fn the_settings_manage_the_oracle(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let test = TestOracle::start(&root);
        let _host = connect_host(&test);
        let config = test.env.dirs.config_dir();
        std::fs::create_dir_all(config.join("skills/review")).unwrap();
        std::fs::write(
            config.join("agent.yaml"),
            "socket: /tmp/loom.sock\npolicy:\n  default: ask\n",
        )
        .unwrap();
        std::fs::write(config.join("AGENTS.md"), "Be brief.\n").unwrap();
        std::fs::write(config.join("skills/review/SKILL.md"), "# Review\n").unwrap();

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::ui::bind_keys(&Default::default(), cx);
        });
        let (entry, env, backend) = (test.entry.clone(), test.env.clone(), test.backend.clone());
        let dirs = env.dirs.clone();
        let link = cx.new(|cx| OracleLink::new(entry, env, backend, cx));
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let (slot, link) = (slot.clone(), link.clone());
            move |window, cx| {
                let view = cx.new(|cx| SettingsView::new(link, dirs, window, cx));
                *slot.borrow_mut() = Some(view.clone());
                Root::new(view, window, cx)
            }
        });
        let view: Entity<SettingsView> = slot.borrow_mut().take().unwrap();
        wait(cx, |cx| {
            let link = link.read(cx);
            link.hosts.iter().any(|h| h.connected) && !link.config.devices.is_empty()
        });

        click("settings-page-hosts", cx);
        assert_eq!(view.read_with(cx, |v, _| v.page()), Page::Hosts);
        assert!(cx.debug_bounds("settings-host-0").is_some());
        view.update_in(cx, |view, window, cx| {
            view.check_host("box".into(), false, window, cx)
        });
        wait(cx, |cx| link.read(cx).hosts[0].checks.len() == 2);
        assert!(
            test.oracle.hosts().unwrap()[0].checked_at.is_some(),
            "the oracle keeps the checks"
        );

        click("settings-page-devices", cx);
        view.update_in(cx, |view, window, cx| {
            view.create_pairing(Role::Device, window, cx)
        });
        wait(cx, |cx| view.read(cx).pairing.is_some());
        wait(cx, |cx| !link.read(cx).config.pairings.is_empty());
        cx.run_until_parked();
        assert!(cx.debug_bounds("settings-pairing").is_some());
        assert!(cx.debug_bounds("settings-qr").is_some());

        click("settings-page-presets", cx);
        view.update_in(cx, |view, window, cx| view.import_preset(window, cx));
        cx.run_until_parked();
        click("form-submit", cx);
        let draft = view.read_with(cx, |view, cx| {
            let draft = view.preset.as_ref().expect("no draft");
            (
                draft.name.clone(),
                draft.notice.clone(),
                draft.text_of(presets::AGENT_YAML, cx),
                draft.files.len(),
            )
        });
        assert_eq!(draft.0, "local");
        assert!(draft.1.unwrap().contains("socket"));
        assert!(!draft.2.contains("socket"), "{}", draft.2);
        assert_eq!(draft.3, 3);
        assert!(view.read_with(cx, |view, cx| view.has_unsaved(cx)));
        view.update_in(cx, |view, window, cx| view.save_preset(window, cx));
        wait(cx, |cx| {
            link.read(cx).presets.iter().any(|p| p.name == "local")
        });
        wait(cx, |cx| !view.read(cx).has_unsaved(cx));
        let preset = test.oracle.preset_detail("local").unwrap();
        assert_eq!(preset.preset.agents_md, "Be brief.\n");
        assert_eq!(preset.files[0].path, "skills/review/SKILL.md");

        click("settings-page-secrets", cx);
        view.update_in(cx, |view, window, cx| view.edit_secret(None, window, cx));
        cx.run_until_parked();
        cx.simulate_input("npm-work");
        click("form-submit", cx);
        assert!(cx.debug_bounds("form-error").is_some());
        assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
        cx.update(|window, cx| window.close_dialog(cx));
        cx.run_until_parked();

        click("settings-page-workspaces", cx);
        test.oracle
            .create_volume(
                &VolumeInput {
                    name: "cache".into(),
                    kind: VolumeKind::Incus,
                    size_mib: None,
                    path: None,
                },
                None,
            )
            .unwrap();
        wait(cx, |cx| !link.read(cx).config.volumes.is_empty());
        view.update_in(cx, |view, window, cx| view.new_workspace(window, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("settings-workspace-editor").is_some());
        cx.simulate_input("https://github.com/acme/Web_Server.git");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| view.add_repository(window, cx));
        cx.run_until_parked();
        cx.simulate_input("https://github.com/acme/web.git");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| view.add_mount(window, cx));
        cx.run_until_parked();
        cx.simulate_input("/home/dev/.cache");
        click("form-submit", cx);
        let (name, repositories, volumes) = view.read_with(cx, |view, cx| {
            let draft = view.workspace.as_ref().expect("no draft");
            (
                draft.typed_name(cx),
                draft.repositories.len(),
                draft.volumes.len(),
            )
        });
        assert_eq!((name.as_str(), repositories, volumes), ("web-server", 2, 1));
        view.update_in(cx, |view, window, cx| view.save_workspace(window, cx));
        wait(cx, |cx| {
            link.read(cx)
                .workspaces
                .iter()
                .any(|w| w.name == "web-server")
        });
        let workspace = &test.oracle.workspaces().unwrap()[0];
        assert_eq!(workspace.host, "box");
        assert_eq!(workspace.repositories.len(), 2);
        assert_eq!(workspace.volumes[0].path, "/home/dev/.cache");
        assert!(view.read_with(cx, |view, _| view.workspace.is_none()));
        assert!(cx.debug_bounds("settings-workspace-0").is_some());
    }
}
