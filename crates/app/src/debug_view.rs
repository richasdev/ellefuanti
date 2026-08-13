//! The debugger panel: where execution is stopped, and what is in scope there.
//!
//! Presentation and session ownership (ADR-0004). The protocol, the listener and the
//! session state machine are `elle-debug`'s; this file decides what they look like, what
//! a click does, and which of the session's blocking calls happen on the background
//! executor.
//!
//! # Why the session lives on a background thread and talks back through a channel
//!
//! Every `elle-debug` call blocks, and one of them — `run` — blocks until the script hits
//! the next breakpoint. That can be a minute of query time, or forever. Doing that on the
//! main thread would freeze the window for exactly as long as the user's code takes,
//! which is the one thing a debugger must not do.
//!
//! So the session is *owned* by a background task and never touched from the render path.
//! The UI sends [`DebugCommand`] to it and receives [`DebugEvent`] back, both over the
//! `smol` channels already used for the test runner (#25). The panel holds only what it
//! was last told: a position, a stack, a variable tree. That also makes every field here
//! plain data, which is what lets the tests below construct a panel state and assert on
//! what it renders without a socket or a PHP process anywhere.
//!
//! # What it refuses to do
//!
//! Show a stack or variables it does not have. A session that is running rather than
//! paused clears them rather than leaving the last stop's values on screen looking
//! current — the debugger equivalent of RISKS.md #4's rule for the test panel, and a
//! worse lie here: stale variables are indistinguishable from real ones, and someone will
//! chase a value that has not existed for ten minutes.

use gpui::prelude::*;
use gpui::{App, FocusHandle, Focusable, MouseButton, SharedString, Window, div, px};

use elle_debug::{Property, StackFrame};

use crate::actions::context;
use crate::theme::{Metrics, Theme, Themed as _};

/// What the panel is doing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DebugState {
    /// Not listening. The debugger is off.
    #[default]
    Off,
    /// Listening on a port, waiting for PHP to connect. This is where a session spends
    /// most of its life: the user has armed the debugger and has not loaded the page yet.
    Listening { port: u16 },
    /// Connected and running. Nothing to inspect until it stops.
    Running,
    /// Stopped somewhere inspectable.
    Paused,
    /// The script finished, or the connection dropped.
    Finished,
    /// Could not listen at all — almost always another debugger on the port. Distinct
    /// from `Off` because the user's next move differs, and the message says what to do.
    Failed { message: String },
}

impl DebugState {
    /// Whether stepping is possible. Drives whether the controls are shown as available,
    /// so the UI does not offer a step that will error.
    pub fn can_step(&self) -> bool {
        matches!(self, Self::Paused)
    }
}

/// Commands the UI sends to the session thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugCommand {
    Run,
    StepInto,
    StepOver,
    StepOut,
    /// Set or clear a breakpoint while a session is live. Breakpoints set while nothing is
    /// connected need no command: the store is registered wholesale when a session opens.
    SetBreakpoint {
        file_uri: String,
        line: u32,
    },
    ClearBreakpoint {
        engine_id: String,
    },
    /// Load one frame's variables. Sent when the user clicks a frame, so frames the user
    /// never opens cost nothing.
    Inspect {
        depth: u32,
    },
    /// Expand a container the depth limit truncated.
    Expand {
        full_name: String,
        depth: u32,
    },
    Stop,
}

/// What the session thread reports back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugEvent {
    Listening {
        port: u16,
    },
    Connected {
        file_uri: String,
        engine_version: String,
    },
    /// Execution stopped. Carries the position from the continuation reply itself, which
    /// is why the arrow can move without waiting for a stack request.
    Paused {
        file_uri: String,
        line: u32,
    },
    Stack(Vec<StackFrame>),
    Variables {
        depth: u32,
        properties: Vec<Property>,
    },
    /// A container the user expanded, to be spliced into the tree under `full_name`.
    Expanded {
        full_name: String,
        property: Property,
    },
    /// A breakpoint the engine accepted, so it can be cleared later.
    BreakpointBound {
        file_uri: String,
        line: u32,
        engine_id: String,
    },
    Finished,
    Failed {
        message: String,
    },
}

