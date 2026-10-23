mod assets;
mod cache_actions;
mod cache_ops;
mod chrome;
mod connection;
mod details;
mod store;
mod task;
mod theme;
mod wizard;

use connection::{ConnectionForm, ConnectionUpdate, NetworkSettings};
use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    Disableable, Icon, IconName, Root, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    select::{Select, SelectState},
};
use gpui_kit::{prelude::*, *};
use oci_pull::download::DownloadManager;
use oci_pull::{model::*, registry};
use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use store::TaskStore;
use task::{Defaults, DownloadTask, State, config_dir, format_label, save_json};
use theme::*;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;
use wizard::{Submission, Wizard};

type Dropdown = Entity<SelectState<Vec<String>>>;

fn dropdown(mut items: Vec<String>, selected: &str, window: &mut Window, cx: &mut App) -> Dropdown {
    if !items.iter().any(|s| s == selected) {
        items.push(selected.into());
    }
    let state = cx.new(|cx| SelectState::new(items, None, window, cx));
    state.update(cx, |state, cx| {
        state.set_selected_value(&selected.to_owned(), window, cx)
    });
    state
}
fn choice(state: &Dropdown, cx: &App) -> String {
    state.read(cx).selected_value().cloned().unwrap_or_default()
}
impl Palette {
    fn caption(self, text: impl Into<SharedString>) -> Div {
        div()
            .text_xs()
            .text_color(rgb(self.muted))
            .child(text.into())
    }
    fn progress(self, ratio: f32, color: u32) -> Div {
        div()
            .w_full()
            .h(px(4.))
            .rounded_full()
            .bg(rgb(self.line))
            .child(
                div()
                    .h_full()
                    .w(relative(ratio.clamp(0., 1.)))
                    .rounded_full()
                    .bg(rgb(color)),
            )
    }
    fn field(self, title: &str, control: impl IntoElement) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(self.text))
                    .child(title.to_owned()),
            )
            .child(control)
    }
}

