use super::task::{DownloadTask, Record};
use anyhow::{Context, Result, ensure};
use oci_pull::paths::{task_directory_name, task_id_from_directory_name};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct TaskStore {
    root: PathBuf,
}
impl TaskStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn directory(&self, id: u64) -> PathBuf {
        self.root.join(task_directory_name(id))
    }
    pub fn known_ids(&self) -> Result<std::collections::HashSet<u64>> {
        let mut ids = std::collections::HashSet::new();
        if !self.root.exists() {
            return Ok(ids);
        }
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.path().join("task.json").exists()
                && let Some(id) = task_id_from_directory_name(&entry.file_name().to_string_lossy())
            {
                ids.insert(id);
            }
        }
        Ok(ids)
    }
    pub fn save(&self, task: &DownloadTask) -> Result<()> {
        self.save_record(&task.record)
    }
    pub fn save_record(&self, record: &Record) -> Result<()> {
        let directory = self.directory(record.id);
        write_atomic(
            &directory.join("task.json"),
            &serde_json::to_vec_pretty(record)?,
        )
    }
    pub fn remove_record(&self, id: u64) -> Result<()> {
        match std::fs::remove_file(self.directory(id).join("task.json")) {
            Ok(()) => {
                let _ = std::fs::remove_dir(self.directory(id));
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn load(&self) -> (Vec<DownloadTask>, Option<String>) {
        match self.read() {
            Ok(result) => result,
            Err(error) => (Vec::new(), Some(format!("读取任务记录失败：{error:#}"))),
        }
    }
    fn read(&self) -> Result<(Vec<DownloadTask>, Option<String>)> {
        std::fs::create_dir_all(&self.root)?;
        let mut tasks = BTreeMap::new();
        let mut errors = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(id) = task_id_from_directory_name(&entry.file_name().to_string_lossy()) else {
                continue;
            };
            let path = entry.path().join("task.json");
            if !path.exists() {
                continue;
            }
            let record = (|| -> Result<Record> {
                let record: Record = serde_json::from_slice(&std::fs::read(path)?)?;
                ensure!(record.id == id, "任务 ID 与目录不一致");
                Ok(record)
            })();
            match record {
                Ok(record) => {
                    tasks.insert(id, DownloadTask::restore(record));
                }
                Err(e) => errors.push(format!("任务 {id}：{e:#}")),
            }
        }
        Ok((
            tasks.into_values().collect(),
            (!errors.is_empty()).then(|| errors.join("；")),
        ))
    }
}
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().context("任务目录无效")?;
    std::fs::create_dir_all(directory)?;
    let mut temp = tempfile::NamedTempFile::new_in(directory)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
