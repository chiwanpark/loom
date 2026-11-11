use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext, AsyncWindowContext, ClipboardEntry, Context, Entity,
    ExternalPaths, FocusHandle, Focusable, Image, InteractiveElement, IntoElement, ObjectFit,
    ParentElement, PathPromptOptions, Render, SharedString, StatefulInteractiveElement, Styled,
    StyledImage, Subscription, Window, div, img, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{InputEvent, Paste, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Sizable, h_flex, v_flex};
use loom_client::proto::{
    ContentPart, Decision, PolicyAction, ReasoningEffort, SessionId, UsageLimit,
};

use crate::attachments::{self, AttachError};
use crate::conversation::Conversation;
use crate::state::{AgentConnection, UsageState};
use crate::ui::dialogs;
use crate::ui::messages::{self, MessageList, MessagesEvent};
use crate::ui::notify;

pub struct SessionView {
    pub connection: Entity<AgentConnection>,
    pub conversation: Entity<Conversation>,
    messages: Entity<MessageList>,
    input: Entity<TextareaState>,
    attachments: Vec<Arc<Image>>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl SessionView {
    pub fn new(
        connection: Entity<AgentConnection>,
        conversation: Entity<Conversation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .placeholder("Ask the agent… (Enter to send, Shift+Enter for a new line)")
                .submit_on_enter(true);
            state.set_auto_grow(2, 12, cx);
            state
        });
        let messages = cx.new(|cx| MessageList::new(conversation.clone(), cx));
        let subscriptions = vec![
            cx.observe(&conversation, |_, _, cx| cx.notify()),
            cx.observe(&connection, |_, _, cx| cx.notify()),
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.submit(window, cx);
                }
            }),
            cx.subscribe_in(
                &messages,
                window,
                |this, _, event: &MessagesEvent, window, cx| match event {
                    MessagesEvent::Resolve { call_id, decision } => {
                        this.resolve(call_id.clone(), *decision, window, cx)
                    }
                },
            ),
        ];
        Self {
            connection,
            conversation,
            messages,
            input,
            attachments: Vec::new(),
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    pub fn session_id(&self, cx: &App) -> SessionId {
        self.conversation.read(cx).info.id.clone()
    }

    pub fn title(&self, cx: &App) -> String {
        self.conversation.read(cx).title()
    }

    pub fn prompt(&self) -> Entity<TextareaState> {
        self.input.clone()
    }

    fn add_images(
        &mut self,
        images: Vec<Result<Arc<Image>, AttachError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for image in images {
            let checked = image.and_then(|image| {
                attachments::check_total(&self.attachments, &image).map(|()| image)
            });
            match checked {
                Ok(image) => self.attachments.push(image),
                Err(err) => notify::error(window, cx, err.to_string()),
            }
        }
        cx.notify();
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let task = cx.background_spawn(async move {
            paths
                .iter()
                .map(|path| attachments::from_path(path))
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let images = task.await;
            let _ = this.update_in(cx, |this, window, cx| this.add_images(images, window, cx));
        })
        .detach();
    }

    fn pick_images(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(
            window,
            async move |this, cx: &mut AsyncWindowContext| match paths.await {
                Ok(Ok(Some(paths))) => {
                    let _ =
                        this.update_in(cx, |this, window, cx| this.add_paths(paths, window, cx));
                }
                Ok(Err(err)) => {
                    let _ = cx.update(|window, cx| notify::error(window, cx, err.to_string()));
                }
                _ => {}
            },
        )
        .detach();
    }

    fn on_paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let images: Vec<_> = item
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(attachments::from_image(image)),
                _ => None,
            })
            .collect();
        if images.is_empty() || item.text().is_some_and(|t| !t.is_empty()) {
            return;
        }
        cx.stop_propagation();
        self.add_images(images, window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentPart::text(text));
        }
        content.extend(
            std::mem::take(&mut self.attachments)
                .into_iter()
                .map(|image| ContentPart::Image {
                    mime_type: image.format.mime_type().to_owned(),
                    data: image.bytes.clone(),
                }),
        );
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.messages.update(cx, |messages, cx| messages.follow(cx));
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let session = self.session_id(cx);
        let task = conn
            .backend
            .run(async move { client.send_prompt(&session, content).await });
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            if let Some(Err(err)) = task.await {
                let _ = cx.update(|window, cx| notify::error(window, cx, err.to_string()));
            }
        })
        .detach();
    }

    fn compact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let session = self.session_id(cx);
        let task = conn
            .backend
            .run(async move { client.compact_session(&session).await });
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            if let Some(Err(err)) = task.await {
                let _ = cx.update(|window, cx| notify::error(window, cx, err.to_string()));
            }
        })
        .detach();
    }

    fn interrupt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let session = self.session_id(cx);
        let task = conn
            .backend
            .run(async move { client.interrupt(&session).await });
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            if let Some(Err(err)) = task.await {
                let _ = cx.update(|window, cx| notify::error(window, cx, err.to_string()));
            }
        })
        .detach();
    }

    fn resolve(
        &mut self,
        call_id: String,
        decision: Decision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let session = self.session_id(cx);
        let task = conn
            .backend
            .run(async move { client.resolve_approval(&session, &call_id, decision).await });
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            if let Some(Err(err)) = task.await {
                let _ = cx.update(|window, cx| notify::error(window, cx, err.to_string()));
            }
        })
        .detach();
    }
}

