use super::cache_ops::{CacheStatus, ChangeMode, PendingChange};
use super::*;
use oci_pull::cache::{CacheManager, Cleanup};
use std::{collections::HashSet, path::PathBuf};

impl AppView {
    pub(super) fn cache_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.defaults.used_cache_roots.clone();
        roots.push(self.defaults.cache_dir.clone());
        roots.extend(
            self.tasks
                .iter()
                .map(|t| t.record.options.cache_root.clone()),
        );
        roots.sort();
        roots.dedup();
        roots
    }
    pub(super) fn cache_status(
        &mut self,
        busy: bool,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.cache_busy = busy;
        self.cache_status
            .update(cx, |status: &mut CacheStatus, cx| {
                status.busy = busy;
                status.message = Some(message.into());
                cx.notify();
            });
        cx.notify();
    }
    pub(super) fn collect_cache(&mut self, manual: bool, cx: &mut Context<Self>) {
        if self.cache_busy {
            return;
        }
        let roots = self.cache_roots();
        let unknown = if manual {
            HashSet::new()
        } else {
            match self.store.known_ids() {
                Ok(mut ids) => {
                    for task in &self.tasks {
                        ids.remove(&task.record.id);
                    }
                    ids
                }
                Err(error) => {
                    self.error = Some(format!("读取回收列表失败：{error:#}"));
                    return;
                }
            }
        };
        let work: Vec<_> = roots
            .into_iter()
            .map(|root| {
                let mut keep = unknown.clone();
                let mut skipped = 0;
                for task in &self.tasks {
                    if task.record.options.cache_root != root {
                        continue;
                    }
                    if task.record.state.active()
                        || task.removing
                        || (!manual && task.record.state != State::Complete)
                    {
                        keep.insert(task.record.id);
                        if manual {
                            skipped += 1;
                        }
                    }
                }
                (root, keep, skipped)
            })
            .collect();
        self.cache_status(true, "正在清理缓存…", cx);
        let sender = self.sender.clone();
        self.runtime.spawn_blocking(move || {
            let mut result = Cleanup::default();
            for (root, keep, skipped) in work {
                result.skipped += skipped;
                result.append(CacheManager::new(root).collect(&keep, manual));
            }
            let _ = sender.send(Message::CacheCleaned(result));
        });
    }
    pub(super) fn apply_settings(&mut self, settings: NetworkSettings) {
        if let Err(error) = self.downloads.configure(settings.download) {
            self.error = Some(format!("更新下载设置失败：{error:#}"));
            return;
        }
        self.network = settings.clone();
        self.network_override = true;
        self.defaults.download = settings.download;
        for task in &mut self.tasks {
            if task.record.state != State::Complete {
                if let Some(request) = &mut task.request {
                    settings.download.apply(request);
                }
                task.record.options.concurrency = settings.download.concurrency;
                task.record.options.chunk_size_mib = settings.download.chunk_size_mib;
            }
        }
        self.save();
        if let Err(error) = save_json("preferences.json", &self.defaults) {
            self.error = Some(format!("保存设置失败：{error:#}"));
        }
    }
    pub(super) fn request_settings(
        &mut self,
        settings: NetworkSettings,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.cache_busy {
            return;
        }
        let roots = self.cache_roots();
        let mut outputs = vec![self.defaults.output.clone()];
        outputs.extend(self.tasks.iter().map(|t| t.record.directory.clone()));
        let root = match cache_ops::validate_root(&root, &roots, &outputs) {
            Ok(root) => root,
            Err(error) => {
                self.cache_status(false, error.to_string(), cx);
                return;
            }
        };
        let current = oci_pull::cache::resolved_future_path(&self.defaults.cache_dir)
            .unwrap_or_else(|_| self.defaults.cache_dir.clone());
        if root == current {
            self.apply_settings(settings);
            window.close_dialog(cx);
            cx.notify();
            return;
        }
        if !self.tasks.iter().any(|t| t.record.cache_path().exists()) {
            self.begin_cache_change(settings, root, ChangeMode::Migrate, cx);
            window.close_dialog(cx);
            return;
        }
        let owner = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let migrate_owner = owner.clone(); let clear_owner = owner.clone();
            let migrate_settings = settings.clone(); let clear_settings = settings.clone();
            let migrate_root = root.clone(); let clear_root = root.clone();
            dialog.title("更换缓存目录").width(px(460.)).overlay_closable(false)
                .child(div().text_sm().child("旧目录中有任务缓存，请选择处理方式。迁移保留断点；清理后，未完成任务重新下载。运行中的任务会暂时停止，处理完成后恢复。"))
                .footer(div().flex().justify_end().gap_2()
                    .child(Button::new("cache-change-cancel").ghost().label("取消").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("cache-change-clear").label("清理").on_click(move |_, window, cx| {
                        clear_owner.update(cx, |view, cx| view.begin_cache_change(clear_settings.clone(), clear_root.clone(), ChangeMode::Clear, cx));
                        window.close_all_dialogs(cx);
                    }))
                    .child(Button::new("cache-change-migrate").primary().label("迁移").on_click(move |_, window, cx| {
                        migrate_owner.update(cx, |view, cx| view.begin_cache_change(migrate_settings.clone(), migrate_root.clone(), ChangeMode::Migrate, cx));
                        window.close_all_dialogs(cx);
                    })))
        });
    }
    pub(super) fn begin_cache_change(
        &mut self,
        settings: NetworkSettings,
        root: PathBuf,
        mode: ChangeMode,
        cx: &mut Context<Self>,
    ) {
        if self.cache_busy {
            return;
        }
        self.apply_settings(settings.clone());
        let resume = self
            .tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.record.state,
                    State::Downloading | State::Packaging | State::Queued
                ) && !t.removing
            })
            .map(|t| t.record.id)
            .collect();
        for task in &mut self.tasks {
            if task.record.state.active() {
                task.cancel.cancel();
                task.record.state = State::Pausing;
            } else if task.record.state == State::Queued {
                task.record.state = State::Paused;
            }
        }
        self.pending_cache_change = Some(PendingChange {
            settings,
            root,
            mode,
            resume,
        });
        self.cache_status(true, "正在等待任务安全停止…", cx);
        self.save();
        self.try_cache_change(cx);
    }
    pub(super) fn try_cache_change(&mut self, cx: &mut Context<Self>) {
        if self.cache_change_running
            || self.pending_cache_change.is_none()
            || self
                .tasks
                .iter()
                .any(|t| t.record.state.active() || t.removing)
        {
            return;
        }
        let pending = self.pending_cache_change.as_ref().unwrap();
        let root = pending.root.clone();
        let mode = pending.mode;
        let records = self.tasks.iter().map(|t| t.record.clone()).collect();
        let store = self.store.clone();
        let sender = self.sender.clone();
        self.cache_change_running = true;
        self.cache_status(
            true,
            if mode == ChangeMode::Migrate {
                "正在迁移并校验缓存…"
            } else {
                "正在清理旧缓存…"
            },
            cx,
        );
        self.runtime.spawn_blocking(move || {
            let result = cache_ops::change_root(&store, records, &root, mode);
            let _ = sender.send(Message::CacheChanged(result));
        });
    }
    pub(super) fn finish_cache_change(
        &mut self,
        result: cache_ops::ChangeResult,
        cx: &mut Context<Self>,
    ) {
        let pending = self.pending_cache_change.take().unwrap();
        self.cache_change_running = false;
        self.defaults.used_cache_roots = self.cache_roots();
        self.defaults.used_cache_roots.push(pending.root.clone());
        self.defaults.used_cache_roots.sort();
        self.defaults.used_cache_roots.dedup();
        for record in result.records {
            if let Some(task) = self.tasks.iter_mut().find(|t| t.record.id == record.id) {
                task.record = record;
                if let Some(request) = &mut task.request {
                    request.cache_dir = task.record.cache_path().join("blobs");
                    request.manifest_dir = Some(task.record.cache_path().join("manifests"));
                }
                if pending.mode == ChangeMode::Clear && task.record.state != State::Complete {
                    task.reset_progress();
                }
            }
        }
        let success = result.errors.is_empty();
        if success {
            self.defaults.cache_dir = pending.root;
        }
        self.defaults.download = pending.settings.download;
        if let Err(error) = save_json("preferences.json", &self.defaults) {
            self.error = Some(format!("保存设置失败：{error:#}"));
        }
        for task in &mut self.tasks {
            if pending.resume.contains(&task.record.id) && task.record.state == State::Paused {
                task.record.state = State::Queued;
            }
        }
        let message = if success {
            "缓存目录已更换".into()
        } else {
            format!("缓存目录未完全更换：{}", result.errors.join("；"))
        };
        if !success {
            self.error = Some(message.clone());
        }
        self.cache_status(false, message, cx);
        self.save();
        self.schedule();
    }
}
