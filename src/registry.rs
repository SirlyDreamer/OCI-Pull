use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use futures_util::{StreamExt, stream};
use oci_client::{
    Reference,
    client::{Client, ClientConfig, ClientProtocol},
    secrets::RegistryAuth,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    archive,
    download::{DownloadManager, cache_blob, cache_blob_scheduled},
    model::*,
};

const ACCEPT: &[&str] = &[
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

pub fn parse_reference(input: &str) -> Result<Reference> {
    let text = input
        .trim()
        .strip_prefix("docker://")
        .unwrap_or(input.trim());
    ensure!(
        !text.contains("://"),
        "请输入镜像引用，例如 docker.io/library/nginx:latest，而不是网页 URL"
    );
    ensure!(
        !text.contains(char::is_whitespace),
        "镜像地址不能包含空白字符"
    );
    text.parse().context("镜像地址格式无效")
}

fn client(request: &PullRequest) -> Result<Client> {
    let config = ClientConfig {
        protocol: if request.allow_http {
            ClientProtocol::Http
        } else {
            ClientProtocol::Https
        },
        https_proxy: request.proxy.clone(),
        http_proxy: request.proxy.clone(),
        connect_timeout: Some(Duration::from_secs(20)),
        read_timeout: Some(Duration::from_secs(60)),
        user_agent: "OCI Pull/0.1.0",
        ..Default::default()
    };
    Client::try_from(config).context("初始化仓库连接失败")
}

fn auth(request: &PullRequest) -> RegistryAuth {
    if request.credentials.username.is_empty() {
        RegistryAuth::Anonymous
    } else {
        RegistryAuth::Basic(
            request.credentials.username.clone(),
            request.credentials.password.clone(),
        )
    }
}

pub async fn inspect(
    request: &PullRequest,
    report: &Reporter,
    cancel: &CancellationToken,
) -> Result<ImagePlan> {
    request.validate()?;
    report(Event::Phase("连接镜像仓库，获取镜像清单…".into()));
    let client = client(request)?;
    resolve(&client, request, report, cancel).await
}

async fn resolve(
    client: &Client,
    request: &PullRequest,
    report: &Reporter,
    cancel: &CancellationToken,
) -> Result<ImagePlan> {
    let original = parse_reference(&request.image)?;
    let credentials = auth(request);
    let mut current = original.clone();
    let mut platforms = Vec::new();
    let mut selected_platform: Option<Platform> = None;
    let mut expected_size = None;
    for _ in 0..5 {
        check_cancel(cancel)?;
        let (raw, reported_digest) =
            manifest_raw(client, &current, &credentials, report, cancel).await?;
        ensure!(raw.len() <= 16 * 1024 * 1024, "镜像清单超过 16 MiB 限制");
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(&raw)));
        ensure!(reported_digest == digest, "镜像清单摘要校验失败");
        if let Some(expected) = current.digest() {
            ensure!(expected == digest, "仓库返回的镜像与指定 digest 不一致");
        }
        if let Some(expected) = expected_size {
            ensure!(raw.len() as u64 == expected, "镜像清单大小与索引不一致");
        }
        let value: serde_json::Value = serde_json::from_slice(&raw).context("清单不是有效 JSON")?;
        ensure!(
            value["schemaVersion"] == 2,
            "仅支持 Docker schema 2 和 OCI 镜像"
        );
        if let Some(entries) = value.get("manifests") {
            let entries: Vec<Descriptor> =
                serde_json::from_value(entries.clone()).context("镜像索引格式无效")?;
            let mut choices: Vec<_> = entries
                .iter()
                .filter_map(|e| e.platform.as_ref())
                .filter(|p| p.os != "unknown")
                .map(Platform::label)
                .collect();
            choices.sort();
            choices.dedup();
            platforms.extend(choices);
            platforms.sort();
            platforms.dedup();
            let selected = entries
                .iter()
                .find(|e| {
                    e.platform
                        .as_ref()
                        .is_some_and(|p| request.platform.matches(p))
                })
                .or_else(|| {
                    (entries.len() == 1 && entries[0].platform.is_none()).then(|| &entries[0])
                });
            let selected = selected.ok_or_else(|| {
                anyhow::anyhow!(
                    "没有 {} 镜像。可选平台：{}",
                    request.platform.label(),
                    platforms.join("、")
                )
            })?;
            digest_hex(&selected.digest)?;
            expected_size = Some(selected.size);
            if selected.platform.is_some() {
                selected_platform = selected.platform.clone();
            }
            current = format!(
                "{}/{}@{}",
                original.registry(),
                original.repository(),
                selected.digest
            )
            .parse()?;
            continue;
        }
        let manifest: Manifest =
            serde_json::from_value(value).context("不是可运行的容器镜像清单")?;
        ensure!(
            ACCEPT.contains(&manifest.media_type.as_str())
                && !manifest.media_type.contains("index")
                && !manifest.media_type.contains("list"),
            "不支持的镜像类型：{}",
            manifest.media_type
        );
        ensure!(manifest.layers.len() <= 4096, "镜像层数超过限制");
        ensure!(
            manifest.config.size <= 16 * 1024 * 1024,
            "镜像配置超过 16 MiB 限制"
        );
        ensure!(
            matches!(
                manifest.config.media_type.as_str(),
                "application/vnd.oci.image.config.v1+json"
                    | "application/vnd.docker.container.image.v1+json"
            ),
            "不支持的镜像配置类型"
        );
        let mut total: u64 = 0;
        let mut unique_layers = HashSet::new();
        for layer in &manifest.layers {
            digest_hex(&layer.digest)?;
            archive::layer_encoding(&layer.media_type)?;
            if unique_layers.insert(&layer.digest) {
                total = total.checked_add(layer.size).context("镜像大小溢出")?;
            }
        }
        let config_path = cache_blob(
            client,
            &current,
            &manifest.config,
            &request.cache_dir,
            report,
            cancel,
            false,
        )
        .await?;
        let config = tokio::fs::read(config_path).await?;
        #[derive(Deserialize)]
        struct Config {
            os: String,
            architecture: String,
            #[serde(default)]
            variant: Option<String>,
            rootfs: RootFs,
        }
        #[derive(Deserialize)]
        struct RootFs {
            #[serde(rename = "type")]
            kind: String,
            diff_ids: Vec<String>,
        }
        let parsed: Config = serde_json::from_slice(&config).context("镜像配置格式无效")?;
        let mut platform = Platform {
            os: parsed.os,
            architecture: parsed.architecture,
            variant: parsed.variant,
        };
        ensure!(
            platform.os == request.platform.os
                && platform.architecture == request.platform.architecture,
            "实际镜像平台 {} 与选择的 {} 不一致",
            platform.label(),
            request.platform.label()
        );
        // Some ARM configs omit variant; the index selection above is authoritative in that case.
        if let Some(index_platform) = selected_platform {
            if platform.variant.is_some() {
                ensure!(
                    index_platform.matches(&platform),
                    "索引与配置中的 CPU variant 不一致"
                );
            } else {
                platform.variant = index_platform.variant;
            }
        }
        ensure!(
            request.platform.matches(&platform),
            "镜像 CPU variant 不匹配或缺少可验证的 variant"
        );
        ensure!(
            parsed.rootfs.kind == "layers" && parsed.rootfs.diff_ids.len() == manifest.layers.len(),
            "镜像层与 rootfs.diff_ids 不一致"
        );
        for id in &parsed.rootfs.diff_ids {
            digest_hex(id)?;
        }
        if platforms.is_empty() {
            platforms.push(platform.label());
        }
        let pinned = format!(
            "{}/{}@{}",
            original.registry(),
            original.repository(),
            digest
        );
        report(Event::Resolved {
            reference: original.to_string(),
            digest: digest.clone(),
            platform: platform.label(),
            layers: unique_layers.len(),
            bytes: total,
        });
        return Ok(ImagePlan {
            reference: original.to_string(),
            pinned_reference: pinned,
            digest,
            platform,
            available_platforms: platforms,
            manifest,
            manifest_json: raw.to_vec(),
            config,
            diff_ids: parsed.rootfs.diff_ids,
        });
    }
    bail!("镜像索引嵌套超过 5 层")
}

