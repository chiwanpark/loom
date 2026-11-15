//! The Ports tab of the right panel: the TCP ports that programs listen on,
//! on the agent's host, and the local ports forwarded to them.

use std::time::{Duration, Instant};

use gpui::{
    AppContext, AsyncWindowContext, ClipboardItem, Context, Entity, InteractiveElement,
    IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled, Subscription, Task,
    Window, div, prelude::FluentBuilder,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Sizable, h_flex, v_flex};
use loom_client::proto::ListeningPort;

use crate::state::{AgentConnection, ConnectionEvent};
use crate::ui::notify;

/// How often the list is read again while the tab shows.
const POLL: Duration = Duration::from_secs(3);

/// A port to forward as typed: `3000`, `host:3000`, or `[::1]:3000`; the
/// host defaults to `localhost`.
pub fn parse_target(text: &str) -> Option<(String, u16)> {
    let text = text.trim();
    if let Ok(port) = text.parse::<u16>() {
        return (port > 0).then(|| ("localhost".to_string(), port));
    }
    let (host, port) = text.rsplit_once(':')?;
    let port = port.parse::<u16>().ok().filter(|port| *port > 0)?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    (!host.is_empty() && !host.contains(char::is_whitespace)).then(|| (host.to_string(), port))
}

pub struct PortsView {
    connection: Entity<AgentConnection>,
    /// `None` until the ports are first read.
    ports: Option<Vec<ListeningPort>>,
    /// Why the ports could not be read the last time.
    error: Option<String>,
    loading: bool,
    /// Forwards being started, by host and port.
    starting: Vec<(String, u16)>,
    input: Entity<InputState>,
    /// When the tab was last drawn: the list is read again only while it shows.
    shown: Option<Instant>,
    _poll: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl PortsView {
    pub fn new(
        connection: Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Port or host:port"));
        let subscriptions = vec![
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.forward_typed(window, cx);
                }
            }),
            cx.observe(&connection, |_, _, cx| cx.notify()),
            cx.subscribe(&connection, |this, _, event, cx| {
                if let ConnectionEvent::SessionsLoaded = event {
                    // Reconnected: the ports may have changed meanwhile.
                    this.refresh(cx);
                }
            }),
        ];
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let alive = this.update(cx, |this, cx| {
                    if this.shown.is_some_and(|shown| shown.elapsed() < POLL * 2) {
                        this.refresh(cx);
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        let mut this = Self {
            connection,
            ports: None,
            error: None,
            loading: false,
            starting: Vec::new(),
            input,
            shown: None,
            _poll: poll,
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this
    }

    /// Reads the agent's listening ports again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        let task = self.connection.read(cx).list_ports();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Some(Ok(ports)) => {
                        this.ports = Some(ports);
                        this.error = None;
                    }
                    Some(Err(err)) => this.error = Some(err.to_string()),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn ports(&self) -> Option<&[ListeningPort]> {
        self.ports.as_deref()
    }

    /// Forwards a local port to `host:port` on the agent's host.
    pub fn forward(
        &mut self,
        host: String,
        port: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.starting.contains(&(host.clone(), port)) {
            return;
        }
        self.starting.push((host.clone(), port));
        let task = self
            .connection
            .update(cx, |conn, cx| conn.forward_port(host.clone(), port, cx));
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.starting.retain(|s| *s != (host.clone(), port));
                if let Err(err) = result {
                    notify::error(
                        window,
                        cx,
                        format!("Could not forward {host}:{port}: {err}"),
                    );
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn forward_typed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        let Some((host, port)) = parse_target(&text) else {
            notify::error(
                window,
                cx,
                format!("{text:?} is not a port: type a number such as 3000, or host:port."),
            );
            return;
        };
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.forward(host, port, window, cx);
    }

    fn render_forwards(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let forwards: Vec<(u16, String, u16, usize)> = self
            .connection
            .read(cx)
            .forwards()
            .iter()
            .map(|f| {
                (
                    f.local_port(),
                    f.host.clone(),
                    f.port,
                    f.forward.connections(),
                )
            })
            .collect();
        v_flex()
            .w_full()
            .child(section_title("Forwarded", cx))
            .when(forwards.is_empty(), |el| {
                el.child(
                    div()
                        .px_3()
                        .pb_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("No local port forwards to the agent's host yet."),
                )
            })
            .children(
                forwards
                    .into_iter()
                    .map(|(local, host, port, connections)| {
                        let address = format!("localhost:{local}");
                        let url = format!("http://localhost:{local}");
                        let copied = address.clone();
                        h_flex()
                            .id(("forward", usize::from(local)))
                            .w_full()
                            .px_3()
                            .py_1()
                            .gap_2()
                            .text_sm()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .debug_selector(move || format!("forward-{local}"))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child(address),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .child(match connections {
                                                0 => format!("to {host}:{port}"),
                                                1 => format!("to {host}:{port} · 1 connection"),
                                                n => format!("to {host}:{port} · {n} connections"),
                                            }),
                                    ),
                            )
                            .child(
                                Button::new(("open-forward", usize::from(local)))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(gpui_kit_assets::IconName::ExternalLink))
                                    .tooltip("Open in the browser")
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                            .child(
                                Button::new(("copy-forward", usize::from(local)))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(IconName::Copy))
                                    .tooltip("Copy the address")
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copied.clone(),
                                        ))
                                    }),
                            )
                            .child(
                                Button::new(("stop-forward", usize::from(local)))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::new(gpui_kit_assets::IconName::CircleStop))
                                    .tooltip("Stop forwarding")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.connection
                                            .update(cx, |conn, cx| conn.stop_forward(local, cx));
                                    })),
                            )
                    }),
            )
    }

    fn render_ports(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let forwarded: Vec<(String, u16, u16)> = self
            .connection
            .read(cx)
            .forwards()
            .iter()
            .map(|f| (f.host.clone(), f.port, f.local_port()))
            .collect();
        let message = match (&self.ports, &self.error) {
            (_, Some(error)) => Some(format!("Could not list the ports: {error}")),
            (None, None) => Some("Reading the ports…".to_string()),
            (Some(ports), None) if ports.is_empty() => {
                Some("No program of the agent's user listens on a TCP port.".to_string())
            }
            _ => None,
        };
        let ports = self.ports.clone().unwrap_or_default();
        v_flex()
            .w_full()
            .child(section_title("Listening on the agent's host", cx))
            .when_some(message, |el, message| {
                el.child(
                    div()
                        .px_3()
                        .pb_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(message),
                )
            })
            .children(ports.into_iter().map(|listening| {
                let host = listening.connect_host();
                let port = listening.port;
                let local = forwarded
                    .iter()
                    .find(|(h, p, _)| *p == port && (*h == host || h == "localhost"))
                    .map(|(_, _, local)| *local);
                let starting = self.starting.iter().any(|(_, p)| *p == port);
                let process = match (&listening.process, listening.pid) {
                    (Some(name), Some(pid)) => format!("{name} ({pid})"),
                    (Some(name), None) => name.clone(),
                    (None, Some(pid)) => format!("process {pid}"),
                    (None, None) => String::new(),
                };
                h_flex()
                    .id(("port", usize::from(port)))
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .text_sm()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .debug_selector(move || format!("port-{port}"))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(port.to_string()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(format!("{process} · {}", listening.address)),
                            ),
                    )
                    .child(match local {
                        Some(local) => div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("on localhost:{local}"))
                            .into_any_element(),
                        None => Button::new(("forward-port", usize::from(port)))
                            .xsmall()
                            .label("Forward")
                            .disabled(starting)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.forward(host.clone(), port, window, cx)
                            }))
                            .into_any_element(),
                    })
            }))
    }
}

