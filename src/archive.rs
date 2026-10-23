use std::{
    collections::HashSet,
    fs::File,
    io::{self, BufReader, BufWriter, Read, Seek, Write},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use flate2::{Compression as GzLevel, read::MultiGzDecoder, write::GzEncoder};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::model::*;

#[derive(Debug, Clone, Copy)]
pub enum LayerEncoding {
    Tar,
    Gzip,
    Zstd,
}

pub fn layer_encoding(media_type: &str) -> Result<LayerEncoding> {
    match media_type {
        "application/vnd.oci.image.layer.v1.tar"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar"
        | "application/vnd.docker.image.rootfs.diff.tar" => Ok(LayerEncoding::Tar),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip"
        | "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip" => Ok(LayerEncoding::Gzip),
        "application/vnd.oci.image.layer.v1.tar+zstd"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd" => Ok(LayerEncoding::Zstd),
        _ => bail!("不支持的镜像层类型：{media_type}"),
    }
}

enum Output {
    Raw(BufWriter<File>),
    Gzip(GzEncoder<BufWriter<File>>),
    Bzip2(bzip2::write::BzEncoder<BufWriter<File>>),
    Xz(Box<lzma_rust2::XzWriter<BufWriter<File>>>),
    Zstd(zstd::stream::write::Encoder<'static, BufWriter<File>>),
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Raw(w) => w.write(bytes),
            Self::Gzip(w) => w.write(bytes),
            Self::Bzip2(w) => w.write(bytes),
            Self::Xz(w) => w.write(bytes),
            Self::Zstd(w) => w.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Raw(w) => w.flush(),
            Self::Gzip(w) => w.flush(),
            Self::Bzip2(w) => w.flush(),
            Self::Xz(w) => w.flush(),
            Self::Zstd(w) => w.flush(),
        }
    }
}

impl Output {
    fn finish(self) -> io::Result<()> {
        let mut writer = match self {
            Self::Raw(w) => w,
            Self::Gzip(w) => w.finish()?,
            Self::Bzip2(w) => w.finish()?,
            Self::Xz(w) => (*w).finish()?,
            Self::Zstd(w) => w.finish()?,
        };
        writer.flush()?;
        writer.get_ref().sync_all()
    }
}

struct CancelWriter {
    inner: Output,
    cancel: CancellationToken,
}
impl Write for CancelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(io::Error::other("任务已取消"));
        }
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub fn export(
    plan: &ImagePlan,
    request: &PullRequest,
    destination: &Path,
    report: &Reporter,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancel(cancel)?;
    ensure!(
        !destination.exists(),
        "目标文件已存在，未覆盖：{}",
        destination.display()
    );
    let temporary = tempfile::Builder::new()
        .prefix(".oci-pull-")
        .suffix(".part")
        .tempfile_in(&request.output_dir)
        .context("无法创建归档文件")?;
    let file = BufWriter::with_capacity(1024 * 1024, temporary.reopen()?);
    let writer = match request.compression {
        Compression::None => Output::Raw(file),
        Compression::Gzip => Output::Gzip(GzEncoder::new(file, GzLevel::new(6))),
        Compression::Bzip2 => Output::Bzip2(bzip2::write::BzEncoder::new(
            file,
            bzip2::Compression::new(6),
        )),
        Compression::Xz => Output::Xz(Box::new(lzma_rust2::XzWriter::new(
            file,
            lzma_rust2::XzOptions::with_preset(6),
        )?)),
        Compression::Zstd => Output::Zstd(zstd::stream::write::Encoder::new(
            file,
            request.compression_level,
        )?),
    };
    let mut archive = tar::Builder::new(CancelWriter {
        inner: writer,
        cancel: cancel.clone(),
    });
    match request.format {
        ArchiveFormat::Docker => docker_archive(&mut archive, plan, request, report, cancel)?,
        ArchiveFormat::Oci => oci_archive(&mut archive, plan, request, report, cancel)?,
    }
    archive.finish().context("完成 tar 归档失败")?;
    archive
        .into_inner()?
        .inner
        .finish()
        .context("完成压缩失败")?;
    check_cancel(cancel)?;
    temporary
        .persist_noclobber(destination)
        .map_err(|e| e.error)
        .context("保存最终文件失败（不会覆盖已有文件）")?;
    Ok(())
}

fn add_bytes<W: Write>(tar: &mut tar::Builder<W>, name: &str, bytes: &[u8]) -> Result<()> {
    add_reader(tar, name, bytes.len() as u64, bytes)
}

fn add_reader<W: Write, R: Read>(
    tar: &mut tar::Builder<W>,
    name: &str,
    size: u64,
    reader: R,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    tar.append_data(&mut header, name, reader)
        .with_context(|| format!("写入归档成员 {name} 失败"))
}

fn layer_reader(path: &Path, encoding: LayerEncoding) -> Result<Box<dyn Read>> {
    let file = BufReader::with_capacity(256 * 1024, File::open(path)?);
    Ok(match encoding {
        LayerEncoding::Tar => Box::new(file),
        LayerEncoding::Gzip => Box::new(MultiGzDecoder::new(file)),
        LayerEncoding::Zstd => Box::new(zstd::stream::read::Decoder::new(file)?),
    })
}

