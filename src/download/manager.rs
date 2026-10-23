use super::scheduler::Scheduler;
use crate::model::DownloadSettings;
use anyhow::Result;
use std::sync::{Arc, Mutex, Weak};

/// Live download policy shared by the UI and running image tasks. This module
/// has no dependency on GPUI, registries, HTTP clients or cache files.
pub struct DownloadManager {
    state: Mutex<State>,
}

struct State {
    settings: DownloadSettings,
    tasks: Vec<Weak<Scheduler>>,
}
impl DownloadManager {
    pub fn new(settings: DownloadSettings) -> Result<Arc<Self>> {
        settings.validate()?;
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                settings,
                tasks: Vec::new(),
            }),
        }))
    }
    /// Wake queued work immediately on increases. On decreases, existing
    /// transfers finish their current piece and no excess replacements start.
    pub fn configure(&self, settings: DownloadSettings) -> Result<()> {
        settings.validate()?;
        let mut state = self.state.lock().unwrap();
        state.settings = settings;
        state.tasks.retain(|task| {
            if let Some(task) = task.upgrade() {
                task.configure(settings.concurrency, settings.chunk_size_mib * 1024 * 1024);
                true
            } else {
                false
            }
        });
        Ok(())
    }
    pub fn settings(&self) -> DownloadSettings {
        self.state.lock().unwrap().settings
    }
    pub(crate) fn task(&self) -> Arc<Scheduler> {
        let mut state = self.state.lock().unwrap();
        let scheduler = Scheduler::new(
            state.settings.concurrency,
            state.settings.chunk_size_mib * 1024 * 1024,
        );
        state.tasks.retain(|task| task.strong_count() > 0);
        state.tasks.push(Arc::downgrade(&scheduler));
        scheduler
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_reaches_running_and_future_tasks_and_rejects_invalid_updates() {
        let manager = DownloadManager::new(DownloadSettings::default()).unwrap();
        let first = manager.task();
        let second = manager.task();
        let next = DownloadSettings {
            concurrency: 16,
            chunk_size_mib: 2,
        };
        manager.configure(next).unwrap();
        assert_eq!(first.chunk_size(), 2 * 1024 * 1024);
        assert_eq!(second.chunk_size(), first.chunk_size());
        assert_eq!(manager.task().chunk_size(), first.chunk_size());
        assert!(
            manager
                .configure(DownloadSettings {
                    concurrency: 0,
                    chunk_size_mib: 0
                })
                .is_err()
        );
        assert_eq!(manager.settings(), next);
    }
}
