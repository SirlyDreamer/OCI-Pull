use std::{path::PathBuf, sync::Arc};

use anyhow::{Result, bail, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum ArchiveFormat {
    Docker,
    Oci,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum Compression {
    Zstd,
    Gzip,
    #[value(alias = "bz2")]
    Bzip2,
    Xz,
    None,
}

impl Compression {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Zstd => "tar.zst",
            Self::Gzip => "tar.gz",
            Self::Bzip2 => "tar.bz2",
            Self::Xz => "tar.xz",
            Self::None => "tar",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl Platform {
    pub fn parse(text: &str) -> Result<Self> {
        let fields: Vec<_> = text.trim().split('/').collect();
        ensure!(
            (2..=3).contains(&fields.len()),
            "平台格式应为 linux/amd64 或 linux/arm/v7"
        );
        ensure!(
            fields.iter().all(|s| !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')),
            "平台名称含非法字符"
        );
        Ok(Self {
            os: fields[0].into(),
            architecture: fields[1].into(),
            variant: fields.get(2).map(|s| (*s).into()),
        })
    }
    pub fn label(&self) -> String {
        let base = format!("{}/{}", self.os, self.architecture);
        self.variant
            .as_ref()
            .map_or(base.clone(), |v| format!("{base}/{v}"))
    }
    pub fn matches(&self, candidate: &Self) -> bool {
        self.os == candidate.os
            && self.architecture == candidate.architecture
            && self
                .variant
                .as_ref()
                .is_none_or(|v| candidate.variant.as_ref() == Some(v))
    }
}

// Credentials are intentionally neither Debug nor Serialize.
#[derive(Clone, Default)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

pub const MAX_DOWNLOAD_CONCURRENCY: usize = 128;
pub const CHUNK_SIZES_MIB: [u64; 6] = [8, 16, 32, 64, 128, 256];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DownloadSettings {
    pub concurrency: usize,
    pub chunk_size_mib: u64,
}
impl Default for DownloadSettings {
    fn default() -> Self {
        Self {
            concurrency: 16,
            chunk_size_mib: 64,
        }
    }
}
impl DownloadSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=MAX_DOWNLOAD_CONCURRENCY).contains(&self.concurrency),
            "下载并发数必须为 1–128"
        );
        ensure!(
            (1..=256).contains(&self.chunk_size_mib),
            "分块大小必须为 1–256 MiB"
        );
        Ok(())
    }
    pub fn apply(&self, request: &mut PullRequest) {
        request.concurrency = self.concurrency;
        request.chunk_size_mib = self.chunk_size_mib;
    }
}

#[derive(Clone)]
pub struct PullRequest {
    pub image: String,
    pub platform: Platform,
    pub format: ArchiveFormat,
    pub compression: Compression,
    pub output_dir: PathBuf,
    pub file_name: Option<String>,
    pub cache_dir: PathBuf,
    pub manifest_dir: Option<PathBuf>,
    pub credentials: Credentials,
    pub proxy: Option<String>,
    pub allow_http: bool,
    pub concurrency: usize,
    pub chunk_size_mib: u64,
    pub compression_level: i32,
}

impl PullRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.image.trim().is_empty(), "请输入镜像地址");
        DownloadSettings {
            concurrency: self.concurrency,
            chunk_size_mib: self.chunk_size_mib,
        }
        .validate()?;
        ensure!(
            (1..=19).contains(&self.compression_level),
            "zstd 压缩级别必须为 1–19"
        );
        ensure!(!self.output_dir.as_os_str().is_empty(), "请选择保存目录");
        if let Some(name) = &self.file_name {
            validate_file_name(name)?;
        }
        if let Some(proxy) = &self.proxy {
            ensure!(
                proxy.starts_with("http://") || proxy.starts_with("https://"),
                "代理地址应以 http:// 或 https:// 开头"
            );
        }
        ensure!(
            self.credentials.username.is_empty() == self.credentials.password.is_empty(),
            "用户名和密码／令牌必须同时填写"
        );
        Ok(())
    }
    pub fn output_path(&self, reference: &str) -> PathBuf {
        if let Some(name) = &self.file_name {
            return self.output_dir.join(name);
        }
        let safe: String = reference
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .take(150)
            .collect();
        let format = match self.format {
            ArchiveFormat::Docker => "docker",
            ArchiveFormat::Oci => "oci",
        };
        self.output_dir.join(format!(
            "image_{safe}_{}_{format}.{}",
            self.platform.label().replace('/', "-"),
            self.compression.extension()
        ))
    }
}

pub fn validate_file_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty() && name != "." && name != "..",
        "请输入文件名"
    );
    ensure!(
        !name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)),
        "文件名不能包含路径或非法字符"
    );
    ensure!(!name.ends_with(['.', ' ']), "文件名不能以点或空格结尾");
    ensure!(name.encode_utf16().count() <= 255, "文件名过长");
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|n| {
                matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
    ensure!(!device, "此文件名由系统保留");
    Ok(())
}