pub async fn pull(
    request: PullRequest,
    report: Reporter,
    cancel: CancellationToken,
) -> Result<PathBuf> {
    request.validate()?;
    let client = client(&request)?;
    let plan = resolve(&client, &request, &report, &cancel).await?;
    pull_resolved(request, plan, report, cancel).await
}

/// Rebuilds a task after restart without resolving its mutable tag again.
pub async fn pull_pinned(
    request: PullRequest,
    digest: &str,
    report: Reporter,
    cancel: CancellationToken,
) -> Result<PathBuf> {
    let manager = DownloadManager::new(DownloadSettings {
        concurrency: request.concurrency,
        chunk_size_mib: request.chunk_size_mib,
    })?;
    pull_pinned_managed(request, digest, report, cancel, manager).await
}

pub async fn pull_pinned_managed(
    request: PullRequest,
    digest: &str,
    report: Reporter,
    cancel: CancellationToken,
    manager: std::sync::Arc<DownloadManager>,
) -> Result<PathBuf> {
    digest_hex(digest)?;
    request.validate()?;
    let original = parse_reference(&request.image)?;
    let mut pinned = request.clone();
    pinned.image = format!(
        "{}/{}@{}",
        original.registry(),
        original.repository(),
        digest
    );
    // The recorded variant was confirmed from the index before this task was
    // created. A pinned manifest may omit it from config, so retain that prior
    // selection, while still rejecting conflicting platform data in config.
    pinned.platform.variant = None;
    let mut plan = inspect(&pinned, &report, &cancel).await?;
    ensure!(plan.digest == digest, "恢复任务时镜像摘要不一致");
    if plan.platform.variant.is_none() {
        plan.platform.variant = request.platform.variant.clone();
    }
    ensure!(
        request.platform.matches(&plan.platform),
        "恢复任务时镜像平台不一致"
    );
    plan.reference = original.to_string();
    pull_resolved_managed(request, plan, report, cancel, manager).await
}

