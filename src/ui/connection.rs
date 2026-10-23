use super::*;

#[derive(Clone)]
pub struct NetworkSettings {
    pub download: DownloadSettings,
    pub proxy: Option<String>,
    pub allow_http: bool,
    pub auth_registry: String,
    pub credentials: Credentials,
}
impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            download: DownloadSettings::default(),
            proxy: None,
            allow_http: false,
            auth_registry: "docker.io".into(),
            credentials: Credentials::default(),
        }
    }
}
impl NetworkSettings {
    pub fn apply(&self, request: &mut PullRequest) {
        request.proxy = self.proxy.clone();
        request.allow_http = self.allow_http;
        if registry::parse_reference(&request.image)
            .is_ok_and(|r| r.registry() == self.auth_registry)
        {
            request.credentials = self.credentials.clone();
        }
    }
    fn validate(&self) -> anyhow::Result<()> {
        self.download.validate()?;
        if let Some(proxy) = &self.proxy {
            anyhow::ensure!(
                proxy.starts_with("http://") || proxy.starts_with("https://"),
                "代理地址应以 http:// 或 https:// 开头"
            );
        }
        anyhow::ensure!(
            self.credentials.username.is_empty() == self.credentials.password.is_empty(),
            "用户名和密码／令牌必须同时填写"
        );
        if !self.credentials.username.is_empty() {
            anyhow::ensure!(
                !self.auth_registry.is_empty()
                    && !self.auth_registry.contains('/')
                    && !self.auth_registry.contains(char::is_whitespace),
                "请填写认证仓库地址，例如 docker.io"
            );
            let reference =
                registry::parse_reference(&format!("{}/oci-pull:latest", self.auth_registry))?;
            anyhow::ensure!(reference.registry() == self.auth_registry, "仓库地址无效");
        }
        Ok(())
    }
}
pub struct ConnectionForm {
    concurrency: Entity<InputState>,
    cache_dir: Entity<InputState>,
    cache_status: Entity<super::cache_ops::CacheStatus>,
    _cache_subscription: Subscription,
    chunk_size: Dropdown,
    protocol: Dropdown,
    proxy: Entity<InputState>,
    registry: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    error: Option<String>,
}
pub enum ConnectionUpdate {
    Save(NetworkSettings, std::path::PathBuf),
    ClearCache,
}
impl EventEmitter<ConnectionUpdate> for ConnectionForm {}
impl ConnectionForm {
    pub fn new(
        settings: NetworkSettings,
        cache_dir: std::path::PathBuf,
        cache_status: Entity<super::cache_ops::CacheStatus>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let concurrency = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.download.concurrency.to_string())
                .placeholder("16")
        });
        let cache_dir =
            cx.new(|cx| InputState::new(window, cx).default_value(cache_dir.display().to_string()));
        let cache_subscription = cx.observe(&cache_status, |_, _, cx| cx.notify());
        let chunk_size = dropdown(
            CHUNK_SIZES_MIB.map(|n| format!("{n} MiB")).to_vec(),
            &format!("{} MiB", settings.download.chunk_size_mib),
            window,
            cx,
        );
        let protocol = dropdown(
            vec!["HTTPS".into(), "HTTP".into()],
            if settings.allow_http { "HTTP" } else { "HTTPS" },
            window,
            cx,
        );
        let proxy = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.proxy.unwrap_or_default())
                .placeholder("http://127.0.0.1:7890")
        });
        let registry = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.auth_registry)
                .placeholder("docker.io")
        });
        let username =
            cx.new(|cx| InputState::new(window, cx).default_value(settings.credentials.username));
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(settings.credentials.password)
                .masked(true)
        });
        Self {
            concurrency,
            cache_dir,
            cache_status,
            _cache_subscription: cache_subscription,
            chunk_size,
            protocol,
            proxy,
            registry,
            username,
            password,
            error: None,
        }
    }
    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("缓存目录".into()),
        });
        cx.spawn_in(window, async move |this, cx| match picker.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.first() {
                    let value = path.display().to_string();
                    let _ = this.update_in(cx, |view, window, cx| {
                        view.cache_dir
                            .update(cx, |input, cx| input.set_value(value, window, cx));
                        cx.notify();
                    });
                }
            }
            Ok(Err(error)) => {
                let _ = this.update(cx, |view, cx| {
                    view.error = Some(error.to_string());
                    cx.notify();
                });
            }
            _ => {}
        })
        .detach();
    }
    fn submit(&mut self, cx: &mut Context<Self>) {
        let proxy = self.proxy.read(cx).value().trim().to_owned();
        let settings = NetworkSettings {
            download: DownloadSettings {
                concurrency: self
                    .concurrency
                    .read(cx)
                    .value()
                    .trim()
                    .parse()
                    .unwrap_or(0),
                chunk_size_mib: choice(&self.chunk_size, cx)
                    .trim_end_matches(" MiB")
                    .parse()
                    .unwrap_or(0),
            },
            allow_http: choice(&self.protocol, cx) == "HTTP",
            proxy: (!proxy.is_empty()).then_some(proxy),
            auth_registry: self.registry.read(cx).value().trim().to_ascii_lowercase(),
            credentials: Credentials {
                username: self.username.read(cx).value().trim().into(),
                password: self.password.read(cx).value().to_string(),
            },
        };
        match settings.validate() {
            Ok(()) => {
                let root = std::path::PathBuf::from(self.cache_dir.read(cx).value().trim());
                if !root.is_absolute() {
                    self.error = Some("缓存目录必须是绝对路径".into());
                    cx.notify();
                    return;
                }
                cx.emit(ConnectionUpdate::Save(settings, root));
            }
            Err(e) => {
                self.error = Some(e.to_string());
                cx.notify();
            }
        }
    }
}
impl Render for ConnectionForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let busy = self.cache_status.read(cx).busy;
        let status = self.cache_status.read(cx).message.clone();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .text_color(rgb(p.text))
            .font_family(FONT)
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div().flex_1().child(
                            p.field("下载并发数", Input::new(&self.concurrency).disabled(busy)),
                        ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(p.field("分块大小", Select::new(&self.chunk_size))),
                    ),
            )
            .child(
                p.field(
                    "缓存目录",
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&self.cache_dir).disabled(busy)),
                        )
                        .child(
                            Button::new("cache-browse")
                                .icon(IconName::Folder)
                                .disabled(busy)
                                .on_click(
                                    cx.listener(|view, _, window, cx| view.browse(window, cx)),
                                ),
                        ),
                ),
            )
            .when_some(status, |d, message| d.child(p.caption(message)))
            .child(p.field("代理", Input::new(&self.proxy)))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .w(px(115.))
                            .child(p.field("仓库协议", Select::new(&self.protocol))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(p.field("认证仓库", Input::new(&self.registry))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .child(p.field("用户名", Input::new(&self.username))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .child(p.field("密码 / 令牌", Input::new(&self.password))),
                    ),
            )
            .when_some(self.error.clone(), |d, error| {
                d.child(div().text_sm().text_color(rgb(p.red)).child(error))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .pt_2()
                    .child(
                        Button::new("clear-cache")
                            .ghost()
                            .icon(Icon::new(LucideIcon::Trash))
                            .label("清除缓存")
                            .disabled(busy)
                            .tooltip("清除闲置缓存，包括暂停任务的断点；正在使用的缓存会跳过")
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(ConnectionUpdate::ClearCache)),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("connection-cancel")
                            .ghost()
                            .label("取消")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("connection-save")
                            .primary()
                            .label("保存")
                            .disabled(busy)
                            .on_click(cx.listener(|v, _, _, cx| v.submit(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::NetworkSettings;
    use oci_pull::model::{ArchiveFormat, Compression, Credentials, Platform, PullRequest};
    #[test]
    fn global_proxy_does_not_forward_credentials_to_a_different_registry() {
        let settings = NetworkSettings {
            proxy: Some("http://localhost:7890".into()),
            credentials: Credentials {
                username: "user".into(),
                password: "secret".into(),
            },
            ..Default::default()
        };
        settings.validate().unwrap();
        let mut request = PullRequest {
            image: "ghcr.io/example/image:latest".into(),
            platform: Platform::parse("linux/amd64").unwrap(),
            format: ArchiveFormat::Docker,
            compression: Compression::Zstd,
            output_dir: "images".into(),
            file_name: None,
            cache_dir: "cache".into(),
            manifest_dir: None,
            credentials: Credentials::default(),
            proxy: None,
            allow_http: false,
            concurrency: 3,
            chunk_size_mib: 8,
            compression_level: 3,
        };
        settings.apply(&mut request);
        assert_eq!(request.proxy, settings.proxy);
        assert!(request.credentials.password.is_empty());
        request.image = "alpine:latest".into();
        settings.apply(&mut request);
        assert_eq!(request.credentials.password, "secret");
    }
}
