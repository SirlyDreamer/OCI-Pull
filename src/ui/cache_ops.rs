use super::{
    connection::NetworkSettings,
    store::TaskStore,
    task::{Record, State, config_dir},
};
use anyhow::{Result, ensure};
use oci_pull::{
    cache::{CacheManager, resolved_future_path},
    model::data_root,
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChangeMode {
    Migrate,
    Clear,
}
pub struct PendingChange {
    pub settings: NetworkSettings,
    pub root: PathBuf,
    pub mode: ChangeMode,
    pub resume: HashSet<u64>,
}
pub struct ChangeResult {
    pub records: Vec<Record>,
    pub errors: Vec<String>,
}
#[derive(Default)]
pub struct CacheStatus {
    pub busy: bool,
    pub message: Option<String>,
}

pub fn validate_root(root: &Path, old_roots: &[PathBuf], outputs: &[PathBuf]) -> Result<PathBuf> {
    ensure!(root.is_absolute(), "缓存目录必须是绝对路径");
    let root = resolved_future_path(root)?;
    for protected in [data_root().join("tasks"), config_dir()] {
        let protected = resolved_future_path(&protected)?;
        ensure!(
            !root.starts_with(&protected) && !protected.starts_with(&root),
            "缓存目录不能与任务记录或配置目录重叠"
        );
    }
    for old in old_roots {
        let old = resolved_future_path(old)?;
        ensure!(
            root == old || (!root.starts_with(&old) && !old.starts_with(&root)),
            "新旧缓存目录不能相互嵌套"
        );
    }
    for output in outputs {
        ensure!(
            !resolved_future_path(output)?.starts_with(&root),
            "缓存目录不能包含镜像保存目录"
        );
    }
    Ok(root)
}

/// Records are committed one task at a time. A crash leaves either the old
/// checkpoint or a fully verified new checkpoint reachable by the saved record.
pub fn change_root(
    store: &TaskStore,
    records: Vec<Record>,
    root: &Path,
    mode: ChangeMode,
) -> ChangeResult {
    let mut result = ChangeResult {
        records: Vec::new(),
        errors: Vec::new(),
    };
    // Establish a writable, lockable destination before touching checkpoints.
    if let Err(error) = CacheManager::new(root.to_path_buf()).preview() {
        result.errors.push(format!("无法使用新缓存目录：{error:#}"));
        return result;
    }
    for mut record in records {
        if record.options.cache_root == root {
            continue;
        }
        let old = CacheManager::new(record.options.cache_root.clone());
        let change = (|| -> Result<()> {
            let source = old
                .task_path(record.id)
                .exists()
                .then(|| old.task(record.id))
                .transpose()?;
            let destination = if mode == ChangeMode::Migrate
                && record.state != State::Complete
                && let Some(source) = &source
            {
                let lease = CacheManager::new(root.to_path_buf()).task(record.id)?;
                source.copy_to(&lease)?;
                Some(lease)
            } else {
                None
            };
            record.options.cache_root = root.to_path_buf();
            if mode == ChangeMode::Clear && record.state != State::Complete {
                for layer in &mut record.layer_progress {
                    layer.downloaded = 0;
                    layer.started = false;
                    layer.cached = false;
                }
            }
            if let Err(error) = store.save_record(&record) {
                if let Some(destination) = &destination {
                    let _ = destination.clear();
                }
                return Err(error);
            }
            result.records.push(record.clone());
            if let Some(source) = source {
                source.clear()?;
            }
            Ok(())
        })();
        if let Err(error) = change {
            result.errors.push(format!("任务 {}：{error:#}", record.id));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::super::task::DownloadTask;
    use super::*;
    fn record(root: &Path) -> Record {
        let mut record: Record = serde_json::from_value(serde_json::json!({
            "id": 42, "image": "alpine:latest", "platform": "linux/amd64",
            "format": "Docker", "compression": "Zstd", "directory": "images",
            "digest": format!("sha256:{}", "ab".repeat(32)), "state": "Paused",
            "total": 100, "layers": 1, "output": null, "output_bytes": 0, "error": null,
            "layer_progress": [{ "digest": format!("sha256:{}", "a".repeat(64)), "total":100, "downloaded":30, "started":true, "cached":false }]
        })).unwrap();
        record.options.cache_root = root.to_path_buf();
        record
    }
    fn task_cache(root: &Path) -> PathBuf {
        CacheManager::new(root.to_path_buf()).task_path(42)
    }
    fn checkpoint(root: &Path) {
        let entry = CacheManager::new(root.to_path_buf()).task(42).unwrap();
        std::fs::create_dir(entry.path().join("blobs")).unwrap();
        std::fs::write(entry.path().join("blobs/partial"), b"checkpoint").unwrap();
    }
    #[test]
    fn migration_and_clear_commit_task_paths_and_preserve_history() {
        for mode in [ChangeMode::Migrate, ChangeMode::Clear] {
            let temp = tempfile::tempdir().unwrap();
            let old = temp.path().join("old");
            let new = temp.path().join("new");
            let store = TaskStore::new(temp.path().join("history"));
            let record = record(&old);
            checkpoint(&old);
            store.save(&DownloadTask::restore(record.clone())).unwrap();
            let result = change_root(&store, vec![record], &new, mode);
            assert!(result.errors.is_empty(), "{:?}", result.errors);
            assert_eq!(result.records.len(), 1);
            assert!(!task_cache(&old).exists());
            let (tasks, error) = store.load();
            assert!(error.is_none());
            assert_eq!(tasks[0].record.options.cache_root, new);
            assert_eq!(
                tasks[0].record.layer_progress[0].downloaded,
                if mode == ChangeMode::Migrate { 30 } else { 0 }
            );
            assert_eq!(
                task_cache(&new).join("blobs/partial").exists(),
                mode == ChangeMode::Migrate
            );
            store.remove_record(42).unwrap();
            assert!(store.load().0.is_empty());
        }
    }
    #[test]
    fn failed_commit_or_busy_source_preserves_checkpoint_and_record_location() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("old");
        let new = temp.path().join("new");
        checkpoint(&old);
        let broken = temp.path().join("blocked");
        std::fs::write(&broken, b"file").unwrap();
        let store = TaskStore::new(broken);
        let result = change_root(&store, vec![record(&old)], &new, ChangeMode::Migrate);
        assert_eq!(result.errors.len(), 1);
        assert!(result.records.is_empty());
        assert!(task_cache(&old).join("blobs/partial").exists());
        assert!(!task_cache(&new).exists());
        let lease = CacheManager::new(old.clone()).task(42).unwrap();
        let result = change_root(&store, vec![record(&old)], &new, ChangeMode::Clear);
        assert_eq!(result.errors.len(), 1);
        assert!(result.records.is_empty());
        assert!(lease.path().join("blobs/partial").exists());
    }
    #[test]
    fn unusable_destination_cannot_clear_old_checkpoints() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("old");
        checkpoint(&old);
        let destination = temp.path().join("file");
        std::fs::write(&destination, b"keep").unwrap();
        let store = TaskStore::new(temp.path().join("history"));
        let result = change_root(&store, vec![record(&old)], &destination, ChangeMode::Clear);
        assert_eq!(result.errors.len(), 1);
        assert!(result.records.is_empty());
        assert!(task_cache(&old).join("blobs/partial").exists());
        assert_eq!(std::fs::read(destination).unwrap(), b"keep");
    }
    #[test]
    fn cache_cannot_contain_exports_or_overlap_history_config_or_other_cache_roots() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        assert!(validate_root(&root, &[], &[root.join("images")]).is_err());
        assert!(validate_root(&root.join("nested"), std::slice::from_ref(&root), &[]).is_err());
        assert!(validate_root(&data_root(), &[], &[]).is_err());
        assert!(validate_root(&config_dir(), &[], &[]).is_err());
        assert!(validate_root(&root, &[], &[temp.path().join("images")]).is_ok());
    }
    #[test]
    fn malformed_record_is_retained_for_recovery_without_hiding_valid_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let store = TaskStore::new(temp.path().join("history"));
        store
            .save_record(&record(&temp.path().join("cache")))
            .unwrap();
        std::fs::create_dir_all(store.directory(43)).unwrap();
        std::fs::write(store.directory(43).join("task.json"), b"broken").unwrap();
        let (tasks, error) = store.load();
        assert_eq!(tasks.len(), 1);
        assert!(error.unwrap().contains("43"));
        assert!(store.known_ids().unwrap().contains(&43));
        assert!(!store.directory(42).join("blobs").exists());
        assert!(!store.directory(42).join("manifests").exists());
    }
}