pub fn open_rename(
    connection: Entity<AgentConnection>,
    session: SessionId,
    current: String,
    window: &mut Window,
    cx: &mut App,
) {
    dialogs::open_form(
        "Rename session",
        Some("Leave the title empty to name the session after its first prompt.".into()),
        vec![dialogs::FormField::new("Title", "Session title").value(current)],
        "Rename",
        move |values, _, cx| {
            let title = values.into_iter().next().unwrap_or_default();
            let title = (!title.trim().is_empty()).then_some(title);
            let session = session.clone();
            connection.update(cx, |conn, cx| conn.rename(session, title, cx));
        },
        window,
        cx,
    );
}

pub const REASONING_CHOICES: [(Option<ReasoningEffort>, &str); 7] = [
    (None, "Default"),
    (Some(ReasoningEffort::None), "None"),
    (Some(ReasoningEffort::Minimal), "Minimal"),
    (Some(ReasoningEffort::Low), "Low"),
    (Some(ReasoningEffort::Medium), "Medium"),
    (Some(ReasoningEffort::High), "High"),
    (Some(ReasoningEffort::Xhigh), "Extra high"),
];

pub const POLICY_CHOICES: [(Option<PolicyAction>, &str); 4] = [
    (None, "Default"),
    (Some(PolicyAction::Ask), "Ask"),
    (Some(PolicyAction::Allow), "Allow"),
    (Some(PolicyAction::Deny), "Deny"),
];

pub fn reasoning_label(effort: Option<ReasoningEffort>) -> &'static str {
    REASONING_CHOICES
        .iter()
        .find(|(value, _)| *value == effort)
        .map(|(_, label)| *label)
        .unwrap_or("Default")
}

pub fn policy_label(policy: Option<PolicyAction>) -> &'static str {
    POLICY_CHOICES
        .iter()
        .find(|(value, _)| *value == policy)
        .map(|(_, label)| *label)
        .unwrap_or("Default")
}

/// How close a plan limit is to running out, as in pi-enhanced's status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitLevel {
    Low,
    High,
    Critical,
}

fn limit_level(used_percent: u8) -> LimitLevel {
    match used_percent {
        90.. => LimitLevel::Critical,
        70.. => LimitLevel::High,
        _ => LimitLevel::Low,
    }
}