enum Message {
    Event(u64, Event),
    Finished(u64, Result<(), String>),
    CacheCleaned(oci_pull::cache::Cleanup),
    CacheChanged(cache_ops::ChangeResult),
    Removed(u64, Result<(), String>),
    Warning(String),
}
struct AppView {
    runtime: Arc<Runtime>,
    tasks: Vec<DownloadTask>,
    store: TaskStore,
    defaults: Defaults,
    sender: mpsc::Sender<Message>,
    receiver: mpsc::Receiver<Message>,
    _poller: Task<()>,
    search: Entity<InputState>,
    filter: Dropdown,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
    wizard_subscription: Option<Subscription>,
    connection_subscription: Option<Subscription>,
    network: NetworkSettings,
    network_override: bool,
    downloads: Arc<DownloadManager>,
    cache_status: Entity<cache_ops::CacheStatus>,
    cache_busy: bool,
    pending_cache_change: Option<cache_ops::PendingChange>,
    cache_change_running: bool,
}
impl AppView {
    fn new(runtime: Arc<Runtime>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut defaults: Defaults = std::fs::read(config_dir().join("preferences.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if defaults.download.validate().is_err() {
            defaults.download = DownloadSettings::default();
        }
        if !CHUNK_SIZES_MIB.contains(&defaults.download.chunk_size_mib) {
            defaults.download.chunk_size_mib = 64;
        }
        let downloads = DownloadManager::new(defaults.download).unwrap();
        let network = NetworkSettings {
            download: defaults.download,
            ..Default::default()
        };
        let store = TaskStore::new(data_root().join("tasks"));
        let (tasks, error) = store.load();
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索镜像"));
        let filter = dropdown(
            ["全部任务", "进行中", "已完成", "已暂停", "失败"]
                .map(String::from)
                .to_vec(),
            "全部任务",
            window,
            cx,
        );
        let subscriptions = vec![
            cx.observe_window_appearance(window, |_, window, cx| {
                theme::apply(window.appearance().into(), cx);
                window.refresh();
                cx.notify();
            }),
            cx.observe(&search, |_, _, cx| cx.notify()),
            cx.observe(&filter, |_, _, cx| cx.notify()),
        ];
        let (sender, receiver) = mpsc::channel();
        let poller = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                if this.update(cx, |view, cx| view.receive(cx)).is_err() {
                    break;
                }
            }
        });
        let mut view = Self {
            cache_status: cx.new(|_| cache_ops::CacheStatus::default()),
            cache_busy: false,
            pending_cache_change: None,
            cache_change_running: false,
            runtime,
            tasks,
            store,
            defaults,
            sender,
            receiver,
            _poller: poller,
            search,
            filter,
            error,
            _subscriptions: subscriptions,
            wizard_subscription: None,
            connection_subscription: None,
            network,
            network_override: false,
            downloads,
        };
        view.collect_cache(false, cx);
        view
    }
    fn save(&mut self) {
        if self.cache_change_running {
            return;
        }
        for task in &self.tasks {
            if task.removing {
                continue;
            }
            if let Err(e) = self.store.save(task) {
                self.error = Some(format!("保存任务记录失败：{e:#}"));
                break;
            }
        }
    }
    fn open_wizard(&mut self, id: Option<u64>, window: &mut Window, cx: &mut Context<Self>) {
        if self.cache_busy {
            return;
        }
        let defaults = id
            .and_then(|id| self.tasks.iter().find(|t| t.record.id == id))
            .map(|t| Defaults::from(&t.record))
            .unwrap_or_else(|| {
                let mut defaults = self.defaults.clone();
                defaults.image.clear();
                defaults
            });
        let preview_cache =
            match oci_pull::cache::CacheManager::new(self.defaults.cache_dir.clone()).preview() {
                Ok(directory) => directory,
                Err(e) => {
                    self.error = Some(format!("创建预览缓存失败：{e}"));
                    cx.notify();
                    return;
                }
            };
        let wizard = cx.new(|cx| {
            Wizard::new(
                defaults,
                self.network.clone(),
                self.runtime.clone(),
                preview_cache,
                window,
                cx,
            )
        });
        self.wizard_subscription = Some(cx.subscribe_in(
            &wizard,
            window,
            move |view, _, event: &Submission, window, cx| {
                let request = event.request.clone();
                let plan = event.plan.clone();
                let output = request.output_path(&plan.reference);
                if view.tasks.iter().any(|t| {
                    Some(t.record.id) != id
                        && (t.record.state.active() || t.record.state == State::Queued)
                        && t.request
                            .as_ref()
                            .is_some_and(|r| r.output_path(&t.record.image) == output)
                }) {
                    view.error = Some("已有相同输出文件的任务".into());
                    window.close_dialog(cx);
                    cx.notify();
                    return;
                }
                let task_id = id.unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64
                });
                let task = DownloadTask::new(
                    task_id,
                    request,
                    plan,
                    oci_pull::cache::CacheManager::new(view.defaults.cache_dir.clone())
                        .task_path(task_id),
                );
                if let Err(e) = view.store.save(&task) {
                    view.error = Some(format!("创建任务目录失败：{e:#}"));
                    window.close_dialog(cx);
                    cx.notify();
                    return;
                }
                let old_roots = view.defaults.used_cache_roots.clone();
                view.defaults = Defaults::from(&task.record);
                view.defaults.used_cache_roots = old_roots;
                if let Err(e) = save_json("preferences.json", &view.defaults) {
                    view.error = Some(format!("保存偏好失败：{e:#}"));
                }
                if let Some(index) = view.tasks.iter().position(|t| t.record.id == task_id) {
                    view.tasks[index] = task;
                } else {
                    view.tasks.push(task);
                }
                view.schedule();
                view.save();
                window.close_dialog(cx);
                cx.notify();
            },
        ));
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .p_0()
                .title(
                    div()
                        .px_4()
                        .pt_4()
                        .text_base()
                        .font_weight(FontWeight::MEDIUM)
                        .child("新建任务"),
                )
                .width(px(560.))
                .overlay_closable(false)
                .close_button(false)
                .keyboard(false)
                .child(wizard.clone())
        });
    }
    fn open_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = cx.new(|cx| {
            ConnectionForm::new(
                self.network.clone(),
                self.defaults.cache_dir.clone(),
                self.cache_status.clone(),
                window,
                cx,
            )
        });
        self.connection_subscription = Some(cx.subscribe_in(
            &form,
            window,
            move |view, _, event: &ConnectionUpdate, window, cx| match event {
                ConnectionUpdate::Save(settings, root) => {
                    view.request_settings(settings.clone(), root.clone(), window, cx)
                }
                ConnectionUpdate::ClearCache => view.collect_cache(true, cx),
            },
        ));
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("设置")
                .width(px(460.))
                .overlay_closable(false)
                .child(form.clone())
        });
    }
    fn open_download_folder(&mut self, cx: &mut Context<Self>) {
        let path = self.defaults.output.clone();
        match std::fs::create_dir_all(&path) {
            Ok(()) => cx.open_with_system(&path),
            Err(error) => {
                self.error = Some(format!("打开下载目录失败：{error}"));
                cx.notify();
            }
        }
    }
    fn open_details(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = self.tasks.iter().find(|task| task.record.id == id) else {
            return;
        };
        let owner = cx.entity();
        let details = cx.new(|cx| details::TaskDetails::new(id, task, &owner, cx));
        window.open_dialog(cx, move |dialog, window, _| {
            dialog
                .title("任务详情")
                .width(px(660.))
                .max_h((window.viewport_size().height - px(130.)).max(px(300.)))
                .child(details.clone())
        });
    }
    fn schedule(&mut self) {
        if self.cache_busy {
            return;
        }
        let mut available = 2usize.saturating_sub(
            self.tasks
                .iter()
                .filter(|t| t.record.state.active())
                .count(),
        );
        for task in &mut self.tasks {
            if available == 0 {
                break;
            }
            if task.record.state != State::Queued || task.removing {
                continue;
            }
            let Some(mut request) = task.request.clone() else {
                task.record.state = State::Failed;
                task.record.error = Some("无法恢复任务参数".into());
                continue;
            };
            let plan = task.plan.clone();
            let digest = task.record.digest.clone();
            available -= 1;
            task.record.state = State::Downloading;
            task.record.error = None;
            task.reset_progress();
            task.started = Some(Instant::now());
            task.cancel = CancellationToken::new();
            let id = task.record.id;
            let sender = self.sender.clone();
            let cancel = task.cancel.clone();
            let progress_sender = sender.clone();
            let downloads = self.downloads.clone();
            let reporter: Reporter = Arc::new(move |event| {
                let _ = progress_sender.send(Message::Event(id, event));
            });
            let cache_root = task.record.options.cache_root.clone();
            self.runtime.spawn(async move {
                let result = async {
                    let lease = oci_pull::cache::CacheManager::new(cache_root).task(id)?;
                    lease.bind(&mut request)?;
                    let result = match plan {
                        Some(plan) => {
                            registry::pull_resolved_managed(
                                request, plan, reporter, cancel, downloads,
                            )
                            .await
                        }
                        None => {
                            registry::pull_pinned_managed(
                                request, &digest, reporter, cancel, downloads,
                            )
                            .await
                        }
                    };
                    if result.is_ok() {
                        let cleanup = tokio::task::spawn_blocking(move || lease.clear()).await?;
                        if let Err(error) = cleanup {
                            let _ = sender.send(Message::Warning(format!(
                                "任务已完成，缓存将在下次清理时重试：{error:#}"
                            )));
                        }
                    }
                    result.map(|_| ())
                }
                .await
                .map_err(|e: anyhow::Error| format!("{e:#}"));
                let _ = sender.send(Message::Finished(id, result));
            });
        }
    }
    fn receive(&mut self, cx: &mut Context<Self>) {
        let mut dirty = false;
        let mut finished = false;
        let mut removals = Vec::new();
        let messages: Vec<_> = self.receiver.try_iter().collect();
        for message in messages {
            let message = match message {
                Message::CacheCleaned(result) => {
                    for (root, id) in result.tasks {
                        if let Some(task) = self.tasks.iter_mut().find(|t| {
                            t.record.id == id
                                && t.record.options.cache_root == root
                                && t.record.state != State::Complete
                        }) {
                            task.reset_progress();
                        }
                    }
                    let message = if result.errors.is_empty() {
                        format!(
                            "已清理 {}，跳过 {} 个使用中缓存",
                            human_bytes(result.bytes),
                            result.skipped
                        )
                    } else {
                        format!(
                            "已清理 {}；{}",
                            human_bytes(result.bytes),
                            result.errors.join("；")
                        )
                    };
                    self.cache_status(false, message, cx);
                    finished = true;
                    dirty = true;
                    continue;
                }
                Message::CacheChanged(result) => {
                    self.finish_cache_change(result, cx);
                    dirty = true;
                    continue;
                }
                Message::Removed(id, result) => {
                    match result {
                        Ok(()) => self.tasks.retain(|t| t.record.id != id),
                        Err(error) => {
                            if let Some(task) = self.tasks.iter_mut().find(|t| t.record.id == id) {
                                task.removing = false;
                            }
                            self.error = Some(error);
                        }
                    }
                    finished = true;
                    dirty = true;
                    continue;
                }
                Message::Warning(error) => {
                    self.error = Some(error);
                    dirty = true;
                    continue;
                }
                other => other,
            };
            let id = match &message {
                Message::Event(id, _) | Message::Finished(id, _) => *id,
                _ => unreachable!(),
            };
            let Some(task) = self.tasks.iter_mut().find(|t| t.record.id == id) else {
                continue;
            };
            dirty = true;
            match message {
                Message::Event(_, event) => task.apply(event),
                Message::Finished(_, result) => {
                    finished = true;
                    if task.removing {
                        removals.push(id);
                    }
                    match result {
                        Ok(()) => task.record.state = State::Complete,
                        Err(_) if task.cancel.is_cancelled() => {
                            task.record.state = State::Paused;
                            task.record.error = None;
                        }
                        Err(error) => {
                            task.record.state = State::Failed;
                            task.record.error = Some(error);
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        for id in removals {
            self.remove(id, cx);
        }
        self.try_cache_change(cx);
        if finished {
            self.schedule();
            self.save();
        }
        if dirty {
            cx.notify();
        }
    }
    fn pause(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.cache_busy {
            return;
        }
        if let Some(task) = self.tasks.iter_mut().find(|t| t.record.id == id) {
            if task.record.state == State::Queued {
                task.record.state = State::Paused;
            } else if task.record.state.active() {
                task.record.state = State::Pausing;
                task.cancel.cancel();
            }
        }
        self.save();
        cx.notify();
    }
    fn resume(&mut self, id: u64, _window: &mut Window, cx: &mut Context<Self>) {
        if self.cache_busy {
            return;
        }
        let Some(task) = self.tasks.iter_mut().find(|t| t.record.id == id) else {
            return;
        };
        if let Err(error) = task.prepare_resume() {
            task.record.error = Some(format!("恢复任务失败：{error:#}"));
            self.error = task.record.error.clone();
            self.save();
            cx.notify();
            return;
        }
        if self.network_override
            && let Some(request) = &mut task.request
        {
            self.network.apply(request);
            task.record.options.requires_proxy = request.proxy.is_some();
        }
        if let Some(request) = &mut task.request {
            self.network.download.apply(request);
        }
        if task.needs_connection() {
            self.error =
                Some("请先在主界面的“设置”中补充此任务的代理或仓库凭据，再点击继续".into());
            cx.notify();
            return;
        }
        task.record.options = task::ResumeOptions::from(task.request.as_ref().unwrap());
        let output = task
            .request
            .as_ref()
            .unwrap()
            .output_path(&task.record.image);
        if self.tasks.iter().any(|t| {
            t.record.id != id
                && (t.record.state.active() || t.record.state == State::Queued)
                && t.request
                    .as_ref()
                    .is_some_and(|r| r.output_path(&t.record.image) == output)
        }) {
            self.error = Some("已有相同输出文件的任务".into());
            cx.notify();
            return;
        }
        let task = self.tasks.iter_mut().find(|t| t.record.id == id).unwrap();
        task.record.state = State::Queued;
        task.record.error = None;
        self.schedule();
        self.save();
        cx.notify();
    }
    fn remove(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.cache_busy && !self.tasks.iter().any(|t| t.record.id == id && t.removing) {
            return;
        }
        if let Some(task) = self.tasks.iter_mut().find(|t| t.record.id == id)
            && !task.prepare_remove()
        {
            self.save();
            cx.notify();
            return;
        }
        let Some(task) = self.tasks.iter_mut().find(|t| t.record.id == id) else {
            return;
        };
        task.record.state = State::Paused;
        let root = task.record.options.cache_root.clone();
        let store = self.store.clone();
        let sender = self.sender.clone();
        self.runtime.spawn_blocking(move || {
            let result = (|| -> anyhow::Result<()> {
                oci_pull::cache::CacheManager::new(root).remove_task(id)?;
                store.remove_record(id)
            })()
            .map_err(|e| format!("删除任务失败：{e:#}"));
            let _ = sender.send(Message::Removed(id, result));
        });
        cx.notify();
    }
    fn row(&self, task: &DownloadTask, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let r = &task.record;
        let id = r.id;
        let state = r.state;
        let color = match state {
            State::Complete => p.green,
            State::Failed => p.red,
            State::Downloading | State::Packaging => p.accent,
            _ => p.muted,
        };
        let rate = task.speed();
        let rate = if rate == 0 {
            "—".into()
        } else {
            format!("{}/s", human_bytes(rate))
        };
        let size = if state == State::Complete {
            r.output_bytes
        } else {
            r.total
        };
        let percent = if state == State::Queued || (state == State::Paused && task.rows.is_empty())
        {
            "—".into()
        } else {
            format!("{:.0}%", task.ratio() * 100.)
        };
        div()
            .id(("task", id))
            .h(px(64.))
            .px_3()
            .flex()
            .items_center()
            .gap_4()
            .border_b_1()
            .border_color(rgb(p.line))
            .bg(rgb(p.panel))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(p.subtle)))
            .on_click(cx.listener(move |view, _, window, cx| view.open_details(id, window, cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(r.image.clone()),
                    )
                    .child(p.caption(r.platform.clone()).font_family("Consolas")),
            )
            .child(
                div()
                    .w(px(86.))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_sm().child(format_label(r.format)))
                    .child(p.caption(r.compression.extension())),
            )
            .child(
                div()
                    .w(px(88.))
                    .flex_shrink_0()
                    .text_sm()
                    .child(human_bytes(size)),
            )
            .child(
                div()
                    .w(px(130.))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(p.caption(percent))
                            .child(p.caption(rate)),
                    )
                    .child(p.progress(task.ratio(), color)),
            )
            .child(
                div()
                    .w(px(64.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(div().size(px(5.)).rounded_full().bg(rgb(color)))
                    .child(
                        p.caption(if task.removing {
                            "删除中"
                        } else {
                            state.label()
                        })
                        .text_color(rgb(color)),
                    ),
            )
            .child(
                div()
                    .w(px(142.))
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new(("resume", id))
                            .small()
                            .ghost()
                            .icon(Icon::new(LucideIcon::Play))
                            .tooltip("继续")
                            .disabled(
                                self.cache_busy
                                    || task.removing
                                    || !matches!(state, State::Paused | State::Failed),
                            )
                            .on_click(
                                cx.listener(move |view, _, window, cx| view.resume(id, window, cx)),
                            ),
                    )
                    .child(
                        Button::new(("pause", id))
                            .small()
                            .ghost()
                            .icon(Icon::new(LucideIcon::Pause))
                            .tooltip("暂停")
                            .disabled(
                                self.cache_busy
                                    || task.removing
                                    || !matches!(
                                        state,
                                        State::Queued | State::Downloading | State::Packaging
                                    ),
                            )
                            .on_click(cx.listener(move |view, _, _, cx| view.pause(id, cx))),
                    )
                    .child(
                        Button::new(("remove", id))
                            .small()
                            .ghost()
                            .icon(Icon::new(LucideIcon::Trash))
                            .tooltip("删除任务")
                            .disabled(self.cache_busy || task.removing)
                            .on_click(cx.listener(move |view, _, _, cx| view.remove(id, cx))),
                    ),
            )
    }
}
impl Drop for AppView {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.cancel.cancel();
        }
    }
}
impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let filter = choice(&self.filter, cx);
        let query = self.search.read(cx).value().to_lowercase();
        let visible: Vec<usize> = self
            .tasks
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, t)| {
                t.record.image.to_lowercase().contains(&query)
                    && match filter.as_str() {
                        "进行中" => t.record.state.active() || t.record.state == State::Queued,
                        "已完成" => t.record.state == State::Complete,
                        "已暂停" => t.record.state == State::Paused,
                        "失败" => t.record.state == State::Failed,
                        _ => true,
                    }
            })
            .map(|(i, _)| i)
            .collect();
        let rows: Vec<_> = visible
            .iter()
            .map(|i| self.row(&self.tasks[*i], cx).into_any_element())
            .collect();
        let dialog = Root::render_dialog_layer(window, cx);
        div()
            .size_full()
            .relative()
            .bg(rgb(p.bg))
            .text_color(rgb(p.text))
            .font_family(FONT)
            .flex()
            .flex_col()
            .child(chrome::title_bar(cx))
            .child(
                div()
                    .h(px(64.))
                    .flex_shrink_0()
                    .px_6()
                    .border_b_1()
                    .border_color(rgb(p.line))
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("任务"),
                    )
                    .child(
                        p.caption(format!("{}", self.tasks.len()))
                            .px_2()
                            .py(px(2.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(rgb(p.line)),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .w(px(225.))
                            .child(Input::new(&self.search).prefix(IconName::Search)),
                    )
                    .child(div().w(px(125.)).child(Select::new(&self.filter)))
                    .child(
                        Button::new("connection-settings")
                            .ghost()
                            .icon(Icon::new(LucideIcon::SlidersHorizontal))
                            .tooltip("设置")
                            .on_click(
                                cx.listener(|view, _, window, cx| view.open_connection(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("open-download-folder")
                            .ghost()
                            .icon(IconName::Folder)
                            .tooltip("打开下载文件夹")
                            .on_click(cx.listener(|view, _, _, cx| view.open_download_folder(cx))),
                    )
                    .child(
                        Button::new("new-task")
                            .primary()
                            .icon(IconName::Plus)
                            .label("新建任务")
                            .disabled(self.cache_busy)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.open_wizard(None, window, cx)
                            })),
                    ),
            )
            .when(self.cache_busy, |d| {
                d.child(
                    p.caption(
                        self.cache_status
                            .read(cx)
                            .message
                            .clone()
                            .unwrap_or_default(),
                    )
                    .mx_6()
                    .mt_3(),
                )
            })
            .when_some(self.error.clone(), |d, error| {
                d.child(
                    div()
                        .mx_6()
                        .mt_3()
                        .p_3()
                        .bg(rgb(p.error_bg))
                        .rounded_md()
                        .flex()
                        .justify_between()
                        .gap_3()
                        .child(div().text_sm().text_color(rgb(p.red)).child(error))
                        .child(
                            Button::new("dismiss-error")
                                .ghost()
                                .small()
                                .icon(IconName::Close)
                                .on_click(cx.listener(|v, _, _, cx| {
                                    v.error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .mx_6()
                    .mt_4()
                    .mb_5()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .bg(rgb(p.panel))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(36.))
                            .flex_shrink_0()
                            .px_3()
                            .flex()
                            .items_center()
                            .gap_4()
                            .border_b_1()
                            .border_color(rgb(p.line))
                            .bg(rgb(p.panel))
                            .child(div().flex_1().child(p.caption("镜像 / 平台")))
                            .child(div().w(px(86.)).flex_shrink_0().child(p.caption("格式")))
                            .child(div().w(px(88.)).flex_shrink_0().child(p.caption("大小")))
                            .child(
                                div()
                                    .w(px(130.))
                                    .flex_shrink_0()
                                    .child(p.caption("进度 / 速度")),
                            )
                            .child(div().w(px(64.)).flex_shrink_0().child(p.caption("状态")))
                            .child(
                                div()
                                    .w(px(142.))
                                    .flex_shrink_0()
                                    .text_right()
                                    .child(p.caption("操作")),
                            ),
                    )
                    .child(
                        div()
                            .id("task-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .when(rows.is_empty(), |d| {
                                d.child(
                                    div().h_full().flex().items_center().justify_center().child(
                                        p.caption(if query.is_empty() && filter == "全部任务" {
                                            "暂无任务"
                                        } else {
                                            "无匹配任务"
                                        }),
                                    ),
                                )
                            })
                            .children(rows),
                    ),
            )
            .children(dialog)
    }
}
pub fn run() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            theme::apply(cx.window_appearance().into(), cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1160.), px(760.)),
                    cx,
                ))),
                window_min_size: Some(size(px(1000.), px(660.))),
                ..chrome::window_options()
            };
            cx.spawn(async move |cx| {
                if let Err(e) = cx.open_window(options, |window, cx| {
                    let view = cx.new(|cx| AppView::new(runtime, window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                }) {
                    eprintln!("{e}");
                }
            })
            .detach();
            cx.activate(true);
        });
}