/// Where execution is stopped, in the terms the editor gutter needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub file_uri: String,
    /// 0-based, converted from the protocol's 1-based line at the point of entry so
    /// nothing downstream has to remember which convention it is holding.
    pub row: usize,
}

/// Everything the panel knows, with no gpui in it.
///
/// Split from [`DebugView`] so the state rules — which are the whole substance of this
/// file — are testable without a window to make a `FocusHandle` from. The view is this
/// plus a focus handle and two callbacks, and it derefs to here, so there is exactly one
/// implementation of every rule rather than a shipped one and a mirrored test one.
#[derive(Debug, Default)]
pub struct DebugData {
    pub state: DebugState,
    /// Where execution is stopped, if it is.
    pub position: Option<Position>,
    pub stack: Vec<StackFrame>,
    /// Which frame the variables belong to. Clicking a frame changes it.
    pub selected_frame: usize,
    pub variables: Vec<Property>,
    /// `full_name`s the user has opened. Expansion is remembered across steps so a tree
    /// the user opened does not collapse under them on every step.
    expanded: Vec<String>,
    /// Xdebug's version, shown once on connect: a surprising version is the commonest
    /// explanation for a session that behaves oddly.
    engine_version: Option<String>,
}

/// What a control or a variable row does when clicked.
///
/// A named type for the reason `test_view::JumpHandler` is one: the panel does not own the
/// session, so it hands the command back to the workspace rather than acting on it.
type CommandHandler = Box<dyn Fn(DebugCommand, &mut Window, &mut App)>;

/// What a stack frame does when clicked.
type FrameJumpHandler = Box<dyn Fn(&StackFrame, &mut Window, &mut App)>;

/// The bottom debugger panel.
pub struct DebugView {
    focus_handle: FocusHandle,
    data: DebugData,
    on_command: Option<CommandHandler>,
    on_jump: Option<FrameJumpHandler>,
}

impl std::ops::Deref for DebugView {
    type Target = DebugData;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl std::ops::DerefMut for DebugView {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

impl DebugView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            data: DebugData::default(),
            on_command: None,
            on_jump: None,
        }
    }

    /// Installs the callback that sends a command to the session thread.
    pub fn on_command(&mut self, send: impl Fn(DebugCommand, &mut Window, &mut App) + 'static) {
        self.on_command = Some(Box::new(send));
    }

    /// Installs the callback a stack frame invokes when clicked.
    ///
    /// A callback rather than opening the file here, for the test panel's reason: the
    /// workspace owns the tabs and `open_path_at` is the app's single jump path (#88).
    pub fn on_jump(&mut self, jump: impl Fn(&StackFrame, &mut Window, &mut App) + 'static) {
        self.on_jump = Some(Box::new(jump));
    }

    /// Applies an event from the session thread.
    pub fn push(&mut self, event: DebugEvent, cx: &mut Context<Self>) {
        self.data.apply(event);
        cx.notify();
    }
}

impl DebugData {
    /// A one-line summary for the status bar.
    pub fn summary(&self) -> String {
        summarise(&self.state, self.position.as_ref())
    }

    /// Whether `full_name` is open in the variable tree.
    pub fn is_expanded(&self, full_name: &str) -> bool {
        self.expanded.iter().any(|name| name == full_name)
    }

    /// Toggles a container open or closed, reporting whether it now needs fetching.
    ///
    /// Returns `true` only when opening something whose children were truncated: a
    /// container already holding all its children expands with no round trip at all.
    ///
    /// Takes the name and the flag rather than the `Property` itself because the caller is
    /// a click handler, and that outlives the frame that built it — it cannot hold a row
    /// borrowed from the tree, and these two values are all the decision needs.
    pub fn toggle_expanded(&mut self, full_name: &str, truncated: bool) -> bool {
        if let Some(index) = self.expanded.iter().position(|name| name == full_name) {
            self.expanded.remove(index);
            return false;
        }
        self.expanded.push(full_name.to_string());
        truncated
    }