/// Time left until a reset, in its two largest units: 3d 4h, 2h 13m, 45m.
fn format_until(seconds: u64) -> String {
    let (d, h, m) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    match (d, h, m) {
        (0, 0, 0) => "under a minute".into(),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// The value shown for a limit: its percentage, or the detail of an uncapped
/// credit balance.
fn limit_value(limit: &UsageLimit) -> String {
    match (limit.used_percent, &limit.detail) {
        (Some(used), _) => format!("{used}%"),
        (None, Some(detail)) => detail.clone(),
        (None, None) => "?".into(),
    }
}

fn limits_tooltip(provider: &str, state: &UsageState, now: u64) -> String {
    let mut lines = vec![format!("Plan usage limits for {provider}")];
    for limit in state.limits.iter().flat_map(|l| &l.limits) {
        let mut line = format!("{}: ", limit.label);
        match (limit.used_percent, &limit.detail) {
            (Some(used), Some(detail)) => line.push_str(&format!("{used}% used ({detail})")),
            (Some(used), None) => line.push_str(&format!("{used}% used")),
            (None, Some(detail)) => line.push_str(detail),
            (None, None) => line.push_str("unknown"),
        }
        if let Some(at) = limit.resets_at {
            line.push_str(&format!(
                ", resets in {}",
                format_until(at.saturating_sub(now))
            ));
        }
        lines.push(line);
    }
    if let Some(error) = &state.error {
        lines.push(format!("Could not refresh: {error}"));
    }
    lines.join("\n")
}

/// Shortens a token count for the status bar: 862, 7.5K, 75.9K, 176K, 1.2M.
pub fn compact_count(n: u64) -> String {
    fn scaled(n: u64, unit: f64, suffix: &str) -> String {
        let value = n as f64 / unit;
        if value < 99.95 {
            let text = format!("{value:.1}");
            format!("{}{suffix}", text.strip_suffix(".0").unwrap_or(&text))
        } else {
            format!("{value:.0}{suffix}")
        }
    }
    if n < 1_000 {
        n.to_string()
    } else if n < 999_500 {
        scaled(n, 1e3, "K")
    } else {
        scaled(n, 1e6, "M")
    }
}

/// Groups digits for tooltips: 75862 -> "75,862".
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (ix, ch) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A status bar entry: an icon, a short label, and a tooltip that says what
/// it means.
fn status_item(
    id: &'static str,
    icon: impl Into<Icon>,
    label: impl Into<SharedString>,
    tooltip: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    let tooltip: SharedString = tooltip.into();
    h_flex()
        .id(id)
        .gap_1()
        .child(icon.into().xsmall())
        .child(label.into())
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
}

fn limits_item(
    provider: &str,
    state: &UsageState,
    theme: &gpui_component::Theme,
) -> Option<AnyElement> {
    let headline: Vec<&UsageLimit> = match &state.limits {
        Some(limits) => limits.limits.iter().filter(|l| l.headline).collect(),
        None => Vec::new(),
    };
    if headline.is_empty() && state.error.is_none() {
        // Not fetched yet, or the provider has no plan limits.
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let tooltip: SharedString = limits_tooltip(provider, state, now).into();
    let mut item = h_flex()
        .id("status-limits")
        .gap_1()
        .child(Icon::new(gpui_kit_assets::IconName::Hourglass).xsmall())
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx));
    if headline.is_empty() {
        return Some(
            item.text_color(theme.warning)
                .child("limits unavailable")
                .into_any_element(),
        );
    }
    for (ix, limit) in headline.into_iter().enumerate() {
        if ix > 0 {
            item = item.child(div().text_color(theme.muted_foreground).child("·"));
        }
        let color = match limit.used_percent.map(limit_level) {
            Some(LimitLevel::Critical) => theme.danger,
            Some(LimitLevel::High) => theme.warning,
            Some(LimitLevel::Low) => theme.success,
            None => theme.muted_foreground,
        };
        item = item
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(limit.label.clone()),
            )
            .child(div().text_color(color).child(limit_value(limit)));
    }
    Some(item.into_any_element())
}

impl SessionView {
    /// The provider whose plan limits belong in the status bar, and a marker
    /// that changes when a turn settles (`None` while one runs).
    pub fn usage_source(&self, cx: &App) -> Option<(String, Option<u64>)> {
        let conv = self.conversation.read(cx);
        let model = conv
            .info
            .model
            .clone()
            .or_else(|| self.connection.read(cx).info.default_model.clone())?;
        let (provider, _) = model.split_once('/')?;
        let total = conv.total_usage;
        let marker = (!conv.is_busy()).then(|| total.prompt_tokens() + total.output);
        Some((provider.to_owned(), marker))
    }

