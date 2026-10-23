use crate::model::*;
use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream};
use oci_client::{Reference, client::Client, manifest::OciDescriptor};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions, TryLockError},
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

mod manager;
mod pieces;
mod scheduler;
mod transport;
pub use manager::DownloadManager;
pub(crate) use scheduler::Scheduler;

#[derive(Debug)]
struct Restart(&'static str);
impl std::fmt::Display for Restart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Restart {}
#[derive(Debug)]
struct NoRange;
impl std::fmt::Display for NoRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("仓库不支持有效的范围下载，改用普通下载")
    }
}
impl std::error::Error for NoRange {}

fn segment_path(directory: &Path, start: u64, end: u64) -> PathBuf {
    directory.join(format!("{start}-{end}.part"))
}

// The lock is held across validation, download, assembly and publication. OS
// locks release after crashes; the empty lock file is safe to reuse next launch.
async fn blob_lock(path: &Path, cancel: &CancellationToken) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    loop {
        check_cancel(cancel)?;
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => tokio::select! {
                _ = cancel.cancelled() => check_cancel(cancel)?,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            },
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

pub(crate) async fn verified(
    path: &Path,
    desc: &Descriptor,
    cancel: &CancellationToken,
) -> Result<bool> {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if file.metadata().await?.len() != desc.size {
        return Ok(false);
    }
    Ok(hash_file(&mut file, cancel).await? == desc.digest)
}
async fn hash_file(file: &mut tokio::fs::File, cancel: &CancellationToken) -> Result<String> {
    file.seek(SeekFrom::Start(0)).await?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        check_cancel(cancel)?;
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("sha256:{}", hex::encode(hash.finalize())))
}
async fn remove_file(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
struct Progress {
    values: Vec<u64>,
    last: Instant,
}
struct Transfer<'a> {
    client: &'a Client,
    image: &'a Reference,
    desc: &'a Descriptor,
    descriptor: OciDescriptor,
    report: &'a Reporter,
    show: bool,
    scheduler: &'a Arc<Scheduler>,
}
impl Transfer<'_> {
    fn progress(&self, downloaded: u64, cached: bool) {
        if self.show {
            (self.report)(Event::Layer {
                digest: self.desc.digest.clone(),
                downloaded,
                total: self.desc.size,
                cached,
            });
        }
    }
    fn update(&self, progress: &Mutex<Progress>, index: usize, bytes: u64, force: bool) {
        let mut progress = progress.lock().unwrap();
        progress.values[index] = bytes;
        if force || progress.last.elapsed() >= Duration::from_millis(120) {
            self.progress(progress.values.iter().sum(), false);
            progress.last = Instant::now();
        }
    }
    async fn sequential(
        &self,
        partial: &Path,
        ranges: bool,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if self.desc.size == 0 {
            tokio::fs::write(partial, []).await?;
            return Ok(());
        }
        let _slot = self.scheduler.acquire(&self.desc.digest, cancel).await?;
        let progress = Mutex::new(Progress {
            values: vec![0],
            last: Instant::now(),
        });
        self.fill(
            partial,
            0,
            self.desc.size - 1,
            false,
            ranges,
            0,
            &progress,
            cancel,
        )
        .await
    }
    async fn parallel(
        &self,
        directory: &Path,
        partial: &Path,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let (state, values) = pieces::Pieces::load(directory, self.desc.size).await?;
        self.progress(values.iter().sum(), false);
        let progress = Mutex::new(Progress {
            values,
            last: Instant::now(),
        });
        let state = tokio::sync::Mutex::new(state);
        let group = cancel.child_token();
        // These are lightweight slot waiters, not a fixed number of network connections per
        // layer. The independent task scheduler alone grants network capacity.
        let jobs: Vec<_> = (0..crate::model::MAX_DOWNLOAD_CONCURRENCY)
            .map(|_| {
                let (state, progress, group) = (&state, &progress, &group);
                async move {
                    loop {
                        check_cancel(group)?;
                        if !state.lock().await.has_work() {
                            return Ok::<_, anyhow::Error>(());
                        }
                        let _slot = self.scheduler.acquire(&self.desc.digest, group).await?;
                        let claim = state
                            .lock()
                            .await
                            .claim(directory, self.scheduler.chunk_size(), progress)
                            .await?;
                        let Some((index, piece)) = claim else {
                            return Ok(());
                        };
                        self.fill(
                            &segment_path(directory, piece.start, piece.end),
                            piece.start,
                            piece.end,
                            true,
                            true,
                            index,
                            progress,
                            group,
                        )
                        .await?;
                    }
                }
            })
            .collect();
        let mut pending =
            stream::iter(jobs).buffer_unordered(crate::model::MAX_DOWNLOAD_CONCURRENCY);
        let mut error = None;
        while let Some(result) = pending.next().await {
            if let Err(e) = result
                && error.is_none()
            {
                error = Some(e);
                group.cancel();
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        drop(pending);
        let mut ranges = state.into_inner().entries;
        ranges.sort_by_key(|piece| piece.start);
        check_cancel(cancel)?;
        // Assembly is local I/O. Finished segments survive cancellation and can
        // be reassembled next time without another network transfer.
        let mut output = tokio::fs::File::create(partial).await?;
        let mut buffer = vec![0; 256 * 1024];
        let result: Result<()> = async {
            for piece in &ranges {
                let mut input =
                    tokio::fs::File::open(segment_path(directory, piece.start, piece.end)).await?;
                loop {
                    check_cancel(cancel)?;
                    let n = input.read(&mut buffer).await?;
                    if n == 0 {
                        break;
                    }
                    output.write_all(&buffer[..n]).await?;
                }
            }
            Ok(())
        }
        .await;
        output.flush().await?;
        output.sync_all().await?;
        result
    }
}

#[cfg(test)]
#[path = "../tests/support/download.rs"]
mod tests;

pub(crate) async fn cache_blob(
    client: &Client,
    image: &Reference,
    desc: &Descriptor,
    cache: &Path,
    report: &Reporter,
    cancel: &CancellationToken,
    show: bool,
) -> Result<PathBuf> {
    cache_blob_scheduled(
        client,
        image,
        desc,
        cache,
        report,
        cancel,
        show,
        &Scheduler::new(1, 64 * 1024 * 1024),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cache_blob_scheduled(
    client: &Client,
    image: &Reference,
    desc: &Descriptor,
    cache: &Path,
    report: &Reporter,
    cancel: &CancellationToken,
    show: bool,
    scheduler: &Arc<Scheduler>,
) -> Result<PathBuf> {
    let hex = digest_hex(&desc.digest)?;
    tokio::fs::create_dir_all(cache)
        .await
        .context("无法创建镜像层缓存")?;
    let _lock = blob_lock(&cache.join(format!("{hex}.lock")), cancel).await?;
    let destination = cache.join(hex);
    let partial = cache.join(format!("{hex}.part"));
    let parts = cache.join(crate::paths::blob_parts_directory_name(&desc.digest)?);
    let fallback = cache.join(format!("{hex}.no-range"));
    let transfer = Transfer {
        client,
        image,
        desc,
        report,
        show,
        scheduler,
        descriptor: OciDescriptor {
            digest: desc.digest.clone(),
            media_type: desc.media_type.clone(),
            size: i64::try_from(desc.size)?,
            urls: (!desc.urls.is_empty()).then(|| desc.urls.clone()),
            ..Default::default()
        },
    };
    if verified(&destination, desc, cancel).await? {
        transfer.progress(desc.size, true);
        return Ok(destination);
    }
    remove_file(&destination).await?;
    let mut ranges = !fallback.exists();
    for attempt in 0..3 {
        check_cancel(cancel)?;
        if verified(&partial, desc, cancel).await? {
            tokio::fs::rename(&partial, &destination).await?;
            pieces::clear(&parts, desc.size).await?;
            transfer.progress(desc.size, true);
            return Ok(destination);
        }
        if tokio::fs::metadata(&partial)
            .await
            .is_ok_and(|m| m.len() >= desc.size)
        {
            remove_file(&partial).await?;
        }
        // A sequential partial remains sequential; a segmented transfer always
        // resumes its per-segment files, including after interrupted assembly.
        let parallel =
            ranges && (parts.exists() || (desc.size > scheduler.chunk_size() && !partial.exists()));
        let mut result = if parallel {
            transfer.parallel(&parts, &partial, cancel).await
        } else {
            transfer.sequential(&partial, ranges, cancel).await
        };
        if result.as_ref().is_err_and(|e| e.is::<NoRange>()) {
            check_cancel(cancel)?;
            ranges = false;
            tokio::fs::write(&fallback, b"1\n").await?;
            pieces::clear(&parts, desc.size).await?;
            remove_file(&partial).await?;
            report(Event::Phase("仓库不支持范围下载，改用普通下载…".into()));
            result = transfer.sequential(&partial, false, cancel).await;
        }
        if result.is_ok() && !verified(&partial, desc, cancel).await? {
            result = Err(Restart("镜像层大小或 SHA-256 校验失败").into());
        }
        match result {
            Ok(()) => {
                check_cancel(cancel)?;
                tokio::fs::rename(&partial, &destination).await?;
                pieces::clear(&parts, desc.size).await?;
                transfer.progress(desc.size, false);
                return Ok(destination);
            }
            Err(error) => {
                check_cancel(cancel)?;
                if error.is::<Restart>()
                    || tokio::fs::metadata(&partial)
                        .await
                        .is_ok_and(|m| m.len() >= desc.size)
                {
                    remove_file(&partial).await?;
                    pieces::clear(&parts, desc.size).await?;
                }
                if attempt == 2 {
                    return Err(error)
                        .context(format!("镜像层 {} 下载失败（已尝试 3 次）", &hex[..12]));
                }
                report(Event::Phase(format!(
                    "重试镜像层 {}（{}/3）…",
                    &hex[..12],
                    attempt + 2
                )));
                tokio::select! {
                    _ = cancel.cancelled() => check_cancel(cancel)?,
                    _ = tokio::time::sleep(Duration::from_secs(1 << attempt)) => {}
                }
            }
        }
    }
    bail!("下载未完成")
}