fn section_title(text: &'static str, cx: &Context<PortsView>) -> impl IntoElement {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

impl Render for PortsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.shown = Some(Instant::now());
        let theme = cx.theme().clone();
        v_flex()
            .id("ports")
            .size_full()
            .overflow_y_scroll()
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .pt_2()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.input).small()))
                    .child(
                        Button::new("forward-typed")
                            .xsmall()
                            .label("Forward")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.forward_typed(window, cx)),
                            ),
                    ),
            )
            .child(self.render_forwards(cx))
            .child(self.render_ports(cx))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        "A forwarded port listens on this machine's loopback address and \
                         carries each connection to the port on the agent's host.",
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_ports_are_parsed() {
        assert_eq!(parse_target(" 3000 "), Some(("localhost".into(), 3000)));
        assert_eq!(parse_target("db:5432"), Some(("db".into(), 5432)));
        assert_eq!(parse_target("[::1]:8080"), Some(("::1".into(), 8080)));
        assert_eq!(parse_target("0"), None);
        assert_eq!(parse_target("web"), None);
        assert_eq!(parse_target(":80"), None);
        assert_eq!(parse_target("a b:80"), None);
    }
}

#[cfg(test)]
mod view_tests {
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::rc::Rc;

    use gpui::{TestAppContext, VisualTestContext};
    use gpui_component::Root;