    /// Flattens the tree into the rows that are actually visible.
    ///
    /// Children of a collapsed node are not walked at all, so a large object the user has
    /// not opened costs nothing to render.
    fn collect_rows<'a>(
        &self,
        property: &'a Property,
        indent: usize,
        rows: &mut Vec<(&'a Property, usize)>,
    ) {
        rows.push((property, indent));
        if self.is_expanded(&property.full_name) {
            for child in &property.children {
                self.collect_rows(child, indent + 1, rows);
            }
        }
    }

    /// Applies an event from the session thread.
    pub fn apply(&mut self, event: DebugEvent) {
        match event {
            DebugEvent::Listening { port } => {
                self.state = DebugState::Listening { port };
                self.position = None;
                self.stack.clear();
                self.variables.clear();
            }
            DebugEvent::Connected { engine_version, .. } => {
                self.state = DebugState::Running;
                self.engine_version = Some(engine_version);
            }
            DebugEvent::Paused { file_uri, line } => {
                self.state = DebugState::Paused;
                // 1-based on the wire, 0-based everywhere in this app. Converted here, once.
                self.position = Some(Position { file_uri, row: line.saturating_sub(1) as usize });
                // The stack and variables belong to the *previous* stop. Clearing them is
                // what stops the panel showing one line's variables under another line's
                // arrow while the new ones are still in flight.
                self.stack.clear();
                self.variables.clear();
                self.selected_frame = 0;
            }
            DebugEvent::Stack(stack) => self.stack = stack,
            DebugEvent::Variables { depth, properties } => {
                // A reply for a frame the user has already clicked away from would
                // otherwise render under the frame they are now looking at.
                if depth as usize == self.selected_frame {
                    self.variables = properties;
                }
            }
            DebugEvent::Expanded { full_name, property } => {
                splice(&mut self.variables, &full_name, property);
            }
            DebugEvent::BreakpointBound { .. } => {}
            DebugEvent::Finished => {
                self.state = DebugState::Finished;
                // A finished script has no stack and no variables. Keeping the last ones
                // would leave values on screen that no longer exist anywhere.
                self.position = None;
                self.stack.clear();
                self.variables.clear();
            }
            DebugEvent::Failed { message } => {
                self.state = DebugState::Failed { message };
                self.position = None;
                self.stack.clear();
                self.variables.clear();
            }
        }
    }
}

/// Replaces the node named `full_name` with a deeper version of itself.
///
/// Depth-first by `full_name`, which is unique within a scope by construction — it is the
/// expression that addresses the value, so two distinct nodes cannot share one.
fn splice(properties: &mut [Property], full_name: &str, replacement: Property) -> bool {
    for property in properties.iter_mut() {
        if property.full_name == full_name {
            *property = replacement;
            return true;
        }
        if splice(&mut property.children, full_name, replacement.clone()) {
            return true;
        }
    }
    false
}

/// The status-bar text for a debug state.
fn summarise(state: &DebugState, position: Option<&Position>) -> String {
    match state {
        // A project not being debugged says nothing at all, exactly as one with no test
        // runner says nothing about tests.
        DebugState::Off => String::new(),
        DebugState::Listening { port } => format!("Debug: waiting on :{port}"),
        DebugState::Running => "Debug: running…".to_string(),
        DebugState::Paused => match position {
            Some(position) => {
                let file =
                    position.file_uri.rsplit('/').next().unwrap_or(&position.file_uri).to_string();
                // Back to 1-based for display: the user counts lines the way the editor
                // numbers them.
                format!("Debug: paused at {file}:{}", position.row + 1)
            }
            None => "Debug: paused".to_string(),
        },
        DebugState::Finished => "Debug: finished".to_string(),
        DebugState::Failed { message } => format!("Debug: {message}"),
    }
}

