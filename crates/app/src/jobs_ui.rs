//! Job progress + toast surfaces (ARCHITECTURE.md §8 "Progress popover +
//! toasts"): **pure observers** of [`JobsModel`] — no channel handling here.
//! [`JobsIndicator`] is the titlebar button (visible while jobs run) whose
//! anchored popover lists the model's rows with per-job cancel buttons;
//! [`ToastLayer`] renders the model's timed toasts as overlay rows (the
//! timing itself lives in the model, on `Spawner::timer`).

use gpui::{
    Context, Entity, Hsla, IntoElement, Render, SharedString, Subscription, Window, anchored,
    deferred, div, prelude::*, px, relative,
};

use crate::jobs_model::{JobRowState, JobsModel, ToastKind, kind_verb};
use crate::theme::Theme;

/// Titlebar jobs button + anchored progress popover. Renders nothing while
/// no jobs run, so idle chrome (and its visual baselines) is unchanged.
pub struct JobsIndicator {
    theme: Theme,
    jobs: Entity<JobsModel>,
    popover_open: bool,
    _observe: Subscription,
}

impl JobsIndicator {
    pub fn new(theme: Theme, jobs: Entity<JobsModel>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&jobs, |_, _, cx| cx.notify());
        Self {
            theme,
            jobs,
            popover_open: false,
            _observe: observe,
        }
    }

    pub fn popover_open(&self) -> bool {
        self.popover_open
    }

    fn render_popover(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let rows = self.jobs.read(cx).rows().to_vec();
        let jobs = self.jobs.clone();
        deferred(
            anchored().snap_to_window_with_margin(px(8.0)).child(
                div()
                    .occlude()
                    .mt(px(24.0))
                    .flex()
                    .flex_col()
                    .w(px(320.0))
                    .p(px(8.0))
                    .gap(px(8.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .children(rows.into_iter().enumerate().map(|(ix, row)| {
                        let label: SharedString = match row.state {
                            JobRowState::AwaitingDecision => {
                                format!("{} — waiting for decision…", kind_verb(row.info.kind))
                                    .into()
                            }
                            JobRowState::Running => {
                                let current = row
                                    .current
                                    .as_deref()
                                    .and_then(|p| p.file_name())
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                format!("{} {current}", kind_verb(row.info.kind)).into()
                            }
                        };
                        let id = row.info.id;
                        let jobs = jobs.clone();
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .flex_1()
                                            .truncate()
                                            .text_size(px(12.0))
                                            .text_color(theme.text)
                                            .child(label),
                                    )
                                    .child(
                                        div()
                                            .id(("job-cancel", ix))
                                            .debug_selector(|| format!("job-cancel-{ix}"))
                                            .px(px(4.0))
                                            .rounded(px(3.0))
                                            .text_size(px(11.0))
                                            .text_color(theme.muted)
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(theme.error))
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                jobs.read(cx).cancel_job(id);
                                            }))
                                            .child(SharedString::new_static("✕")),
                                    ),
                            )
                            // Progress bar: track + accent fill.
                            .child(
                                div()
                                    .h(px(4.0))
                                    .w_full()
                                    .rounded(px(2.0))
                                    .bg(theme.border)
                                    .child(
                                        div()
                                            .h_full()
                                            .rounded(px(2.0))
                                            .bg(theme.accent)
                                            .w(relative(row.fraction())),
                                    ),
                            )
                    })),
            ),
        )
    }
}

impl Render for JobsIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let active = self.jobs.read(cx).rows().len();
        if active == 0 {
            self.popover_open = false;
            return div();
        }
        let mut root = div().relative().child(
            div()
                .id("jobs-indicator")
                .flex()
                .items_center()
                .gap(px(4.0))
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(4.0))
                .text_size(px(12.0))
                .text_color(theme.text)
                .cursor_pointer()
                .hover(|s| s.bg(theme.accent.opacity(0.15)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.popover_open = !this.popover_open;
                    cx.notify();
                }))
                .child(SharedString::new(format!(
                    "⟳ {active} job{}",
                    if active == 1 { "" } else { "s" }
                ))),
        );
        if self.popover_open {
            root = root.child(self.render_popover(cx));
        }
        root
    }
}

/// Timed toast overlay rows (completion / error / undo-invalidation),
/// anchored to the workspace's bottom-right corner. Click dismisses early;
/// expiry is the model's `Spawner::timer` task.
pub struct ToastLayer {
    theme: Theme,
    jobs: Entity<JobsModel>,
    _observe: Subscription,
}

impl ToastLayer {
    pub fn new(theme: Theme, jobs: Entity<JobsModel>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&jobs, |_, _, cx| cx.notify());
        Self {
            theme,
            jobs,
            _observe: observe,
        }
    }

    fn accent_for(&self, kind: ToastKind) -> Hsla {
        match kind {
            ToastKind::Success => self.theme.accent,
            ToastKind::Error => self.theme.error,
            ToastKind::Info => self.theme.muted,
        }
    }
}

impl Render for ToastLayer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();
        let toasts = self.jobs.read(cx).toasts().to_vec();
        if toasts.is_empty() {
            return div();
        }
        let jobs = self.jobs.clone();
        div()
            .absolute()
            .bottom(px(32.0))
            .right(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(toasts.into_iter().enumerate().map(|(ix, toast)| {
                let jobs = jobs.clone();
                let toast_id = toast.id;
                div()
                    .id(("toast", ix))
                    .debug_selector(|| format!("toast-{ix}"))
                    .occlude()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .max_w(px(360.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(self.accent_for(toast.kind))
                    .bg(theme.panel)
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .cursor_pointer()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        jobs.update(cx, |jobs, cx| jobs.dismiss_toast(toast_id, cx));
                    }))
                    .child(toast.message.clone())
            }))
    }
}