    use super::*;
    use crate::test_agent::{TestAgent, wait_until};

    fn open<'a>(
        agent: &TestAgent,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<AgentConnection>,
        Entity<PortsView>,
        &'a mut VisualTestContext,
    ) {
        cx.update(gpui_component::init);
        let conn = agent.connect(cx);
        let slot = Rc::new(RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let (slot, conn) = (slot.clone(), conn.clone());
            move |window, cx| {
                let view = cx.new(|cx| PortsView::new(conn, window, cx));
                *slot.borrow_mut() = Some(view.clone());
                Root::new(view, window, cx)
            }
        });
        let view = slot.borrow_mut().take().unwrap();
        (conn, view, cx)
    }

    #[gpui::test]
    fn listening_ports_are_listed_and_forwarded(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in server.incoming() {
                let mut stream = stream.unwrap();
                let mut line = [0u8; 5];
                stream.read_exact(&mut line).unwrap();
                line.reverse();
                stream.write_all(&line).unwrap();
            }
        });
        let (conn, view, cx) = open(&agent, cx);
        wait_until(cx, |cx| {
            view.read(cx)
                .ports()
                .is_some_and(|ports| ports.iter().any(|p| p.port == port))
        });
        assert!(
            cx.debug_bounds(Box::leak(format!("port-{port}").into_boxed_str()))
                .is_some()
        );

        view.update_in(cx, |view, window, cx| {
            view.forward("127.0.0.1".into(), port, window, cx)
        });
        wait_until(cx, |cx| conn.read(cx).forwards().len() == 1);
        let local = conn.read_with(cx, |conn, _| conn.forwards()[0].local_port());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(Box::leak(format!("forward-{local}").into_boxed_str()))
                .is_some()
        );
        // Forwarding it again keeps the one forward.
        view.update_in(cx, |view, window, cx| {
            view.forward("127.0.0.1".into(), port, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(conn.read_with(cx, |conn, _| conn.forwards().len()), 1);

        let reply = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", local)).unwrap();
            stream.write_all(b"hello").unwrap();
            let mut reply = [0u8; 5];
            stream.read_exact(&mut reply).unwrap();
            reply
        });
        wait_until(cx, |_| reply.is_finished());
        assert_eq!(&reply.join().unwrap(), b"olleh");

        conn.update(cx, |conn, cx| conn.stop_forward(local, cx));
        assert!(conn.read_with(cx, |conn, _| conn.forwards().is_empty()));
        wait_until(cx, |_| {
            std::net::TcpStream::connect(("127.0.0.1", local)).is_err()
        });
    }
}