/// How a property renders on one line.
///
/// A free function because it is the panel's only real formatting rule and the one most
/// worth pinning: a container must never render as if it were an empty scalar.
fn describe(property: &Property) -> String {
    let name = &property.name;
    match &property.value {
        Some(value) => {
            let type_name = &property.type_name;
            // Strings are quoted so an empty one is visible as "" rather than as nothing,
            // and so `"42"` cannot be mistaken for `42`.
            if type_name == "string" {
                format!("{name} = \"{value}\"")
            } else {
                format!("{name} = {value}")
            }
        }
        None => {
            let kind = match &property.class_name {
                Some(class) => class.clone(),
                None => property.type_name.clone(),
            };
            if property.is_truncated() {
                // The honest form: the user must be able to see that the list is short.
                format!("{name}: {kind} ({} of {})", property.children.len(), property.child_count)
            } else if property.child_count > 0 {
                format!("{name}: {kind} ({})", property.child_count)
            } else if property.type_name == "uninitialized" {
                // Not the same as an empty array, and the difference is usually the bug.
                format!("{name}: uninitialized")
            } else {
                format!("{name}: {kind}")
            }
        }
    }
}

impl Focusable for DebugView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DebugView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        div()
            .key_context(context::DEBUG)
            .track_focus(&self.focus_handle(cx))
            .h(Metrics::TERMINAL_HEIGHT)
            .flex_none()
            .flex()
            .flex_col()
            .bg(theme.background)
            .border_t_1()
            .border_color(theme.border)
            .child(self.render_header(&theme, cx))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .min_h_0()
                    .child(self.render_stack(&theme, cx))
                    .child(self.render_variables(&theme, cx)),
            )
    }
}

impl DebugView {
    /// The header: what the session is doing, and the controls that act on it.
    fn render_header(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let can_step = self.state.can_step();

        let status: SharedString = match &self.state {
            DebugState::Off => "Debugger off".into(),
            DebugState::Listening { port } => {
                format!("Waiting for a request on port {port}…").into()
            }
            DebugState::Running => "Running…".into(),
            DebugState::Paused => match &self.position {
                Some(position) => format!(
                    "Paused at {}:{}",
                    position.file_uri.rsplit('/').next().unwrap_or(&position.file_uri),
                    position.row + 1
                )
                .into(),
                None => "Paused".into(),
            },
            DebugState::Finished => "The script finished".into(),
            DebugState::Failed { message } => message.clone().into(),
        };

        let tint = match &self.state {
            DebugState::Failed { .. } => theme.error,
            DebugState::Paused => theme.text,
            _ => theme.text_muted,
        };

        // Every control carries a word, not a glyph alone: the icons for "step over" and
        // "step into" are near-identical arrows in every IDE and nobody learns which is
        // which from the shape.
        let controls = [
            ("continue", "Continue", DebugCommand::Run),
            ("step-over", "Step Over", DebugCommand::StepOver),
            ("step-into", "Step Into", DebugCommand::StepInto),
            ("step-out", "Step Out", DebugCommand::StepOut),
            ("stop", "Stop", DebugCommand::Stop),
        ];

        div()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3()
            .py_2()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .child(div().flex().flex_col().child(div().text_color(tint).child(status)).children(
                self.engine_version.as_ref().map(|version| {
                    div()
                        .text_color(theme.text_muted)
                        .text_size(px(11.0))
                        .child(SharedString::from(format!("Xdebug {version}")))
                }),
            ))
            .child(div().flex().gap_1().children(controls.into_iter().map(
                |(id, label, command)| {
                    let entity = entity.clone();
                    // `Stop` stays live while the session does; the steps do not, because
                    // stepping a script that has finished is an error rather than a step.
                    let enabled = can_step || matches!(command, DebugCommand::Stop);
                    div()
                        .id(id)
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .text_size(px(11.0))
                        .text_color(if enabled { theme.text } else { theme.text_muted })
                        .when(enabled, |el| {
                            el.cursor_pointer().hover(|el| el.bg(theme.hover)).on_mouse_down(
                                MouseButton::Left,
                                move |_event, window, cx| {
                                    // The callback is taken out of the entity before being
                                    // called: it needs `&mut App`, and it cannot borrow one
                                    // while the entity that owns it is itself borrowed
                                    // mutably. Putting it back afterwards keeps the panel
                                    // usable for the next click.
                                    let taken =
                                        entity.update(cx, |this, _cx| this.on_command.take());
                                    if let Some(send) = taken {
                                        send(command.clone(), window, cx);
                                        entity.update(cx, |this, _cx| {
                                            this.on_command = Some(send);
                                        });
                                    }
                                },
                            )
                        })
                        .child(label)
                },
            )))
    }