fn docker_archive<W: Write>(
    tar: &mut tar::Builder<W>,
    plan: &ImagePlan,
    request: &PullRequest,
    report: &Reporter,
    cancel: &CancellationToken,
) -> Result<()> {
    let config_name = format!("{}.json", digest_hex(&plan.manifest.config.digest)?);
    add_bytes(tar, &config_name, &plan.config)?;
    let mut names = Vec::new();
    let mut written = HashSet::new();
    for (i, layer) in plan.manifest.layers.iter().enumerate() {
        check_cancel(cancel)?;
        let name = format!("layers/{}/layer.tar", digest_hex(&plan.diff_ids[i])?);
        names.push(name.clone());
        if written.insert(name.clone()) {
            report(Event::Phase(format!(
                "校验并打包镜像层 {}/{}…",
                i + 1,
                plan.manifest.layers.len()
            )));
            let path = request.cache_dir.join(digest_hex(&layer.digest)?);
            let mut reader = layer_reader(&path, layer_encoding(&layer.media_type)?)?;
            // A tar header requires the decoded size. Spool just this layer, never a whole image tar.
            let mut spool =
                tempfile::tempfile_in(&request.cache_dir).context("无法创建镜像层临时文件")?;
            let mut hash = Sha256::new();
            let mut size = 0u64;
            let mut buffer = vec![0u8; 256 * 1024];
            loop {
                check_cancel(cancel)?;
                let n = reader.read(&mut buffer).context("解压镜像层失败")?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
                spool.write_all(&buffer[..n])?;
                size = size.checked_add(n as u64).context("解压大小溢出")?;
            }
            ensure!(
                format!("sha256:{}", hex::encode(hash.finalize())) == plan.diff_ids[i],
                "镜像层 {} 的解压摘要 diff_id 不匹配",
                i + 1
            );
            spool.rewind()?;
            add_reader(tar, &name, size, spool)?;
        }
        report(Event::Packaging {
            completed: i + 1,
            total: plan.manifest.layers.len(),
        });
    }
    let reference = crate::registry::parse_reference(&plan.reference)?;
    // A digest-only source has no repository tag; preserve that rather than inventing :latest.
    let tags: Vec<String> = if reference.digest().is_none() {
        vec![plan.reference.clone()]
    } else {
        Vec::new()
    };
    let manifest = json!([{ "Config": config_name, "RepoTags": tags, "Layers": names }]);
    add_bytes(tar, "manifest.json", &serde_json::to_vec(&manifest)?)
}

fn oci_archive<W: Write>(
    tar: &mut tar::Builder<W>,
    plan: &ImagePlan,
    request: &PullRequest,
    report: &Reporter,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut manifest: serde_json::Value = serde_json::from_slice(&plan.manifest_json)?;
    manifest["mediaType"] = json!("application/vnd.oci.image.manifest.v1+json");
    manifest["config"]["mediaType"] = json!("application/vnd.oci.image.config.v1+json");
    let mut written = HashSet::new();
    for (i, layer) in plan.manifest.layers.iter().enumerate() {
        check_cancel(cancel)?;
        let nondistributable =
            layer.media_type.contains("foreign") || layer.media_type.contains("nondistributable");
        let base = if nondistributable {
            "application/vnd.oci.image.layer.nondistributable.v1.tar"
        } else {
            "application/vnd.oci.image.layer.v1.tar"
        };
        let suffix = match layer_encoding(&layer.media_type)? {
            LayerEncoding::Tar => "",
            LayerEncoding::Gzip => "+gzip",
            LayerEncoding::Zstd => "+zstd",
        };
        manifest["layers"][i]["mediaType"] = json!(format!("{base}{suffix}"));
        if written.insert(layer.digest.clone()) {
            let hex = digest_hex(&layer.digest)?;
            let file = BufReader::new(File::open(request.cache_dir.join(hex))?);
            add_reader(tar, &format!("blobs/sha256/{hex}"), layer.size, file)?;
        }
        report(Event::Packaging {
            completed: i + 1,
            total: plan.manifest.layers.len(),
        });
    }
    add_bytes(
        tar,
        &format!("blobs/sha256/{}", digest_hex(&plan.manifest.config.digest)?),
        &plan.config,
    )?;
    let manifest = serde_json::to_vec(&manifest)?;
    let digest = hex::encode(Sha256::digest(&manifest));
    add_bytes(tar, &format!("blobs/sha256/{digest}"), &manifest)?;
    add_bytes(tar, "oci-layout", br#"{"imageLayoutVersion":"1.0.0"}"#)?;
    let index = json!({ "schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [{
        "mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": format!("sha256:{digest}"), "size": manifest.len(),
        "platform": plan.platform, "annotations": {"org.opencontainers.image.ref.name": plan.reference}
    }] });
    add_bytes(tar, "index.json", &serde_json::to_vec(&index)?)
}
