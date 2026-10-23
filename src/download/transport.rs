// A single bounded HTTP transfer. Scheduling and retries belong to the caller.
use super::{NoRange, Progress, Restart, Transfer};
use crate::model::*;
use anyhow::{Result, ensure};
use futures_util::TryStreamExt;
use oci_client::{
    client::{BlobResponse, SizedStream},
    errors::OciDistributionError,
};
use std::{io::SeekFrom, path::Path, sync::Mutex};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

impl Transfer<'_> {
    async fn response(
        &self,
        offset: u64,
        end: u64,
        range: bool,
        cancel: &CancellationToken,
    ) -> Result<BlobResponse> {
        let response = tokio::select! {
            _ = cancel.cancelled() => { check_cancel(cancel)?; unreachable!() },
            response = async {
                if range {
                    self.client.pull_blob_stream_partial(self.image, &self.descriptor, offset, Some(end - offset + 1)).await
                } else {
                    self.client.pull_blob_stream(self.image, &self.descriptor).await.map(BlobResponse::Full)
                }
            } => response,
        };
        match response {
            Err(OciDistributionError::ServerError { code: 416, .. }) => Err(NoRange.into()),
            other => Ok(other?),
        }
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn fill(
        &self,
        path: &Path,
        start: u64,
        end: u64,
        parallel: bool,
        ranges: bool,
        index: usize,
        progress: &Mutex<Progress>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let length = end - start + 1;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .await?;
        let mut size = file.metadata().await?.len();
        if size > length {
            file.set_len(0).await?;
            size = 0;
        }
        self.update(progress, index, size, true);
        if size == length {
            return Ok(());
        }
        let response = self
            .response(start + size, end, ranges && (parallel || size > 0), cancel)
            .await?;
        let mut source = match response {
            BlobResponse::Partial(source) => {
                if !valid_range(&source, start + size, end, self.desc.size) {
                    return Err(NoRange.into());
                }
                source
            }
            BlobResponse::Full(source) if parallel => {
                drop(source);
                return Err(NoRange.into());
            }
            BlobResponse::Full(source) => {
                // A 200 response starts at byte zero, never at the requested offset.
                file.set_len(0).await?;
                size = 0;
                self.update(progress, index, 0, true);
                source
            }
        };
        file.seek(SeekFrom::Start(size)).await?;
        let result: Result<()> = async {
            loop {
                let chunk = tokio::select! {
                    _ = cancel.cancelled() => { check_cancel(cancel)?; unreachable!() },
                    chunk = source.try_next() => chunk?,
                };
                let Some(chunk) = chunk else {
                    break;
                };
                if chunk.len() as u64 > length - size {
                    return Err(Restart("镜像层数据超过声明的范围").into());
                }
                // Do not cancel an in-flight write: flush it before reporting the
                // stopped task, so a resume never races an old write.
                file.write_all(&chunk).await?;
                size += chunk.len() as u64;
                if self.show {
                    (self.report)(Event::NetworkBytes(chunk.len() as u64));
                }
                self.update(progress, index, size, false);
            }
            ensure!(size == length, "下载连接提前结束，已保存断点");
            Ok(())
        }
        .await;
        file.flush().await?;
        file.sync_all().await?;
        self.update(progress, index, size, true);
        result
    }
}

fn valid_range(source: &SizedStream, start: u64, end: u64, total: u64) -> bool {
    let Some(value) = source
        .content_range
        .as_deref()
        .and_then(|s| s.strip_prefix("bytes "))
    else {
        return false;
    };
    let Some((range, actual_total)) = value.split_once('/') else {
        return false;
    };
    let Some((actual_start, actual_end)) = range.split_once('-') else {
        return false;
    };
    actual_start.parse::<u64>().ok() == Some(start)
        && actual_end.parse::<u64>().ok() == Some(end)
        && actual_total.parse::<u64>().ok() == Some(total)
        && source
            .content_length
            .is_none_or(|length| length == end - start + 1)
}
