use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use oci_pull::{model::*, registry};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "oci-pull-cli",
    version,
    about = "Pull container images without Docker or external tools"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Inspect(Options),
    Pull(Options),
}

#[derive(Args)]
struct Options {
    image: String,
    #[arg(long, default_value = "linux/amd64")]
    platform: String,
    #[arg(long, value_enum, default_value = "docker")]
    format: ArchiveFormat,
    #[arg(long, value_enum, default_value = "zstd")]
    compression: Compression,
    #[arg(short, long, default_value = ".")]
    output: PathBuf,
    #[arg(long)]
    filename: Option<String>,
    #[arg(long)]
    cache: Option<PathBuf>,
    #[arg(long)]
    proxy: Option<String>,
    #[arg(long, default_value = "")]
    username: String,
    /// Environment variable containing the registry password or token (never logged).
    #[arg(long, default_value = "OCI_PULL_PASSWORD")]
    password_env: String,
    /// Use plaintext HTTP for this registry, e.g. an explicitly selected local registry.
    #[arg(long)]
    http: bool,
    /// Total concurrent piece downloads for this image task, across all layers (1-128).
    #[arg(long, default_value_t = 16)]
    concurrency: usize,
    /// Piece size in MiB (1-256); existing partial pieces retain their saved layout.
    #[arg(long, default_value_t = 64)]
    chunk_size_mib: u64,
    #[arg(long, default_value_t = 3)]
    zstd_level: i32,
}

impl Options {
    fn request(self) -> Result<PullRequest> {
        let password = if self.username.is_empty() {
            String::new()
        } else {
            std::env::var(&self.password_env).context("指定的密码环境变量未设置")?
        };
        Ok(PullRequest {
            image: self.image,
            platform: Platform::parse(&self.platform)?,
            format: self.format,
            compression: self.compression,
            output_dir: self.output,
            file_name: self
                .filename
                .map(|name| archive_file_name(&name, self.compression))
                .transpose()?,
            cache_dir: self.cache.unwrap_or_else(configured_cache_dir),
            manifest_dir: None,
            credentials: Credentials {
                username: self.username,
                password,
            },
            proxy: self.proxy,
            allow_http: self.http,
            concurrency: self.concurrency,
            chunk_size_mib: self.chunk_size_mib,
            compression_level: self.zstd_level,
        })
    }
}

fn configured_cache_dir() -> PathBuf {
    std::fs::read(
        std::env::temp_dir()
            .join(oci_pull::paths::APP_DIRECTORY)
            .join("preferences.json"),
    )
    .ok()
    .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok())
    .and_then(|value| {
        value
            .get("cache_dir")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
    })
    .unwrap_or_else(default_cache_dir)
}
fn managed_request(
    options: Options,
    preview: bool,
) -> Result<(PullRequest, Arc<oci_pull::cache::CacheLease>)> {
    use sha2::{Digest, Sha256};
    let mut request = options.request()?;
    let manager = oci_pull::cache::CacheManager::new(std::path::absolute(&request.cache_dir)?);
    let lease = if preview {
        manager.preview()?
    } else {
        let key = oci_pull::paths::directory_key(&Sha256::digest(format!(
            "{}|{}",
            registry::parse_reference(&request.image)?,
            request.platform.label()
        )));
        manager.cli(&key)?
    };
    lease.bind(&mut request)?;
    Ok((request, lease))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let last = Arc::new(Mutex::new(Instant::now()));
    let report: Reporter = Arc::new(move |event| match event {
        Event::Phase(message) => eprintln!("{message}"),
        Event::LayerList(_) => {}
        Event::NetworkBytes(_) => {}
        Event::Resolved {
            reference,
            digest,
            platform,
            layers,
            bytes,
        } => eprintln!(
            "{reference}\n{digest}\n{platform} · {layers} layers · {}",
            human_bytes(bytes)
        ),
        Event::Layer {
            digest,
            downloaded,
            total,
            cached,
        } => {
            if let Ok(mut last) = last.lock()
                && (downloaded == total || last.elapsed().as_secs() >= 1)
            {
                eprintln!(
                    "{} {} / {}{}",
                    &digest[..19.min(digest.len())],
                    human_bytes(downloaded),
                    human_bytes(total),
                    if cached { " (cached)" } else { "" }
                );
                *last = Instant::now();
            }
        }
        Event::Packaging { completed, total } => eprintln!("Archive {completed}/{total}"),
        Event::Complete { path, bytes } => {
            eprintln!("Saved {} ({})", path.display(), human_bytes(bytes))
        }
    });
    match cli.command {
        Command::Inspect(options) => {
            let (request, _lease) = managed_request(options, true)?;
            let plan = registry::inspect(&request, &report, &cancel).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"reference": plan.reference, "digest": plan.digest, "platform": plan.platform.label(), "platforms": plan.available_platforms, "layers": plan.manifest.layers.len(), "downloadBytes": plan.total_bytes()})
                )?
            );
        }
        Command::Pull(options) => {
            let (request, lease) = managed_request(options, false)?;
            let path = registry::pull(request, report, cancel).await?;
            if let Err(error) = lease.clear() {
                eprintln!("导出成功，缓存清理失败：{error:#}");
            }
            println!("{}", path.display());
        }
    }
    Ok(())
}