    /// The session's entries in the workspace status bar: model, effort, policy, token usage, plan limits, and pending approvals.
    pub fn status_items(view: &Entity<Self>, cx: &App) -> Vec<AnyElement> {
        let theme = cx.theme();
        let this = view.read(cx);
        let conv = this.conversation.read(cx);
        let weak = view.downgrade();
        let mut items = Vec::new();

        let model = conv
            .info
            .model
            .clone()
            .or_else(|| this.connection.read(cx).info.default_model.clone())
            .unwrap_or_else(|| "no model".into());
        let picker = weak.clone();
        items.push(
            Button::new("status-model")
                .ghost()
                .xsmall()
                .icon(IconName::Bot)
                .label(model)
                .tooltip("Model for this session (click to change)")
                .on_click(move |_, window, cx| {
                    let Some(view) = picker.upgrade() else {
                        return;
                    };
                    let view = view.read(cx);
                    let (connection, session) = (view.connection.clone(), view.session_id(cx));
                    dialogs::open_model_picker(connection, Some(session), window, cx);
                })
                .into_any_element(),
        );

        let current = conv.info.reasoning;
        let menu_view = weak.clone();
        items.push(
            Button::new("status-effort")
                .ghost()
                .xsmall()
                .icon(Icon::new(gpui_kit_assets::IconName::Brain))
                .label(reasoning_label(current))
                .tooltip("Reasoning effort for this session")
                .dropdown_menu(move |mut menu, _, _| {
                    for (value, label) in REASONING_CHOICES {
                        let view = menu_view.clone();
                        menu = menu.item(
                            PopupMenuItem::new(label)
                                .checked(value == current)
                                .on_click(move |_, _, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        let session = this.session_id(cx);
                                        this.connection.update(cx, |conn, cx| {
                                            conn.set_reasoning(session, value, cx)
                                        });
                                    });
                                }),
                        );
                    }
                    menu
                })
                .into_any_element(),
        );

        let current = conv.info.policy_default;
        let menu_view = weak.clone();
        items.push(
            Button::new("status-policy")
                .ghost()
                .xsmall()
                .icon(Icon::new(gpui_kit_assets::IconName::ShieldCheck))
                .label(policy_label(current))
                .tooltip("Default action for tool calls without a matching policy rule")
                .dropdown_menu(move |mut menu, _, _| {
                    for (value, label) in POLICY_CHOICES {
                        let view = menu_view.clone();
                        menu = menu.item(
                            PopupMenuItem::new(label)
                                .checked(value == current)
                                .on_click(move |_, _, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        let session = this.session_id(cx);
                                        this.connection.update(cx, |conn, cx| {
                                            conn.set_policy(session, value, cx)
                                        });
                                    });
                                }),
                        );
                    }
                    menu
                })
                .into_any_element(),
        );

        if let Some(usage) = conv.last_usage {
            let context = usage.context_tokens();
            items.push(
                status_item(
                    "status-context",
                    Icon::new(gpui_kit_assets::IconName::Gauge),
                    compact_count(context),
                    format!(
                        "Context: {} tokens after the last response",
                        grouped(context)
                    ),
                )
                .into_any_element(),
            );
            let (input, output) = (conv.total_usage.prompt_tokens(), conv.total_usage.output);
            items.push(
                status_item(
                    "status-usage",
                    Icon::new(gpui_kit_assets::IconName::Coins),
                    format!(
                        "{} in · {} out",
                        compact_count(input),
                        compact_count(output)
                    ),
                    format!(
                        "Tokens used by this session: {} input (cached included), {} output",
                        grouped(input),
                        grouped(output)
                    ),
                )
                .into_any_element(),
            );
        }

        if let Some((provider, _)) = this.usage_source(cx)
            && let Some(state) = this.connection.read(cx).usage.get(&provider)
            && let Some(item) = limits_item(&provider, state, theme)
        {
            items.push(item);
        }

        let pending = conv.pending_approvals().count();
        if pending > 0 {
            items.push(
                status_item(
                    "status-approvals",
                    IconName::TriangleAlert,
                    pending.to_string(),
                    format!("{pending} tool call(s) waiting for approval"),
                )
                .text_color(theme.warning)
                .into_any_element(),
            );
        }

        if conv.info.archived {
            let restore = weak;
            items.push(
                Button::new("status-restore")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(gpui_kit_assets::IconName::ArchiveRestore))
                    .label("Archived")
                    .tooltip("Restore this session to the list")
                    .on_click(move |_, _, cx| {
                        let _ = restore.update(cx, |this, cx| {
                            let session = this.session_id(cx);
                            this.connection
                                .update(cx, |conn, cx| conn.set_archived(session, false, cx));
                        });
                    })
                    .into_any_element(),
            );
        }
        items
    }
}

impl SessionView {
    fn render_attachments(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        h_flex()
            .flex_wrap()
            .gap_2()
            .children(self.attachments.iter().enumerate().map(|(ix, image)| {
                let preview = image.clone();
                div()
                    .id(("attachment", ix))
                    .relative()
                    .size(px(96.))
                    .rounded_md()
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .hover(|el| el.border_color(theme.ring))
                    .on_click(move |_, window, cx| {
                        crate::ui::image_preview::open_image_preview(preview.clone(), window, cx)
                    })
                    .child(img(image.clone()).size_full().object_fit(ObjectFit::Cover))
                    .child(
                        div().absolute().top_0().right_0().child(
                            Button::new(("remove-attachment", ix))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip("Remove")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if ix < this.attachments.len() {
                                        this.attachments.remove(ix);
                                    }
                                    cx.notify();
                                })),
                        ),
                    )
            }))
    }
}