/// Downloads exactly the manifest shown in the task preview, preserving its source tag.
pub async fn pull_resolved(
    request: PullRequest,
    plan: ImagePlan,
    report: Reporter,
    cancel: CancellationToken,
) -> Result<PathBuf> {
    let manager = DownloadManager::new(DownloadSettings {
        concurrency: request.concurrency,
        chunk_size_mib: request.chunk_size_mib,
    })?;
    pull_resolved_managed(request, plan, report, cancel, manager).await
}

pub async fn pull_resolved_managed(
    request: PullRequest,
    plan: ImagePlan,
    report: Reporter,
    cancel: CancellationToken,
    manager: std::sync::Arc<DownloadManager>,
) -> Result<PathBuf> {
    request.validate()?;
    let client = client(&request)?;
    // Authenticate this fresh client and verify the pinned manifest again before blob requests.
    let reference: Reference = plan.pinned_reference.parse()?;
    let (raw, _) = manifest_raw(&client, &reference, &auth(&request), &report, &cancel).await?;
    ensure!(
        format!("sha256:{}", hex::encode(Sha256::digest(&raw))) == plan.digest,
        "预览后的镜像摘要发生变化"
    );
    if let Some(directory) = &request.manifest_dir {
        persist_plan(&plan, directory)?;
    }
    tokio::fs::create_dir_all(&request.cache_dir).await?;
    // The config was validated during preview; recover it if the cache has since been cleared.
    cache_blob(
        &client,
        &reference,
        &plan.manifest.config,
        &request.cache_dir,
        &report,
        &cancel,
        false,
    )
    .await?;
    let unique: std::collections::HashMap<_, _> = plan
        .manifest
        .layers
        .iter()
        .map(|l| (&l.digest, l.size))
        .collect();
    report(Event::Resolved {
        reference: plan.reference.clone(),
        digest: plan.digest.clone(),
        platform: plan.platform.label(),
        layers: unique.len(),
        bytes: unique.values().sum(),
    });
    let output = request.output_path(&plan.reference);
    ensure!(
        !output.exists(),
        "目标文件已存在，请更换保存目录或移动现有文件：{}",
        output.display()
    );
    tokio::fs::create_dir_all(&request.output_dir)
        .await
        .context("无法创建保存目录")?;
    report(Event::LayerList(plan.manifest.layers.clone()));
    report(Event::Phase("下载并校验镜像层…".into()));
    let reference: Reference = plan.pinned_reference.parse()?;
    // Deduplicate repeated descriptors without changing their archive order.
    let mut seen = HashSet::new();
    let layers: Vec<_> = plan
        .manifest
        .layers
        .iter()
        .filter(|l| seen.insert(l.digest.clone()))
        .cloned()
        .collect();
    let workers = cancel.child_token();
    let scheduler = manager.task();
    let downloads: Vec<_> = layers
        .into_iter()
        .map(|layer| {
            let scheduler = scheduler.clone();
            let (client, reference, cache, report, cancel) = (
                client.clone(),
                reference.clone(),
                request.cache_dir.clone(),
                report.clone(),
                workers.clone(),
            );
            async move {
                cache_blob_scheduled(
                    &client, &reference, &layer, &cache, &report, &cancel, true, &scheduler,
                )
                .await
            }
        })
        .collect();
    // All layers submit their pieces to the same task scheduler. Assembly and
    // SHA verification do not occupy network slots or gate the next layer.
    let count = downloads.len().max(1);
    let mut pending = stream::iter(downloads).buffer_unordered(count);
    let mut failure = None;
    while let Some(result) = pending.next().await {
        if let Err(error) = result
            && failure.is_none()
        {
            failure = Some(error);
            workers.cancel();
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    check_cancel(&cancel)?;
    report(Event::Phase("生成镜像归档并压缩…".into()));
    let output_clone = output.clone();
    let report_clone = report.clone();
    tokio::task::spawn_blocking(move || {
        archive::export(&plan, &request, &output_clone, &report_clone, &cancel)
    })
    .await
    .context("归档任务意外结束")??;
    let bytes = tokio::fs::metadata(&output).await?.len();
    report(Event::Complete {
        path: output.clone(),
        bytes,
    });
    Ok(output)
}

/// Persist exact verified JSON, named by content digest inside a task's manifests/.
pub fn persist_plan(plan: &ImagePlan, directory: &Path) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(directory)?;
    for (digest, bytes) in [
        (&plan.digest, &plan.manifest_json),
        (&plan.manifest.config.digest, &plan.config),
    ] {
        let name = digest_hex(digest)?;
        ensure!(
            format!("sha256:{}", hex::encode(Sha256::digest(bytes))) == *digest,
            "任务元数据摘要校验失败"
        );
        let path = directory.join(format!("{name}.json"));
        if std::fs::read(&path).ok().as_deref() == Some(bytes.as_slice()) {
            continue;
        }
        let mut temp = tempfile::NamedTempFile::new_in(directory)?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|e| e.error)?;
    }
    Ok(())
}

