use oci_pull::model::*;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, time::Instant};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Queued,
    Downloading,
    Packaging,
    Pausing,
    Paused,
    Complete,
    Failed,
}
impl State {
    pub fn active(self) -> bool {
        matches!(self, Self::Downloading | Self::Packaging | Self::Pausing)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "等待中",
            Self::Downloading => "下载中",
            Self::Packaging => "打包中",
            Self::Pausing => "暂停中",
            Self::Paused => "已暂停",
            Self::Complete => "已完成",
            Self::Failed => "失败",
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: u64,
    pub image: String,
    pub platform: String,
    pub format: ArchiveFormat,
    pub compression: Compression,
    pub directory: PathBuf,
    #[serde(default)]
    pub file_name: Option<String>,
    pub digest: String,
    pub state: State,
    pub total: u64,
    pub layers: usize,
    #[serde(default)]
    pub layer_progress: Vec<LayerProgress>,
    pub output: Option<PathBuf>,
    pub output_bytes: u64,
    pub error: Option<String>,
    #[serde(default)]
    pub options: ResumeOptions,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LayerProgress {
    pub digest: String,
    pub total: u64,
    pub downloaded: u64,
    pub started: bool,
    pub cached: bool,
}
impl LayerProgress {
    pub fn complete(&self) -> bool {
        self.started && self.downloaded >= self.total
    }
    pub fn ratio(&self) -> f32 {
        if self.complete() {
            1.
        } else {
            (self.downloaded as f32 / self.total.max(1) as f32).clamp(0., 1.)
        }
    }
    pub fn label(&self, state: State) -> &'static str {
        if self.complete() {
            if self.cached {
                "缓存命中"
            } else {
                "已下载"
            }
        } else {
            match state {
                State::Paused => "已暂停",
                State::Pausing => "暂停中",
                State::Failed if self.started => "未完成",
                State::Downloading if self.started => "下载中",
                _ => "等待中",
            }
        }
    }
}

// Store reproducible task options, but never credentials or proxy URLs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResumeOptions {
    pub cache_root: PathBuf,
    pub allow_http: bool,
    pub concurrency: usize,
    pub chunk_size_mib: u64,
    pub compression_level: i32,
    pub requires_credentials: bool,
    pub requires_proxy: bool,
}
impl Default for ResumeOptions {
    fn default() -> Self {
        Self {
            cache_root: cache_root(),
            allow_http: false,
            concurrency: 16,
            chunk_size_mib: 64,
            compression_level: 3,
            requires_credentials: false,
            requires_proxy: false,
        }
    }
}
impl From<&PullRequest> for ResumeOptions {
    fn from(request: &PullRequest) -> Self {
        Self {
            cache_root: request
                .cache_dir
                .ancestors()
                .nth(3)
                .unwrap_or(&request.cache_dir)
                .to_path_buf(),
            allow_http: request.allow_http,
            concurrency: request.concurrency,
            chunk_size_mib: request.chunk_size_mib,
            compression_level: request.compression_level,
            requires_credentials: !request.credentials.username.is_empty(),
            requires_proxy: request.proxy.is_some(),
        }
    }
}
impl Record {
    pub fn cache_path(&self) -> PathBuf {
        oci_pull::cache::CacheManager::new(self.options.cache_root.clone()).task_path(self.id)
    }
    fn request(&self) -> anyhow::Result<PullRequest> {
        digest_hex(&self.digest)?;
        let request = PullRequest {
            image: self.image.clone(),
            platform: Platform::parse(&self.platform)?,
            format: self.format,
            compression: self.compression,
            output_dir: self.directory.clone(),
            file_name: self.file_name.clone(),
            cache_dir: self.cache_path().join("blobs"),
            manifest_dir: Some(self.cache_path().join("manifests")),
            credentials: Credentials::default(),
            proxy: None,
            allow_http: self.options.allow_http,
            concurrency: self.options.concurrency,
            chunk_size_mib: self.options.chunk_size_mib,
            compression_level: self.options.compression_level,
        };
        request.validate()?;
        Ok(request)
    }
}
pub struct DownloadTask {
    pub record: Record,
    pub request: Option<PullRequest>,
    pub plan: Option<ImagePlan>,
    pub cancel: CancellationToken,
    pub rows: HashMap<String, (u64, u64, bool)>,
    pub started: Option<Instant>,
    pub packing: Option<(usize, usize)>,
    pub phase: String,
    pub removing: bool,
    network_bytes: u64,
}
impl DownloadTask {
    pub fn new(id: u64, mut request: PullRequest, plan: ImagePlan, directory: PathBuf) -> Self {
        request.cache_dir = directory.join("blobs");
        request.manifest_dir = Some(directory.join("manifests"));
        let unique: HashMap<_, _> = plan
            .manifest
            .layers
            .iter()
            .map(|l| (&l.digest, l.size))
            .collect();
        let record = Record {
            id,
            image: plan.reference.clone(),
            platform: plan.platform.label(),
            format: request.format,
            compression: request.compression,
            directory: request.output_dir.clone(),
            file_name: request.file_name.clone(),
            digest: plan.digest.clone(),
            state: State::Queued,
            total: unique.values().sum(),
            layers: unique.len(),
            layer_progress: Vec::new(),
            output: None,
            output_bytes: 0,
            error: None,
            options: ResumeOptions::from(&request),
        };
        let mut task = Self {
            record,
            request: Some(request),
            plan: Some(plan),
            cancel: CancellationToken::new(),
            rows: HashMap::new(),
            started: None,
            packing: None,
            phase: String::new(),
            removing: false,
            network_bytes: 0,
        };
        let layers = task.plan.as_ref().unwrap().manifest.layers.clone();
        task.set_layers(&layers);
        task
    }
    pub fn restore(mut record: Record) -> Self {
        if record.state.active() || record.state == State::Queued {
            record.state = State::Paused;
        }
        let request = record.request().ok();
        let rows = record
            .layer_progress
            .iter()
            .filter(|l| l.started)
            .map(|l| {
                (
                    l.digest.clone(),
                    (l.downloaded.min(l.total), l.total, l.cached),
                )
            })
            .collect();
        Self {
            record,
            request,
            plan: None,
            cancel: CancellationToken::new(),
            rows,
            started: None,
            packing: None,
            phase: String::new(),
            removing: false,
            network_bytes: 0,
        }
    }
    pub fn prepare_resume(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self.record.state, State::Paused | State::Failed),
            "当前任务不能继续"
        );
        if self.request.is_none() {
            self.request = Some(self.record.request()?);
        }
        self.request.as_ref().unwrap().validate()?;
        digest_hex(&self.record.digest)?;
        Ok(())
    }
    pub fn set_layers(&mut self, descriptors: &[Descriptor]) {
        let mut seen = std::collections::HashSet::new();
        let previous: HashMap<_, _> = std::mem::take(&mut self.record.layer_progress)
            .into_iter()
            .map(|layer| (layer.digest.clone(), layer))
            .collect();
        self.record.layer_progress = descriptors
            .iter()
            .filter(|d| seen.insert(d.digest.clone()))
            .map(|d| {
                let mut layer = previous.get(&d.digest).cloned().unwrap_or(LayerProgress {
                    digest: d.digest.clone(),
                    total: d.size,
                    downloaded: 0,
                    started: false,
                    cached: false,
                });
                layer.total = d.size;
                layer.downloaded = layer.downloaded.min(d.size);
                if self.record.state == State::Complete {
                    layer.downloaded = d.size;
                    layer.started = true;
                }
                layer
            })
            .collect();
    }
    pub fn reset_progress(&mut self) {
        self.network_bytes = 0;
        self.rows.clear();
        self.packing = None;
        self.phase.clear();
        for layer in &mut self.record.layer_progress {
            layer.downloaded = 0;
            layer.started = false;
            layer.cached = false;
        }
    }
    /// Running workers must stop before the record can be removed safely.
    pub fn prepare_remove(&mut self) -> bool {
        self.removing = true;
        if self.record.state.active() {
            self.record.state = State::Pausing;
            self.cancel.cancel();
            false
        } else {
            true
        }
    }
    pub fn needs_connection(&self) -> bool {
        self.request.as_ref().is_some_and(|r| {
            (self.record.options.requires_credentials && r.credentials.password.is_empty())
                || (self.record.options.requires_proxy && r.proxy.is_none())
        })
    }
    pub fn downloaded(&self) -> u64 {
        self.rows.values().map(|r| r.0).sum()
    }
    pub fn ratio(&self) -> f32 {
        if self.record.state == State::Complete {
            return 1.;
        }
        if let Some((done, total)) = self.packing {
            return done as f32 / total.max(1) as f32;
        }
        (self.downloaded() as f32 / self.record.total.max(1) as f32).clamp(0., 1.)
    }
    pub fn speed(&self) -> u64 {
        if self.record.state != State::Downloading {
            return 0;
        }
        let elapsed = self
            .started
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.);
        if elapsed < 0.3 {
            0
        } else {
            (self.network_bytes as f64 / elapsed) as u64
        }
    }
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Phase(phase) => self.phase = phase,
            Event::LayerList(layers) => self.set_layers(&layers),
            Event::NetworkBytes(bytes) => {
                self.network_bytes = self.network_bytes.saturating_add(bytes)
            }
            Event::Resolved { layers, bytes, .. } => {
                self.record.layers = layers;
                self.record.total = bytes;
            }
            Event::Layer {
                digest,
                downloaded,
                total,
                cached,
            } => {
                if let Some(layer) = self
                    .record
                    .layer_progress
                    .iter_mut()
                    .find(|l| l.digest == digest)
                {
                    layer.downloaded = downloaded.min(total);
                    layer.total = total;
                    layer.started = true;
                    layer.cached = cached;
                } else {
                    self.record.layer_progress.push(LayerProgress {
                        digest: digest.clone(),
                        total,
                        downloaded: downloaded.min(total),
                        started: true,
                        cached,
                    });
                }
                self.rows.insert(digest, (downloaded, total, cached));
            }
            Event::Packaging { completed, total } => {
                if self.record.state != State::Pausing {
                    self.record.state = State::Packaging;
                }
                self.packing = Some((completed, total));
            }
            Event::Complete { path, bytes } => {
                for layer in &mut self.record.layer_progress {
                    layer.downloaded = layer.total;
                    layer.started = true;
                }
                self.record.output = Some(path);
                self.record.output_bytes = bytes;
            }
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub cache_dir: PathBuf,
    pub used_cache_roots: Vec<PathBuf>,
    pub image: String,
    pub platform: String,
    pub output: PathBuf,
    pub format: ArchiveFormat,
    pub compression: Compression,
    #[serde(default)]
    pub download: DownloadSettings,
}
impl Default for Defaults {
    fn default() -> Self {
        Self {
            cache_dir: cache_root(),
            used_cache_roots: Vec::new(),
            image: String::new(),
            platform: "linux/amd64".into(),
            output: dirs::download_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join(oci_pull::paths::APP_DIRECTORY),
            format: ArchiveFormat::Docker,
            compression: Compression::Zstd,
            download: DownloadSettings::default(),
        }
    }
}
impl From<&Record> for Defaults {
    fn from(r: &Record) -> Self {
        Self {
            cache_dir: r.options.cache_root.clone(),
            used_cache_roots: Vec::new(),
            image: r.image.clone(),
            platform: r.platform.clone(),
            output: r.directory.clone(),
            format: r.format,
            compression: r.compression,
            download: DownloadSettings {
                concurrency: r.options.concurrency,
                chunk_size_mib: r.options.chunk_size_mib,
            },
        }
    }
}
pub fn config_dir() -> PathBuf {
    std::env::temp_dir().join(oci_pull::paths::APP_DIRECTORY)
}
pub fn save_json<T: Serialize>(name: &str, value: &T) -> anyhow::Result<()> {
    use std::io::Write;
    let directory = config_dir();
    std::fs::create_dir_all(&directory)?;
    let mut temp = tempfile::NamedTempFile::new_in(&directory)?;
    temp.write_all(&serde_json::to_vec_pretty(value)?)?;
    temp.as_file().sync_all()?;
    temp.persist(directory.join(name)).map_err(|e| e.error)?;
    Ok(())
}
pub fn format_label(format: ArchiveFormat) -> &'static str {
    match format {
        ArchiveFormat::Docker => "Docker",
        ArchiveFormat::Oci => "OCI",
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn sample_record() -> Record {
        Record {
            id: 1,
            image: "alpine:latest".into(),
            platform: "linux/amd64".into(),
            format: ArchiveFormat::Docker,
            compression: Compression::Zstd,
            directory: "images".into(),
            file_name: None,
            digest: format!("sha256:{}", "ab".repeat(32)),
            state: State::Downloading,
            total: 100,
            layers: 1,
            layer_progress: Vec::new(),
            output: None,
            output_bytes: 0,
            error: None,
            options: ResumeOptions::default(),
        }
    }
    #[test]
    fn interrupted_history_restores_request_without_a_wizard_or_secrets() {
        let mut record = sample_record();
        record.file_name = Some("离线镜像.tar.xz".into());
        record.compression = Compression::Xz;
        record.options.allow_http = true;
        record.options.concurrency = 5;
        record.options.chunk_size_mib = 16;
        let mut task = DownloadTask::restore(record);
        assert!(task.record.state == State::Paused);
        task.prepare_resume().unwrap();
        let request = task.request.as_ref().unwrap();
        assert_eq!(request.image, "alpine:latest");
        assert!(request.allow_http);
        assert_eq!(request.concurrency, 5);
        assert_eq!(request.chunk_size_mib, 16);
        assert_eq!(
            request.output_path(&request.image),
            task.record.directory.join("离线镜像.tar.xz")
        );
        assert!(!task.needs_connection());
        assert!(request.credentials.password.is_empty());
        let json = serde_json::to_value(&task.record).unwrap();
        assert!(json.get("credentials").is_none());
        assert!(json.get("password").is_none());
    }
    #[test]
    fn paused_and_failed_tasks_keep_their_request_and_session_credentials() {
        let mut task = DownloadTask::restore(sample_record());
        let request = task.request.as_mut().unwrap();
        request.credentials = Credentials {
            username: "user".into(),
            password: "session-secret".into(),
        };
        request.proxy = Some("http://user:proxy-secret@localhost:1234".into());
        task.record.options = ResumeOptions::from(&*request);
        for state in [State::Paused, State::Failed] {
            task.record.state = state;
            task.prepare_resume().unwrap();
            assert!(!task.needs_connection());
            assert_eq!(
                task.request.as_ref().unwrap().credentials.password,
                "session-secret"
            );
        }
        let json = serde_json::to_string(&task.record).unwrap();
        assert!(!json.contains("session-secret"));
        assert!(!json.contains("proxy-secret"));
        let restored = DownloadTask::restore(serde_json::from_str(&json).unwrap());
        assert!(restored.needs_connection());
    }
    #[test]
    fn records_receive_defaults_and_invalid_digests_cannot_resume() {
        let mut json = serde_json::to_value(sample_record()).unwrap();
        json.as_object_mut().unwrap().remove("options");
        let mut task = DownloadTask::restore(serde_json::from_value(json.clone()).unwrap());
        task.prepare_resume().unwrap();
        assert_eq!(task.request.as_ref().unwrap().concurrency, 16);
        assert_eq!(task.request.as_ref().unwrap().chunk_size_mib, 64);
        json["digest"] = serde_json::json!("../../invalid");
        let mut task = DownloadTask::restore(serde_json::from_value(json).unwrap());
        assert!(task.prepare_resume().is_err());
    }
    #[test]
    fn removing_an_active_task_waits_for_cancellation_before_deleting_its_record() {
        for state in [State::Downloading, State::Packaging, State::Pausing] {
            let mut task = DownloadTask::restore(sample_record());
            task.record.state = state;
            assert!(!task.prepare_remove());
            assert!(task.removing);
            assert!(task.cancel.is_cancelled());
            assert!(task.record.state == State::Pausing);
            task.record.state = State::Paused;
            assert!(task.prepare_remove());
        }
        let mut task = DownloadTask::restore(sample_record());
        task.record.state = State::Queued;
        assert!(task.prepare_remove());
        assert!(task.removing);
    }
    #[test]
    fn layer_details_keep_manifest_order_and_track_cache_retry_and_pause() {
        let mut task = DownloadTask::restore(sample_record());
        task.record.state = State::Downloading;
        let descriptor = |letter: &str, size| Descriptor {
            digest: format!("sha256:{}", letter.repeat(64)),
            size,
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
            platform: None,
            urls: vec![],
        };
        let a = descriptor("a", 100);
        let b = descriptor("b", 200);
        let c = descriptor("c", 0);
        task.apply(Event::LayerList(vec![
            b.clone(),
            a.clone(),
            b.clone(),
            c.clone(),
        ]));
        assert_eq!(task.record.layer_progress.len(), 3);
        assert_eq!(task.record.layer_progress[0].digest, b.digest);
        assert_eq!(task.record.layer_progress[2].ratio(), 0.);
        assert_eq!(
            task.record.layer_progress[1].label(task.record.state),
            "等待中"
        );
        task.apply(Event::Layer {
            digest: a.digest.clone(),
            downloaded: 35,
            total: 100,
            cached: false,
        });
        task.apply(Event::Layer {
            digest: b.digest.clone(),
            downloaded: 200,
            total: 200,
            cached: true,
        });
        assert_eq!(
            task.record.layer_progress[0].label(task.record.state),
            "缓存命中"
        );
        assert_eq!(
            task.record.layer_progress[1].label(task.record.state),
            "下载中"
        );
        task.record.state = State::Paused;
        assert_eq!(
            task.record.layer_progress[1].label(task.record.state),
            "已暂停"
        );
        let json = serde_json::to_vec(&task.record).unwrap();
        let mut restored = DownloadTask::restore(serde_json::from_slice(&json).unwrap());
        assert_eq!(restored.record.layer_progress[1].downloaded, 35);
        assert_eq!(restored.downloaded(), 235);
        restored.reset_progress();
        assert_eq!(restored.downloaded(), 0);
        assert!(
            restored
                .record
                .layer_progress
                .iter()
                .all(|l| !l.started && l.downloaded == 0 && !l.cached)
        );
        restored.apply(Event::Layer {
            digest: a.digest,
            downloaded: 10,
            total: 100,
            cached: false,
        });
        assert_eq!(restored.record.layer_progress[1].downloaded, 10);
        restored.apply(Event::Layer {
            digest: c.digest,
            downloaded: 0,
            total: 0,
            cached: true,
        });
        assert_eq!(restored.record.layer_progress[2].ratio(), 1.);
        restored.apply(Event::Complete {
            path: "images/completed.tar".into(),
            bytes: 100,
        });
        assert!(
            restored
                .record
                .layer_progress
                .iter()
                .all(LayerProgress::complete)
        );
    }
}
