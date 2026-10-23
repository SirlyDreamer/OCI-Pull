//! Reclaimable download data, separate from task history and user archives.
//! Each entry has an ownership marker and an OS lock outside the deleted tree.
use crate::model::PullRequest;
use crate::paths::{
    directory_key, task_directory_name, task_id_from_directory_name, valid_directory_name,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const OWNER: &str = ".oci-pull-cache";
const SIGNATURE: &[u8] = b"OCI Pull cache entry v1\n";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct CacheManager {
    root: PathBuf,
}
pub struct CacheLease {
    path: PathBuf,
    root: PathBuf,
    lock: Option<File>,
    lock_path: PathBuf,
    temporary: bool,
}
#[derive(Default, Debug)]
pub struct Cleanup {
    pub bytes: u64,
    pub skipped: usize,
    pub tasks: Vec<(PathBuf, u64)>,
    pub errors: Vec<String>,
}
impl Cleanup {
    pub fn append(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.skipped += other.skipped;
        self.tasks.extend(other.tasks);
        self.errors.extend(other.errors);
    }
}
impl CacheManager {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn task_path(&self, id: u64) -> PathBuf {
        self.root.join("tasks").join(task_directory_name(id))
    }
    pub fn task(&self, id: u64) -> Result<Arc<CacheLease>> {
        self.acquire("tasks", &task_directory_name(id), false)
    }
    pub fn preview(&self) -> Result<Arc<CacheLease>> {
        let key = format!(
            "preview-{}-{}-{}",
            directory_key(&std::process::id().to_be_bytes()),
            directory_key(
                &SystemTime::now()
                    .duration_since(UNIX_EPOCH)?
                    .as_nanos()
                    .to_be_bytes()
            ),
            directory_key(&SEQUENCE.fetch_add(1, Ordering::Relaxed).to_be_bytes())
        );
        self.acquire("previews", &key, true)
    }
    pub fn cli(&self, key: &str) -> Result<Arc<CacheLease>> {
        self.acquire("cli", key, false)
    }
    fn acquire(&self, category: &str, key: &str, temporary: bool) -> Result<Arc<CacheLease>> {
        self.try_acquire(category, key, temporary)?
            .context("缓存正在使用中，请稍后重试")
    }
    fn try_acquire(
        &self,
        category: &str,
        key: &str,
        temporary: bool,
    ) -> Result<Option<Arc<CacheLease>>> {
        ensure!(
            valid_directory_name(key),
            "缓存目录名只能包含小写英文字母和 -"
        );
        ensure!(
            ["tasks", "previews", "cli"].contains(&category),
            "缓存类型无效"
        );
        ensure!(self.root.is_absolute(), "缓存目录必须是绝对路径");
        fs::create_dir_all(&self.root).context("无法创建缓存根目录")?;
        let root = fs::canonicalize(&self.root)?;
        let _control = control_lock(&root)?;
        let locks = root.join("locks");
        safe_directory(&locks)?;
        let lock_path = locks.join(format!("{category}-{key}.lock"));
        reject_link(&lock_path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        ensure!(
            lock.metadata()?.len() == 0,
            "缓存锁文件包含未知数据，未修改"
        );
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        safe_directory(&root.join(category))?;
        let path = root.join(category).join(key);
        safe_directory(&path)?;
        let owner = path.join(OWNER);
        reject_link(&owner)?;
        if owner.exists() {
            ensure!(fs::read(&owner)? == SIGNATURE, "缓存归属标记不正确");
        } else {
            ensure!(
                fs::read_dir(&path)?.next().is_none(),
                "缓存目录包含未知数据，未修改"
            );
        }
        fs::write(owner, SIGNATURE)?;
        Ok(Some(Arc::new(CacheLease {
            path,
            root,
            lock: Some(lock),
            lock_path,
            temporary,
        })))
    }
    pub fn remove_task(&self, id: u64) -> Result<u64> {
        if !self.task_path(id).exists() {
            return Ok(0);
        }
        self.task(id)?.clear()
    }
    /// `keep` is used at startup; manual clearing passes an empty set. Active
    /// tasks/previews/CLI downloads are protected by leases in both cases.
    pub fn collect(&self, keep: &HashSet<u64>, manual: bool) -> Cleanup {
        let mut result = Cleanup::default();
        if !self.root.exists() {
            return result;
        }
        for category in ["tasks", "previews", "cli"] {
            let directory = self.root.join(category);
            let scan = (|| -> Result<()> {
                reject_link(&directory)?;
                if !directory.exists() {
                    return Ok(());
                }
                for entry in fs::read_dir(&directory)? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    let id = task_id_from_directory_name(&name);
                    if category == "tasks" && (id.is_none() || keep.contains(&id.unwrap())) {
                        continue;
                    }
                    let owner = entry.path().join(OWNER);
                    reject_link(&entry.path())?;
                    reject_link(&owner)?;
                    if fs::read(&owner).ok().as_deref() != Some(SIGNATURE) {
                        continue;
                    }
                    // Failed CLI downloads have no task record. Keep their
                    // checkpoints for seven days unless explicitly cleared.
                    if category == "cli"
                        && !manual
                        && owner
                            .metadata()?
                            .modified()?
                            .elapsed()
                            .unwrap_or_default()
                            .as_secs()
                            < 7 * 24 * 3600
                    {
                        continue;
                    }
                    let clean = (|| -> Result<()> {
                        match self.try_acquire(category, &name, false)? {
                            Some(lease) => {
                                result.bytes += lease.clear()?;
                                if category == "tasks" {
                                    result.tasks.push((self.root.clone(), id.unwrap()));
                                }
                            }
                            None => result.skipped += 1,
                        }
                        Ok(())
                    })();
                    if let Err(e) = clean {
                        result.errors.push(format!("{category}/{name}：{e:#}"));
                    }
                }
                Ok(())
            })();
            if let Err(e) = scan {
                result
                    .errors
                    .push(format!("{}：{e:#}", directory.display()));
            }
        }
        result
    }
}
impl CacheLease {
    /// Copies a checkpoint into an empty, exclusively leased destination. The
    /// caller must durably commit its task record before clearing the source.
    pub fn copy_to(&self, destination: &CacheLease) -> Result<()> {
        ensure!(self.path != destination.path, "源缓存与目标缓存相同");
        ensure!(
            fs::read_dir(&destination.path)?.all(|e| e.is_ok_and(|e| e.file_name() == OWNER)),
            "目标任务缓存已存在，未覆盖"
        );
        tree_bytes(&self.path)?;
        if let Err(error) = copy_tree(&self.path, &destination.path) {
            let _ = destination.clear();
            return Err(error);
        }
        Ok(())
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn bind(&self, request: &mut PullRequest) -> Result<()> {
        let output = resolved_future_path(&request.output_dir)?;
        ensure!(
            !output.starts_with(&self.root),
            "保存目录不能位于缓存目录内"
        );
        request.cache_dir = self.path.join("blobs");
        request.manifest_dir = Some(self.path.join("manifests"));
        safe_directory(&request.cache_dir)?;
        safe_directory(request.manifest_dir.as_ref().unwrap())?;
        Ok(())
    }
    pub fn clear(&self) -> Result<u64> {
        ensure!(
            self.path.starts_with(&self.root) && self.path != self.root,
            "拒绝清理缓存根目录之外的数据"
        );
        if !self.path.exists() {
            return Ok(0);
        }
        reject_link(&self.path)?;
        ensure!(
            fs::read(self.path.join(OWNER))? == SIGNATURE,
            "缓存归属标记不正确"
        );
        let bytes = tree_bytes(&self.path)?; // Validate the entire tree before deleting anything.
        fs::remove_dir_all(&self.path).context("缓存清理失败")?;
        Ok(bytes)
    }
}
impl Drop for CacheLease {
    fn drop(&mut self) {
        if self.temporary {
            let _ = self.clear();
        }
        // Serializing open/unlink avoids two processes locking different file
        // identities for the same entry. Only one root control file remains.
        if let Ok(_control) = control_lock(&self.root) {
            drop(self.lock.take());
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}
fn control_lock(root: &Path) -> Result<File> {
    let path = root.join(".oci-pull-lock");
    reject_link(&path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    ensure!(
        file.metadata()?.len() == 0,
        "缓存控制文件包含未知数据，未修改"
    );
    file.lock()?;
    Ok(file)
}
fn safe_directory(path: &Path) -> Result<()> {
    reject_link(path)?;
    fs::create_dir_all(path)?;
    Ok(())
}
fn reject_link(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                ensure!(
                    metadata.file_attributes() & 0x400 == 0,
                    "缓存中存在链接或重解析点，已跳过"
                );
            }
            ensure!(!metadata.file_type().is_symlink(), "缓存中存在链接，已跳过");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn tree_bytes(path: &Path) -> Result<u64> {
    reject_link(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    ensure!(metadata.is_dir(), "缓存内存在不支持的文件类型");
    let mut bytes = 0u64;
    for entry in fs::read_dir(path)? {
        bytes = bytes.saturating_add(tree_bytes(&entry?.path())?);
    }
    Ok(bytes)
}
pub fn resolved_future_path(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    if absolute.exists() {
        return Ok(fs::canonicalize(absolute)?);
    }
    let Some(parent) = absolute.parent() else {
        bail!("保存路径无效")
    };
    Ok(resolved_future_path(parent)?.join(absolute.file_name().context("保存路径无效")?))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_name() == OWNER {
            continue;
        }
        reject_link(&entry.path())?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            ensure!(
                valid_directory_name(&entry.file_name().to_string_lossy()),
                "缓存子目录名只能包含小写英文字母和 -"
            );
            fs::create_dir(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else {
            let mut input = File::open(entry.path())?;
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&target)?;
            let mut expected = Sha256::new();
            let mut buffer = vec![0u8; 1024 * 1024];
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                expected.update(&buffer[..count]);
                output.write_all(&buffer[..count])?;
            }
            output.sync_all()?;
            drop(output);
            let mut actual = Sha256::new();
            let mut copied = File::open(&target)?;
            loop {
                let count = copied.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                actual.update(&buffer[..count]);
            }
            ensure!(
                expected.finalize() == actual.finalize(),
                "迁移文件校验失败：{}",
                target.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_entries_create_only_lowercase_and_hyphen_directories() {
        fn check(path: &Path) {
            for entry in fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    assert!(valid_directory_name(&entry.file_name().to_string_lossy()));
                    check(&entry.path());
                }
            }
        }
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join(crate::paths::APP_DIRECTORY);
        let manager = CacheManager::new(root.clone());
        let task = manager.task(u64::MAX).unwrap();
        let preview = manager.preview().unwrap();
        let cli = manager.cli(&directory_key(&[0, 255, 42])).unwrap();
        let digest = format!("sha256:{}", "0123456789abcdef".repeat(4));
        fs::create_dir_all(
            task.path()
                .join("blobs")
                .join(crate::paths::blob_parts_directory_name(&digest).unwrap()),
        )
        .unwrap();
        for name in [
            "42",
            "UPPER",
            "has space",
            "has_underscore",
            "a.parts",
            "中文",
            "../escape",
        ] {
            assert!(manager.cli(name).is_err());
        }
        check(&root);
        drop((task, preview, cli));
        let removed = manager.collect(&HashSet::new(), true);
        assert!(removed.errors.is_empty());
        assert_eq!(removed.tasks, vec![(root, u64::MAX)]);
    }
    #[test]
    fn collection_keeps_live_leases_and_paused_tasks_then_reclaims_idle_data() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CacheManager::new(temp.path().join("cache"));
        let active = manager.task(1).unwrap();
        fs::write(active.path().join("part"), b"active").unwrap();
        for id in [2, 3] {
            let entry = manager.task(id).unwrap();
            fs::write(entry.path().join("part"), b"checkpoint").unwrap();
        }
        let archive = temp.path().join("image.tar.zst");
        fs::write(&archive, b"archive").unwrap();
        fs::create_dir_all(manager.task_path(4)).unwrap();
        fs::write(manager.task_path(4).join("personal-file"), b"untouched").unwrap();
        let sweep = manager.collect(&HashSet::from([2]), false);
        assert!(sweep.errors.is_empty());
        assert_eq!(sweep.skipped, 1);
        assert!(manager.task_path(1).exists());
        assert!(manager.task_path(2).exists());
        assert!(!manager.task_path(3).exists());
        let clear = manager.collect(&HashSet::new(), true);
        assert_eq!(clear.skipped, 1);
        assert!(!manager.task_path(2).exists());
        assert!(manager.task_path(4).join("personal-file").exists());
        assert_eq!(fs::read(archive).unwrap(), b"archive");
        drop(active);
        assert!(manager.collect(&HashSet::new(), true).errors.is_empty());
        assert!(!manager.task_path(1).exists());
    }
    #[test]
    fn preview_lifetime_follows_last_worker_and_cli_checkpoints_expire_or_clear() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CacheManager::new(temp.path().join("cache"));
        let preview = manager.preview().unwrap();
        let path = preview.path().to_path_buf();
        let worker = preview.clone();
        drop(preview);
        assert_eq!(manager.collect(&HashSet::new(), true).skipped, 1);
        drop(worker);
        assert!(!path.exists());
        let cli = manager.cli("test").unwrap();
        let path = cli.path().to_path_buf();
        drop(cli);
        manager.collect(&HashSet::new(), false);
        assert!(path.exists());
        let old = SystemTime::now() - std::time::Duration::from_secs(8 * 86400);
        File::options()
            .write(true)
            .open(path.join(OWNER))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        manager.collect(&HashSet::new(), false);
        assert!(!path.exists());
    }
    #[test]
    fn migration_verifies_nested_checkpoints_and_never_overwrites_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = CacheManager::new(temp.path().join("old")).task(42).unwrap();
        fs::create_dir_all(source.path().join("blobs/a-parts")).unwrap();
        fs::write(source.path().join("blobs/a-parts/0"), b"partial-checkpoint").unwrap();
        fs::write(source.path().join("blobs/a-parts/layout.json"), b"layout").unwrap();
        let destination = CacheManager::new(temp.path().join("new")).task(42).unwrap();
        source.copy_to(&destination).unwrap();
        assert_eq!(
            fs::read(destination.path().join("blobs/a-parts/0")).unwrap(),
            b"partial-checkpoint"
        );
        assert!(source.path().exists());
        assert!(source.copy_to(&destination).is_err());
        source.clear().unwrap();
        assert!(
            destination
                .path()
                .join("blobs/a-parts/layout.json")
                .exists()
        );
    }
}