    /// The call stack, innermost frame first. Clicking a frame opens its file and shows
    /// its variables.
    fn render_stack(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let selected = self.selected_frame;

        div()
            .id("debug-stack")
            .w(px(320.0))
            .flex_none()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child("Call stack"),
            )
            .children(self.stack.iter().enumerate().map(|(index, frame)| {
                let entity = entity.clone();
                let file = frame.file_uri.rsplit('/').next().unwrap_or(&frame.file_uri).to_string();
                div()
                    .id(("frame", index))
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .when(index == selected, |el| el.bg(theme.hover))
                    .hover(|el| el.bg(theme.hover))
                    .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
                        // Both callbacks are taken out before being called, for the reason
                        // the controls above document: they need `&mut App`, which cannot
                        // be borrowed while the entity holding them is.
                        let taken = entity.update(cx, |this, cx| {
                            this.selected_frame = index;
                            // The variables on screen belong to the frame being left.
                            this.variables.clear();
                            cx.notify();
                            this.stack
                                .get(index)
                                .cloned()
                                .map(|frame| (frame, this.on_command.take(), this.on_jump.take()))
                        });

                        let Some((frame, send, jump)) = taken else { return };
                        if let Some(send) = &send {
                            send(DebugCommand::Inspect { depth: index as u32 }, window, cx);
                        }
                        if let Some(jump) = &jump {
                            jump(&frame, window, cx);
                        }
                        entity.update(cx, |this, _cx| {
                            this.on_command = send;
                            this.on_jump = jump;
                        });
                    })
                    .child(
                        div()
                            .text_color(theme.text)
                            .child(SharedString::from(frame.function.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("{file}:{}", frame.line))),
                    )
            }))
    }

    /// The variables of the selected frame.
    fn render_variables(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows = Vec::new();
        for property in &self.variables {
            self.collect_rows(property, 0, &mut rows);
        }
        let entity = cx.entity();
        let depth = self.selected_frame as u32;

        div()
            .id("debug-variables")
            .flex_1()
            .overflow_y_scroll()
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child("Variables"),
            )
            .children(rows.into_iter().enumerate().map(|(index, (property, indent))| {
                let entity = entity.clone();
                let expandable = property.child_count > 0;
                let open = self.is_expanded(&property.full_name);
                // The click handler outlives this frame, so it cannot hold a row borrowed
                // from `self`. Only the name and the truncation flag are needed to decide
                // what the click does, so those are what it captures.
                let full_name = property.full_name.clone();
                let truncated = property.is_truncated();
                let marker = if !expandable {
                    "  "
                } else if open {
                    "▾ "
                } else {
                    "▸ "
                };

                div()
                    .id(("variable", index))
                    .flex()
                    .px_3()
                    .py_0p5()
                    .pl(px(12.0 + indent as f32 * 12.0))
                    .when(expandable, |el| {
                        el.cursor_pointer().hover(|el| el.bg(theme.hover)).on_mouse_down(
                            MouseButton::Left,
                            move |_event, window, cx| {
                                // Only a truncated container costs a request; one that
                                // already holds its children just opens.
                                let taken = entity.update(cx, |this, cx| {
                                    let needs_fetch = this.toggle_expanded(&full_name, truncated);
                                    cx.notify();
                                    needs_fetch.then(|| this.on_command.take()).flatten()
                                });

                                if let Some(send) = taken {
                                    send(
                                        DebugCommand::Expand {
                                            full_name: full_name.clone(),
                                            depth,
                                        },
                                        window,
                                        cx,
                                    );
                                    entity.update(cx, |this, _cx| {
                                        this.on_command = Some(send);
                                    });
                                }
                            },
                        )
                    })
                    .child(
                        div()
                            .text_color(theme.text_muted)
                            .text_size(px(11.0))
                            .child(SharedString::from(marker)),
                    )
                    .child(
                        div()
                            .text_color(theme.text)
                            .text_size(px(12.0))
                            .child(SharedString::from(describe(property))),
                    )
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise `DebugData` — the real type the panel holds and the real `apply` that
    // ships — rather than a mirror of it. That is the whole reason the data was split from
    // the view: a mirrored state machine in a test module agrees with itself and proves
    // nothing about what runs.

    fn property(name: &str, type_name: &str, value: Option<&str>) -> Property {
        Property {
            name: name.to_string(),
            full_name: name.to_string(),
            type_name: type_name.to_string(),
            class_name: None,
            value: value.map(str::to_string),
            child_count: 0,
            children: Vec::new(),
        }
    }

    fn frame(level: usize, function: &str, line: u32) -> StackFrame {
        StackFrame {
            level,
            function: function.to_string(),
            file_uri: "file:///srv/app/index.php".to_string(),
            line,
        }
    }

    #[test]
    fn a_pause_reports_the_position_as_a_zero_based_row() {
        // The protocol counts lines from 1 and this app counts rows from 0. Getting this
        // wrong puts the gutter arrow one line off on every single stop.
        let mut data = DebugData::default();
        data.apply(DebugEvent::Paused { file_uri: "file:///srv/app/index.php".into(), line: 24 });
        assert_eq!(data.state, DebugState::Paused);
        assert_eq!(data.position.as_ref().unwrap().row, 23);
    }

    #[test]
    fn line_zero_does_not_underflow_the_row() {
        // Xdebug should never send it, but a `line: 0` turning into `usize::MAX` would
        // panic the gutter rather than merely being wrong.
        let mut data = DebugData::default();
        data.apply(DebugEvent::Paused { file_uri: "file:///a.php".into(), line: 0 });
        assert_eq!(data.position.unwrap().row, 0);
    }

    #[test]
    fn a_new_stop_clears_the_previous_stops_stack_and_variables() {
        // The lie this prevents: the last line's variables sitting under this line's arrow
        // while the new ones are still in flight. They are indistinguishable from real
        // ones, and someone will chase a value that no longer exists.
        let mut data = DebugData::default();
        data.apply(DebugEvent::Stack(vec![frame(0, "old", 1)]));
        data.apply(DebugEvent::Variables {
            depth: 0,
            properties: vec![property("$a", "int", Some("1"))],
        });
        data.apply(DebugEvent::Paused { file_uri: "file:///a.php".into(), line: 2 });

        assert!(data.stack.is_empty());
        assert!(data.variables.is_empty());
        assert_eq!(data.selected_frame, 0, "a new stop starts at the innermost frame");
    }

    #[test]
    fn a_finished_script_keeps_nothing_on_screen() {
        let mut data = DebugData::default();
        data.apply(DebugEvent::Paused { file_uri: "file:///a.php".into(), line: 2 });
        data.apply(DebugEvent::Stack(vec![frame(0, "{main}", 2)]));
        data.apply(DebugEvent::Finished);

        assert_eq!(data.state, DebugState::Finished);
        assert!(data.position.is_none(), "there is no line to point at any more");
        assert!(data.stack.is_empty());
        assert!(data.variables.is_empty());
    }

    #[test]
    fn a_failure_clears_the_session_and_keeps_the_message() {
        let mut data = DebugData::default();
        data.apply(DebugEvent::Paused { file_uri: "file:///a.php".into(), line: 2 });
        data.apply(DebugEvent::Failed { message: "port 9003 is already in use".into() });

        assert_eq!(
            data.state,
            DebugState::Failed { message: "port 9003 is already in use".into() }
        );
        assert!(data.position.is_none());
    }

    #[test]
    fn variables_for_a_frame_the_user_left_are_discarded() {
        // The user clicks frame 0, then frame 1, and frame 0's slower reply arrives last.
        // Rendering it would show frame 0's variables labelled as frame 1's.
        let mut data = DebugData { selected_frame: 1, ..DebugData::default() };
        data.apply(DebugEvent::Variables {
            depth: 0,
            properties: vec![property("$stale", "int", Some("1"))],
        });
        assert!(data.variables.is_empty());

        data.apply(DebugEvent::Variables {
            depth: 1,
            properties: vec![property("$fresh", "int", Some("2"))],
        });
        assert_eq!(data.variables[0].name, "$fresh");
    }

    #[test]
    fn connecting_records_the_engine_version() {
        // A surprising Xdebug version is the commonest explanation for a session that
        // connects and then behaves oddly, so it is worth the one line it occupies.
        let mut data = DebugData::default();
        data.apply(DebugEvent::Connected {
            file_uri: "file:///srv/app/index.php".into(),
            engine_version: "3.3.1".into(),
        });
        assert_eq!(data.state, DebugState::Running);
        assert_eq!(data.engine_version.as_deref(), Some("3.3.1"));
    }

    #[test]
    fn stepping_is_offered_only_while_paused() {
        // Offering a step to a script that has finished produces a DBGp error rather than
        // a step, so the control must not look available.
        assert!(DebugState::Paused.can_step());
        assert!(!DebugState::Running.can_step());
        assert!(!DebugState::Finished.can_step());
        assert!(!DebugState::Off.can_step());
        assert!(!DebugState::Listening { port: 9003 }.can_step());
    }

    #[test]
    fn the_status_line_says_nothing_when_the_debugger_is_off() {
        // A project not being debugged must not spend status-bar space saying so, exactly
        // as one with no test runner says nothing about tests.
        assert_eq!(DebugData::default().summary(), "");
        assert_eq!(
            summarise(&DebugState::Listening { port: 9003 }, None),
            "Debug: waiting on :9003"
        );
    }

    #[test]
    fn the_status_line_counts_lines_from_one_again_for_the_user() {
        let position = Position { file_uri: "file:///srv/app/index.php".into(), row: 23 };
        assert_eq!(
            summarise(&DebugState::Paused, Some(&position)),
            "Debug: paused at index.php:24"
        );
    }

    #[test]
    fn a_container_never_renders_as_an_empty_scalar() {
        // The failure this pins: an array with no `value` rendering as `$rows = `, which
        // reads as an empty string rather than as a container.
        //
        // The children are present as well as counted. A `child_count` of 3 with an empty
        // `children` is not a complete container at all — it is a truncated one, and the
        // test below is what covers that case.
        let array = Property {
            child_count: 3,
            children: vec![
                property("0", "int", Some("1")),
                property("1", "int", Some("2")),
                property("2", "int", Some("3")),
            ],
            ..property("$rows", "array", None)
        };
        assert_eq!(describe(&array), "$rows: array (3)");

        let object = Property {
            class_name: Some("App\\Models\\User".into()),
            child_count: 2,
            children: vec![property("id", "int", Some("7")), property("name", "string", Some("R"))],
            ..property("$user", "object", None)
        };
        assert_eq!(describe(&object), "$user: App\\Models\\User (2)");
    }

    #[test]
    fn a_truncated_container_says_how_much_it_is_hiding() {
        // Showing 100 of 500 rows as if they were all of them is the failure that makes a
        // debugger actively misleading rather than merely limited.
        let array = Property {
            child_count: 500,
            children: vec![property("0", "int", Some("1"))],
            ..property("$rows", "array", None)
        };
        assert_eq!(describe(&array), "$rows: array (1 of 500)");
    }

    #[test]
    fn strings_are_quoted_so_an_empty_one_is_visible() {
        assert_eq!(describe(&property("$name", "string", Some("Ricardo"))), "$name = \"Ricardo\"");
        assert_eq!(describe(&property("$blank", "string", Some(""))), "$blank = \"\"");
        // Unquoted, `"42"` and `42` would look identical, and the difference is often the
        // bug being hunted.
        assert_eq!(describe(&property("$n", "int", Some("42"))), "$n = 42");
        assert_eq!(describe(&property("$s", "string", Some("42"))), "$s = \"42\"");
    }

    #[test]
    fn an_uninitialised_variable_is_distinguished_from_an_empty_one() {
        assert_eq!(describe(&property("$later", "uninitialized", None)), "$later: uninitialized");
    }

    #[test]
    fn expanding_a_full_container_costs_no_round_trip() {
        // It already holds its children; asking Xdebug again would be a request to learn
        // something we have.
        let mut data = DebugData::default();
        let array = Property {
            child_count: 2,
            children: vec![property("0", "int", Some("1")), property("1", "int", Some("2"))],
            ..property("$small", "array", None)
        };
        assert!(
            !data.toggle_expanded(&array.full_name, array.is_truncated()),
            "a complete container just opens"
        );
        assert!(data.is_expanded("$small"));
    }

    #[test]
    fn expanding_a_truncated_container_asks_for_it_and_closing_asks_for_nothing() {
        let mut data = DebugData::default();
        let array = Property {
            child_count: 500,
            children: vec![property("0", "int", Some("1"))],
            ..property("$big", "array", None)
        };
        assert!(
            data.toggle_expanded(&array.full_name, array.is_truncated()),
            "a truncated container needs fetching"
        );
        assert!(!data.toggle_expanded(&array.full_name, array.is_truncated()));
        assert!(!data.is_expanded("$big"));
    }

    #[test]
    fn expansion_survives_a_step() {
        // A tree the user opened must not collapse under them on every step, which is what
        // would make stepping through a loop unusable.
        let mut data = DebugData::default();
        let array = Property { child_count: 2, ..property("$user", "array", None) };
        data.toggle_expanded(&array.full_name, array.is_truncated());
        data.apply(DebugEvent::Paused { file_uri: "file:///a.php".into(), line: 9 });
        assert!(data.is_expanded("$user"));
    }

    #[test]
    fn an_expanded_reply_replaces_the_node_it_names_anywhere_in_the_tree() {
        // The node the user opened may be nested. Splicing by `full_name` — the expression
        // that addresses the value, and therefore unique within a scope — is what finds it.
        let deep = Property {
            full_name: "$outer['inner']".into(),
            child_count: 9,
            ..property("inner", "array", None)
        };
        let outer = Property {
            full_name: "$outer".into(),
            child_count: 1,
            children: vec![deep],
            ..property("$outer", "array", None)
        };
        let mut data = DebugData { variables: vec![outer], ..DebugData::default() };

        let replacement = Property {
            full_name: "$outer['inner']".into(),
            child_count: 9,
            children: vec![property("0", "int", Some("7"))],
            ..property("inner", "array", None)
        };
        data.apply(DebugEvent::Expanded {
            full_name: "$outer['inner']".into(),
            property: replacement,
        });

        assert_eq!(data.variables[0].children[0].children[0].value.as_deref(), Some("7"));
    }

    #[test]
    fn splicing_a_name_that_is_not_there_changes_nothing() {
        let mut tree = vec![property("$a", "int", Some("1"))];
        assert!(!splice(&mut tree, "$absent", property("x", "int", Some("9"))));
        assert_eq!(tree[0].value.as_deref(), Some("1"));
    }

    #[test]
    fn a_collapsed_container_contributes_one_row_and_its_children_none() {
        // What keeps a large object cheap to render: children of a closed node are never
        // walked at all.
        let data = DebugData::default();
        let array = Property {
            child_count: 2,
            children: vec![property("0", "int", Some("1")), property("1", "int", Some("2"))],
            ..property("$rows", "array", None)
        };
        let mut rows = Vec::new();
        data.collect_rows(&array, 0, &mut rows);
        assert_eq!(rows.len(), 1);

        let mut data = DebugData::default();
        data.toggle_expanded(&array.full_name, array.is_truncated());
        let mut rows = Vec::new();
        data.collect_rows(&array, 0, &mut rows);
        assert_eq!(rows.len(), 3, "the container and its two children");
        assert_eq!(rows[1].1, 1, "children are indented one level");
    }
}