impl Focusable for SessionView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SessionView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let conversation = self.conversation.read(cx);
        let busy = conversation.is_busy();
        let empty = conversation.items.is_empty();
        let compact_tooltip: SharedString = match conversation.last_usage {
            Some(usage) => format!(
                "Summarize earlier turns to free context (now {} tokens)",
                compact_count(usage.context_tokens())
            )
            .into(),
            None => "Summarize earlier turns to free context".into(),
        };
        v_flex()
            .id("session")
            .size_full()
            .track_focus(&self.focus)
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.add_paths(paths.paths().to_vec(), window, cx);
            }))
            .child(messages::pane(&self.messages, cx))
            .child(
                v_flex()
                    .w_full()
                    .p_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .capture_action(cx.listener(Self::on_paste))
                    .when(!self.attachments.is_empty(), |el| {
                        el.child(self.render_attachments(cx))
                    })
                    // Large is the size whose text matches the chat text (1 rem);
                    // the default size uses the smaller `text_sm`.
                    .child(Textarea::new(&self.input).large())
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("attach")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(gpui_kit_assets::IconName::ImagePlus))
                                    .label("Attach")
                                    .tooltip("Attach images (or paste or drop them here)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.pick_images(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("compact")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(gpui_kit_assets::IconName::FoldVertical))
                                    .label("Compact")
                                    .tooltip(compact_tooltip)
                                    .disabled(busy || empty)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.compact(window, cx)),
                                    ),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("interrupt")
                                    .small()
                                    .icon(Icon::new(gpui_kit_assets::IconName::CircleStop))
                                    .label("Interrupt")
                                    .tooltip("Stop the current turn")
                                    .disabled(!busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.interrupt(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("send")
                                    .primary()
                                    .small()
                                    .icon(Icon::new(gpui_kit_assets::IconName::SendHorizontal))
                                    .label("Send")
                                    .tooltip("Send the prompt (enter)")
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                    ),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::{
        LimitLevel, UsageState, compact_count, format_until, grouped, limit_level, limit_value,
        limits_tooltip,
    };
    use loom_client::proto::UsageLimit;

    #[test]
    fn limit_levels_follow_the_reference_thresholds() {
        assert_eq!(limit_level(0), LimitLevel::Low);
        assert_eq!(limit_level(69), LimitLevel::Low);
        assert_eq!(limit_level(70), LimitLevel::High);
        assert_eq!(limit_level(89), LimitLevel::High);
        assert_eq!(limit_level(90), LimitLevel::Critical);
        assert_eq!(limit_level(100), LimitLevel::Critical);
    }

    #[test]
    fn reset_countdowns_use_two_units() {
        assert_eq!(format_until(30), "under a minute");
        assert_eq!(format_until(45 * 60), "45m");
        assert_eq!(format_until(2 * 3600), "2h");
        assert_eq!(format_until(2 * 3600 + 13 * 60 + 5), "2h 13m");
        assert_eq!(format_until(86_400 + 30 * 60), "1d");
        assert_eq!(format_until(3 * 86_400 + 4 * 3600 + 59 * 60), "3d 4h");
    }

    #[test]
    fn limits_tooltip_lists_every_limit_and_the_error() {
        let limit = |label: &str, used, detail: Option<&str>, resets_at| UsageLimit {
            label: label.into(),
            used_percent: used,
            detail: detail.map(str::to_owned),
            resets_at,
            headline: true,
        };
        let mut state = UsageState::default();
        state.limits = Some(Rc::new(loom_client::proto::UsageLimits {
            limits: vec![
                limit("5h", Some(23), None, Some(1000 + 2 * 3600 + 13 * 60)),
                limit("weekly", Some(41), None, None),
                limit("extra usage", Some(12), Some("$12.34 / $100.00"), None),
                limit("credits", None, Some("1,234 left"), None),
            ],
        }));
        state.error = Some("HTTP 429".into());
        assert_eq!(
            limits_tooltip("anthropic", &state, 1000),
            "Plan usage limits for anthropic\n\
             5h: 23% used, resets in 2h 13m\n\
             weekly: 41% used\n\
             extra usage: 12% used ($12.34 / $100.00)\n\
             credits: 1,234 left\n\
             Could not refresh: HTTP 429"
        );
        assert_eq!(
            limit_value(&state.limits.as_ref().unwrap().limits[3]),
            "1,234 left"
        );
    }

    #[test]
    fn compact_counts() {
        let cases = [
            (0, "0"),
            (862, "862"),
            (999, "999"),
            (1_000, "1K"),
            (7_500, "7.5K"),
            (75_862, "75.9K"),
            (99_949, "99.9K"),
            (99_950, "100K"),
            (175_862, "176K"),
            (999_499, "999K"),
            (999_500, "1M"),
            (1_234_567, "1.2M"),
            (250_000_000, "250M"),
        ];
        for (n, text) in cases {
            assert_eq!(compact_count(n), text, "{n}");
        }
    }

    #[test]
    fn grouped_digits() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(75_862), "75,862");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }
}
