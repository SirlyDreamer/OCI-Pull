use super::*;
use gpui_kit::component::input::InputEvent;
use oci_pull::model::Platform as ImagePlatform;
use std::{path::PathBuf, sync::mpsc};

pub struct Submission {
    pub request: PullRequest,
    pub plan: ImagePlan,
}
impl EventEmitter<Submission> for Wizard {}

pub struct Wizard {
    step: usize,
    image: Entity<InputState>,
    platform: Dropdown,
    format: Dropdown,
    compression: Dropdown,
    output: Entity<InputState>,
    file_name: Entity<InputState>,
    auto_name: String,
    network: NetworkSettings,
    busy: bool,
    error: Option<String>,
    preview: Option<Submission>,
    submitted: bool,
    runtime: Arc<Runtime>,
    receiver: Option<mpsc::Receiver<Result<Submission, String>>>,
    cancel: CancellationToken,
    _poller: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    preview_cache: Arc<oci_pull::cache::CacheLease>,
}
impl Wizard {
    pub fn new(
        defaults: Defaults,
        network: NetworkSettings,
        runtime: Arc<Runtime>,
        preview_cache: Arc<oci_pull::cache::CacheLease>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let image = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(defaults.image)
                .placeholder("docker.io/library/alpine:latest")
        });
        let output = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(defaults.output.display().to_string())
                .placeholder("保存目录")
        });
        let file_name = cx.new(|cx| InputState::new(window, cx).placeholder("文件名"));
        let platform = dropdown(
            [
                "linux/amd64",
                "linux/arm64",
                "linux/arm/v7",
                "linux/arm/v6",
                "linux/386",
                "linux/ppc64le",
                "linux/riscv64",
                "linux/s390x",
                "windows/amd64",
            ]
            .map(String::from)
            .to_vec(),
            &defaults.platform,
            window,
            cx,
        );
        let format = dropdown(
            vec!["Docker 兼容".into(), "OCI Archive".into()],
            if defaults.format == ArchiveFormat::Docker {
                "Docker 兼容"
            } else {
                "OCI Archive"
            },
            window,
            cx,
        );
        let compression = dropdown(
            ["tar.zst", "tar.gz", "tar.xz", "tar.bz2", "tar"]
                .map(String::from)
                .to_vec(),
            defaults.compression.extension(),
            window,
            cx,
        );
        let subscriptions = vec![
            cx.observe_in(&compression, window, |view, _, window, cx| {
                view.normalize_file_name(window, cx);
                cx.notify();
            }),
            cx.subscribe_in(&file_name, window, |view, _, event, window, cx| {
                if matches!(event, InputEvent::Blur) {
                    view.normalize_file_name(window, cx);
                }
            }),
        ];
        Self {
            step: 0,
            image,
            platform,
            format,
            compression,
            output,
            file_name,
            auto_name: String::new(),
            network,
            busy: false,
            error: None,
            preview: None,
            submitted: false,
            runtime,
            receiver: None,
            cancel: CancellationToken::new(),
            _poller: None,
            _subscriptions: subscriptions,
            preview_cache,
        }
    }
    fn compression(&self, cx: &App) -> Compression {
        match choice(&self.compression, cx).as_str() {
            "tar.gz" => Compression::Gzip,
            "tar.bz2" => Compression::Bzip2,
            "tar.xz" => Compression::Xz,
            "tar" => Compression::None,
            _ => Compression::Zstd,
        }
    }
    fn normalize_file_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.file_name.read(cx).value().to_string();
        if let Ok(name) = archive_file_name(&value, self.compression(cx))
            && name != value
        {
            if value == self.auto_name {
                self.auto_name = name.clone();
            }
            self.file_name
                .update(cx, |state, cx| state.set_value(name, window, cx));
        }
    }
    fn request(&self, saving: bool, cx: &App) -> anyhow::Result<PullRequest> {
        let compression = self.compression(cx);
        let mut request = PullRequest {
            image: self.image.read(cx).value().trim().into(),
            platform: ImagePlatform::parse(&choice(&self.platform, cx))?,
            format: if choice(&self.format, cx) == "OCI Archive" {
                ArchiveFormat::Oci
            } else {
                ArchiveFormat::Docker
            },
            compression,
            output_dir: PathBuf::from(self.output.read(cx).value().trim()),
            file_name: if saving {
                Some(archive_file_name(
                    &self.file_name.read(cx).value(),
                    compression,
                )?)
            } else {
                None
            },
            cache_dir: self.preview_cache.path().to_path_buf(),
            manifest_dir: None,
            credentials: Credentials::default(),
            proxy: None,
            allow_http: false,
            concurrency: self.network.download.concurrency,
            chunk_size_mib: self.network.download.chunk_size_mib,
            compression_level: 3,
        };
        self.preview_cache.bind(&mut request)?;
        self.network.apply(&mut request);
        request.validate()?;
        registry::parse_reference(&request.image)?;
        Ok(request)
    }
    fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.submitted {
            return;
        }
        let request = match self.request(self.step == 1, cx) {
            Ok(request) => request,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                cx.notify();
                return;
            }
        };
        self.error = None;
        if self.step == 1 {
            let Some(preview) = &self.preview else {
                return;
            };
            if request.output_path(&preview.plan.reference).exists() {
                self.error = Some("文件已存在，请修改文件名或保存目录".into());
                cx.notify();
                return;
            }
            self.submitted = true;
            cx.emit(Submission {
                request,
                plan: preview.plan.clone(),
            });
            return;
        }
        // Going back without changing the image/platform reuses the same resolution.
        if self.preview.as_ref().is_some_and(|p| {
            p.request.image == request.image && p.request.platform == request.platform
        }) {
            self.step = 1;
            cx.notify();
            return;
        }
        self.busy = true;
        self.preview = None;
        self.cancel = CancellationToken::new();
        let cancel = self.cancel.clone();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        let cache_lease = self.preview_cache.clone();
        self.runtime.spawn(async move {
            let _cache_lease = cache_lease;
            let report: Reporter = Arc::new(|_| {});
            let result = registry::inspect(&request, &report, &cancel)
                .await
                .map(|plan| Submission { request, plan })
                .map_err(|e| format!("{e:#}"));
            let _ = sender.send(result);
        });
        self._poller = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let keep = this.update_in(cx, |view, window, cx| {
                    view.receive(window, cx);
                    view.busy
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }
        }));
        cx.notify();
    }
    fn receive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self.receiver.as_ref().and_then(|r| r.try_recv().ok()) else {
            return;
        };
        self.busy = false;
        self.receiver = None;
        match result {
            Ok(mut preview) => {
                let selected = preview.plan.platform.label();
                let mut items = preview.plan.available_platforms.clone();
                if !items.contains(&selected) {
                    items.push(selected.clone());
                }
                self.platform.update(cx, |state, cx| {
                    state.set_items(items, window, cx);
                    state.set_selected_value(&selected, window, cx);
                });
                preview.request.platform = preview.plan.platform.clone();
                let value = self.file_name.read(cx).value().to_string();
                if value.is_empty() || value == self.auto_name {
                    let generated = preview.request.output_path(&preview.plan.reference);
                    let name = generated.file_name().unwrap_or_default().to_string_lossy();
                    let suffix = format!(
                        "_{}.{}",
                        if preview.request.format == ArchiveFormat::Docker {
                            "docker"
                        } else {
                            "oci"
                        },
                        preview.request.compression.extension()
                    );
                    self.auto_name = format!(
                        "{}.{}",
                        name.strip_suffix(&suffix).unwrap_or(&name),
                        preview.request.compression.extension()
                    );
                    let value = self.auto_name.clone();
                    self.file_name
                        .update(cx, |state, cx| state.set_value(value, window, cx));
                }
                self.preview = Some(preview);
                self.step = 1;
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }
    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("保存到".into()),
        });
        cx.spawn_in(window, async move |this, cx| match picker.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.first() {
                    let value = path.display().to_string();
                    let _ = this.update_in(cx, |view, window, cx| {
                        view.output
                            .update(cx, |input, cx| input.set_value(value, window, cx));
                        cx.notify();
                    });
                }
            }
            Ok(Err(e)) => {
                let _ = this.update(cx, |v, cx| {
                    v.error = Some(e.to_string());
                    cx.notify();
                });
            }
            _ => {}
        })
        .detach();
    }
    fn source(&self, cx: &App) -> impl IntoElement {
        let p = palette(cx);
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(p.field("镜像来源", Input::new(&self.image).disabled(self.busy)))
            .child(p.field("目标平台", Select::new(&self.platform).disabled(self.busy)))
    }
    fn storage(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let mut body = div().flex().flex_col().gap_3();
        if let Some(preview) = &self.preview {
            let plan = &preview.plan;
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .rounded(px(8.))
                    .bg(rgb(p.subtle))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(plan.reference.clone()),
                    )
                    .child(p.caption(format!(
                        "{} · {} · {} 层",
                        plan.platform.label(),
                        human_bytes(plan.total_bytes()),
                        plan.manifest.layers.len()
                    )))
                    .child(
                        p.caption(plan.digest.clone())
                            .font_family("Consolas")
                            .truncate(),
                    ),
            );
        }
        body.child(
            div()
                .flex()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .child(p.field("镜像格式", Select::new(&self.format))),
                )
                .child(
                    div()
                        .flex_1()
                        .child(p.field("压缩格式", Select::new(&self.compression))),
                ),
        )
        .child(p.field("文件名", Input::new(&self.file_name)))
        .child(
            p.field(
                "保存路径",
                div()
                    .flex()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(Input::new(&self.output)))
                    .child(
                        Button::new("browse")
                            .icon(IconName::Folder)
                            .tooltip("选择目录")
                            .on_click(cx.listener(|v, _, window, cx| v.browse(window, cx))),
                    ),
            ),
        )
    }
}
impl Drop for Wizard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl Render for Wizard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let body = if self.step == 0 {
            self.source(cx).into_any_element()
        } else {
            self.storage(cx).into_any_element()
        };
        div()
            .text_color(rgb(p.text))
            .font_family(FONT)
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .pt_1()
                    .pb_3()
                    .border_b_1()
                    .border_color(rgb(p.line))
                    .children(["镜像来源", "保存与确认"].into_iter().enumerate().map(
                        |(i, title)| {
                            div()
                                .flex_1()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .size(px(20.))
                                        .rounded(px(5.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_xs()
                                        .bg(rgb(if i <= self.step { p.accent } else { p.hover }))
                                        .text_color(rgb(if i <= self.step {
                                            p.on_accent
                                        } else {
                                            p.muted
                                        }))
                                        .child((i + 1).to_string()),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(if i == self.step {
                                            p.text
                                        } else {
                                            p.muted
                                        }))
                                        .child(title),
                                )
                        },
                    )),
            )
            .child(
                div()
                    .id("wizard-body")
                    .h(px(if self.step == 0 { 142. } else { 310. }))
                    .overflow_y_scroll()
                    .px_4()
                    .py_1()
                    .child(body),
            )
            .when_some(self.error.clone(), |d, error| {
                d.child(
                    div()
                        .px_3()
                        .py_2()
                        .mx_4()
                        .rounded_md()
                        .bg(rgb(p.error_bg))
                        .text_sm()
                        .text_color(rgb(p.red))
                        .child(error),
                )
            })
            .child(
                div()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(rgb(p.line))
                    .bg(rgb(p.subtle))
                    .rounded_b(px(14.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(Button::new("wizard-cancel").ghost().label("取消").on_click(
                        cx.listener(|v, _, window, cx| {
                            v.cancel.cancel();
                            window.close_dialog(cx);
                        }),
                    ))
                    .child(div().flex_1())
                    .when(self.step > 0, |d| {
                        d.child(
                            Button::new("wizard-back")
                                .label("上一步")
                                .disabled(self.submitted)
                                .on_click(cx.listener(|v, _, _, cx| {
                                    v.step = 0;
                                    v.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        Button::new("wizard-next")
                            .primary()
                            .label(if self.busy {
                                "解析中…"
                            } else if self.step == 1 {
                                "创建任务"
                            } else {
                                "下一步"
                            })
                            .disabled(self.busy || self.submitted)
                            .on_click(cx.listener(|v, _, window, cx| v.next(window, cx))),
                    ),
            )
    }
}
