use super::*;
use task::{LayerProgress, Record};

struct Snapshot {
    record: Record,
    path: std::path::PathBuf,
    phase: String,
    ratio: f32,
    speed: u64,
    packing: Option<(usize, usize)>,
}
impl Snapshot {
    fn from_task(task: &DownloadTask) -> Self {
        Self {
            record: task.record.clone(),
            path: task.record.output.clone().unwrap_or_else(|| {
                task.request
                    .as_ref()
                    .map(|r| r.output_path(&task.record.image))
                    .unwrap_or_else(|| task.record.directory.clone())
            }),
            phase: task.phase.clone(),
            ratio: task.ratio(),
            speed: task.speed(),
            packing: task.packing,
        }
    }
}

pub(super) struct TaskDetails {
    snapshot: Option<Snapshot>,
    _subscription: Subscription,
}
impl TaskDetails {
    pub fn new(
        id: u64,
        task: &DownloadTask,
        owner: &Entity<AppView>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(owner, move |view, owner, cx| {
            view.snapshot = owner
                .read(cx)
                .tasks
                .iter()
                .find(|t| t.record.id == id)
                .map(Snapshot::from_task);
            cx.notify();
        });
        Self {
            snapshot: Some(Snapshot::from_task(task)),
            _subscription: subscription,
        }
    }
    fn field(p: Palette, title: &str, value: String) -> Div {
        div()
            .flex()
            .gap_3()
            .items_start()
            .child(p.caption(title.to_owned()).w(px(44.)).flex_shrink_0())
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_xs()
                    .whitespace_normal()
                    .child(value),
            )
    }
    fn layer(p: Palette, index: usize, layer: &LayerProgress, state: State) -> impl IntoElement {
        let color = if layer.complete() { p.green } else { p.accent };
        div()
            .py_3()
            .flex()
            .flex_col()
            .gap_2()
            .border_b_1()
            .border_color(rgb(p.line))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_xs()
                            .w(px(46.))
                            .flex_shrink_0()
                            .child(format!("层 {}", index + 1)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .font_family("Consolas")
                            .truncate()
                            .child(layer.digest.clone()),
                    )
                    .child(
                        p.caption(layer.label(state))
                            .text_color(rgb(if layer.complete() { p.green } else { p.muted })),
                    ),
            )
            .child(p.progress(layer.ratio(), color))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(p.caption(format!(
                        "{} / {}",
                        human_bytes(layer.downloaded),
                        human_bytes(layer.total)
                    )))
                    .child(p.caption(format!("{:.0}%", layer.ratio() * 100.))),
            )
    }
}
impl Render for TaskDetails {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let Some(snapshot) = &self.snapshot else {
            return div().child(p.caption("任务已删除")).into_any_element();
        };
        let r = &snapshot.record;
        let color = match r.state {
            State::Complete => p.green,
            State::Failed => p.red,
            _ => p.accent,
        };
        let completed = r.layer_progress.iter().filter(|l| l.complete()).count();
        let phase = if r.state.active() && !snapshot.phase.is_empty() {
            snapshot.phase.clone()
        } else {
            r.state.label().to_owned()
        };
        let progress = if let Some((done, total)) = snapshot.packing {
            format!("打包 {done}/{total}")
        } else {
            format!("{}/{} 层 · {}", completed, r.layers, human_bytes(r.total))
        };
        div()
            .flex()
            .flex_col()
            .gap_4()
            .font_family(FONT)
            .text_color(rgb(p.text))
            .child(
                div()
                    .id("task-description")
                    .max_h(px(190.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(r.image.clone()),
                            )
                            .child(
                                p.caption(r.state.label())
                                    .text_color(rgb(color))
                                    .flex_shrink_0(),
                            ),
                    )
                    .child(p.caption(format!(
                        "{} · {} · {}",
                        r.platform,
                        format_label(r.format),
                        r.compression.extension()
                    )))
                    .child(Self::field(p, "文件", snapshot.path.display().to_string()))
                    .child(Self::field(p, "摘要", r.digest.clone()))
                    .child(Self::field(
                        p,
                        "下载",
                        format!(
                            "并发 {} · 新分块 {} MiB",
                            r.options.concurrency, r.options.chunk_size_mib
                        ),
                    ))
                    .when_some(r.error.clone(), |d, error| {
                        d.child(div().text_xs().text_color(rgb(p.red)).child(error))
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap_3()
                            .child(p.caption(phase))
                            .child(
                                p.caption(format!("{:.0}%", snapshot.ratio * 100.))
                                    .flex_shrink_0(),
                            ),
                    )
                    .child(p.progress(snapshot.ratio, color))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(p.caption(progress))
                            .when(snapshot.speed > 0, |d| {
                                d.child(p.caption(format!("{}/s", human_bytes(snapshot.speed))))
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child("镜像层"),
                    )
                    .child(
                        div()
                            .id("task-layer-progress")
                            .max_h(px(240.))
                            .overflow_y_scroll()
                            .when(r.layer_progress.is_empty(), |d| {
                                d.child(
                                    p.caption(if r.layers == 0 {
                                        "无镜像层"
                                    } else {
                                        "暂无层明细，继续任务后获取"
                                    })
                                    .py_3(),
                                )
                            })
                            .children(
                                r.layer_progress
                                    .iter()
                                    .enumerate()
                                    .map(|(i, layer)| Self::layer(p, i, layer, r.state)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .when(r.state == State::Complete, |d| {
                        let path = snapshot.path.clone();
                        d.child(
                            Button::new("details-open-file")
                                .ghost()
                                .icon(IconName::Folder)
                                .label("打开文件位置")
                                .on_click(move |_, _, cx| cx.reveal_path(&path)),
                        )
                    })
                    .child(
                        Button::new("details-close")
                            .label("关闭")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
            .into_any_element()
    }
}