async fn manifest_raw(
    client: &Client,
    reference: &Reference,
    credentials: &RegistryAuth,
    report: &Reporter,
    cancel: &CancellationToken,
) -> Result<(Vec<u8>, String)> {
    for attempt in 0..3 {
        let result = tokio::select! {
            _ = cancel.cancelled() => bail!("任务已取消"),
            result = client.pull_manifest_raw(reference, credentials, ACCEPT) => result,
        };
        match result {
            Ok((bytes, digest)) => {
                ensure!(bytes.len() <= 16 * 1024 * 1024, "镜像清单超过 16 MiB 限制");
                return Ok((bytes.to_vec(), digest));
            }
            Err(e) if attempt == 2 => {
                return Err(e).context("获取镜像清单失败（请检查仓库地址、代理和权限）");
            }
            Err(_) => {
                report(Event::Phase(format!("重试连接 {}/3", attempt + 2)));
                tokio::select! { _ = cancel.cancelled() => bail!("任务已取消"), _ = tokio::time::sleep(Duration::from_secs(1 << attempt)) => {} }
            }
        }
    }
    unreachable!()
}

pub async fn platforms(request: &PullRequest, cancel: &CancellationToken) -> Result<Vec<String>> {
    request.validate()?;
    let client = client(request)?;
    let reference = parse_reference(&request.image)?;
    let report: Reporter = std::sync::Arc::new(|_| {});
    let (raw, reported) =
        manifest_raw(&client, &reference, &auth(request), &report, cancel).await?;
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&raw)));
    ensure!(
        reported == digest && reference.digest().is_none_or(|expected| expected == digest),
        "镜像清单摘要校验失败"
    );
    let value: serde_json::Value = serde_json::from_slice(&raw)?;
    let mut choices: Vec<String> = if let Some(entries) = value.get("manifests") {
        let entries: Vec<Descriptor> = serde_json::from_value(entries.clone())?;
        entries
            .into_iter()
            .filter_map(|e| e.platform)
            .filter(|p| p.os != "unknown" && p.architecture != "unknown")
            .map(|p| p.label())
            .collect()
    } else {
        let manifest: Manifest = serde_json::from_value(value)?;
        ensure!(manifest.config.size <= 16 * 1024 * 1024, "镜像配置过大");
        let path = cache_blob(
            &client,
            &reference,
            &manifest.config,
            &request.cache_dir,
            &report,
            cancel,
            false,
        )
        .await?;
        let value: serde_json::Value = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        let platform = Platform {
            os: value["os"].as_str().context("缺少镜像系统信息")?.into(),
            architecture: value["architecture"]
                .as_str()
                .context("缺少镜像架构信息")?
                .into(),
            variant: value["variant"].as_str().map(String::from),
        };
        vec![platform.label()]
    };
    choices.sort();
    choices.dedup();
    ensure!(!choices.is_empty(), "没有可选的镜像平台");
    Ok(choices)
}