pub fn archive_file_name(name: &str, compression: Compression) -> Result<String> {
    let name = name.trim();
    validate_file_name(name)?;
    let lower = name.to_ascii_lowercase();
    let stem = [".tar.zst", ".tar.gz", ".tar.bz2", ".tar.xz", ".tar"]
        .iter()
        .find(|suffix| lower.ends_with(**suffix))
        .map_or(name, |suffix| &name[..name.len() - suffix.len()]);
    validate_file_name(stem)?;
    let result = format!("{stem}.{}", compression.extension());
    validate_file_name(&result)?;
    Ok(result)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    pub media_type: String,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Debug, Clone)]
pub struct ImagePlan {
    pub reference: String,
    pub pinned_reference: String,
    pub digest: String,
    pub platform: Platform,
    pub available_platforms: Vec<String>,
    pub manifest: Manifest,
    pub manifest_json: Vec<u8>,
    pub config: Vec<u8>,
    pub diff_ids: Vec<String>,
}

impl ImagePlan {
    pub fn total_bytes(&self) -> u64 {
        self.manifest.layers.iter().map(|l| l.size).sum()
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Phase(String),
    LayerList(Vec<Descriptor>),
    NetworkBytes(u64),
    Resolved {
        reference: String,
        digest: String,
        platform: String,
        layers: usize,
        bytes: u64,
    },
    Layer {
        digest: String,
        downloaded: u64,
        total: u64,
        cached: bool,
    },
    Packaging {
        completed: usize,
        total: usize,
    },
    Complete {
        path: PathBuf,
        bytes: u64,
    },
}

pub type Reporter = Arc<dyn Fn(Event) + Send + Sync>;

pub fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        bail!("任务已取消，镜像层与下载断点保留在缓存中");
    }
    Ok(())
}

pub fn digest_hex(digest: &str) -> Result<&str> {
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("目前仅支持 SHA-256 摘要"))?;
    ensure!(
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "非法的 SHA-256 摘要"
    );
    Ok(hex)
}

pub fn default_cache_dir() -> PathBuf {
    cache_root()
}
pub fn data_root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(crate::paths::APP_DIRECTORY)
}
pub fn cache_root() -> PathBuf {
    data_root().join("cache")
}

pub fn human_bytes(bytes: u64) -> String {
    let mut n = bytes as f64;
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB", "TiB"] {
        if n < 1024.0 {
            break;
        }
        n /= 1024.0;
        unit = next;
    }
    format!("{n:.1} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn platforms_do_not_silently_cross_architectures() {
        assert!(
            Platform::parse("linux/arm64")
                .unwrap()
                .matches(&Platform::parse("linux/arm64/v8").unwrap())
        );
        assert!(
            !Platform::parse("linux/arm/v7")
                .unwrap()
                .matches(&Platform::parse("linux/arm/v6").unwrap())
        );
        assert!(
            !Platform::parse("linux/amd64")
                .unwrap()
                .matches(&Platform::parse("windows/amd64").unwrap())
        );
        assert!(Platform::parse("../amd64").is_err());
    }
    #[test]
    fn digest_cannot_be_a_path() {
        assert!(digest_hex("sha256:../../secret").is_err());
        assert!(digest_hex(&format!("sha256:{}", "a".repeat(64))).is_ok());
    }
    #[test]
    fn custom_names_follow_compression_and_cannot_escape_the_output_directory() {
        for compression in [
            Compression::None,
            Compression::Gzip,
            Compression::Bzip2,
            Compression::Xz,
            Compression::Zstd,
        ] {
            for name in [
                "离线镜像",
                "离线镜像.tar",
                "离线镜像.TAR.GZ",
                "离线镜像.tar.xz",
                "离线镜像.tar.bz2",
                "离线镜像.tar.zst",
            ] {
                assert_eq!(
                    archive_file_name(name, compression).unwrap(),
                    format!("离线镜像.{}", compression.extension())
                );
            }
        }
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "C:\\escape",
            "a/b",
            "a:b",
            "NUL.tar",
            "COM1",
            "LPT¹.tar.xz",
            "CON .tar",
            "name.",
            "a\n",
        ] {
            assert!(validate_file_name(name).is_err(), "accepted {name:?}");
        }
        assert!(archive_file_name(".tar.xz", Compression::Xz).is_err());
        assert!(archive_file_name(&"a".repeat(251), Compression::Xz).is_err());
        assert_eq!(
            archive_file_name("release.v2", Compression::Xz).unwrap(),
            "release.v2.tar.xz"
        );
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    #[test]
    fn default_download_limits_and_all_ui_chunk_choices_are_valid() {
        assert_eq!(
            DownloadSettings::default(),
            DownloadSettings {
                concurrency: 16,
                chunk_size_mib: 64
            }
        );
        for chunk_size_mib in CHUNK_SIZES_MIB {
            DownloadSettings {
                concurrency: 128,
                chunk_size_mib,
            }
            .validate()
            .unwrap();
        }
        for concurrency in [0, 129, usize::MAX] {
            assert!(
                DownloadSettings {
                    concurrency,
                    chunk_size_mib: 64
                }
                .validate()
                .is_err()
            );
        }
    }
}
