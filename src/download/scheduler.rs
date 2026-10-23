use crate::model::check_cancel;
use anyhow::Result;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

// One scheduler per image task. Every layer submits its unfinished pieces here;
// a released slot rotates to the next layer with work, with no per-layer limit.
pub(crate) struct Scheduler {
    chunk_size: AtomicU64,
    queue: Mutex<Queue>,
}
#[derive(Default)]
struct Queue {
    limit: usize,
    active: usize,
    layers: VecDeque<(String, VecDeque<Arc<Waiting>>)>,
}
#[derive(Default)]
struct Waiting {
    granted: AtomicBool,
    ready: Notify,
}
pub(super) struct Slot {
    scheduler: Arc<Scheduler>,
    waiting: Arc<Waiting>,
}
impl Queue {
    fn dispatch(&mut self) {
        while self.active < self.limit {
            let Some((layer, mut pieces)) = self.layers.pop_front() else {
                break;
            };
            let waiting = pieces.pop_front().unwrap();
            self.active += 1;
            waiting.granted.store(true, Ordering::Release);
            waiting.ready.notify_one();
            if !pieces.is_empty() {
                self.layers.push_back((layer, pieces));
            }
        }
    }
}
impl Scheduler {
    pub fn new(limit: usize, chunk_size: u64) -> Arc<Self> {
        Arc::new(Self {
            chunk_size: AtomicU64::new(chunk_size),
            queue: Mutex::new(Queue {
                limit: limit.max(1),
                ..Queue::default()
            }),
        })
    }
    pub fn configure(&self, limit: usize, chunk_size: u64) {
        self.chunk_size.store(chunk_size, Ordering::Release);
        let mut queue = self.queue.lock().unwrap();
        queue.limit = limit.max(1);
        queue.dispatch();
    }
    pub fn chunk_size(&self) -> u64 {
        self.chunk_size.load(Ordering::Acquire)
    }
    fn reserve(self: &Arc<Self>, layer: &str) -> Slot {
        let waiting = Arc::new(Waiting::default());
        let mut queue = self.queue.lock().unwrap();
        if let Some((_, pieces)) = queue.layers.iter_mut().find(|(name, _)| name == layer) {
            pieces.push_back(waiting.clone());
        } else {
            queue
                .layers
                .push_back((layer.to_owned(), VecDeque::from([waiting.clone()])));
        }
        queue.dispatch();
        Slot {
            scheduler: self.clone(),
            waiting,
        }
    }
    pub(super) async fn acquire(
        self: &Arc<Self>,
        layer: &str,
        cancel: &CancellationToken,
    ) -> Result<Slot> {
        check_cancel(cancel)?;
        let slot = self.reserve(layer);
        loop {
            check_cancel(cancel)?;
            if slot.waiting.granted.load(Ordering::Acquire) {
                return Ok(slot);
            }
            tokio::select! {
                _ = cancel.cancelled() => check_cancel(cancel)?,
                _ = slot.waiting.ready.notified() => {}
            }
        }
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        let mut queue = self.scheduler.queue.lock().unwrap();
        if self.waiting.granted.swap(false, Ordering::AcqRel) {
            queue.active -= 1;
        } else {
            for (_, pieces) in &mut queue.layers {
                pieces.retain(|w| !Arc::ptr_eq(w, &self.waiting));
            }
            queue.layers.retain(|(_, pieces)| !pieces.is_empty());
        }
        queue.dispatch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_increase_dispatches_now_and_decrease_drains_without_new_work() {
        let scheduler = Scheduler::new(1, 8 * 1024 * 1024);
        let a = scheduler.reserve("a");
        let b = scheduler.reserve("b");
        let c = scheduler.reserve("c");
        let d = scheduler.reserve("d");
        scheduler.configure(3, 1024 * 1024);
        assert!(b.waiting.granted.load(Ordering::Acquire));
        assert!(c.waiting.granted.load(Ordering::Acquire));
        assert_eq!(scheduler.chunk_size(), 1024 * 1024);
        scheduler.configure(1, 2 * 1024 * 1024);
        drop(a);
        assert!(!d.waiting.granted.load(Ordering::Acquire));
        drop(b);
        assert!(!d.waiting.granted.load(Ordering::Acquire));
        drop(c);
        assert!(d.waiting.granted.load(Ordering::Acquire));
    }
    #[test]
    fn task_slots_rotate_between_layers_and_reassign_all_capacity() {
        let scheduler = Scheduler::new(2, 1024 * 1024);
        let a = scheduler.reserve("large");
        let b = scheduler.reserve("large");
        let c = scheduler.reserve("large");
        let d = scheduler.reserve("large");
        let small = scheduler.reserve("small");
        assert!(!small.waiting.granted.load(Ordering::Acquire));
        drop(a);
        assert!(c.waiting.granted.load(Ordering::Acquire));
        drop(b);
        assert!(small.waiting.granted.load(Ordering::Acquire));
        assert!(!d.waiting.granted.load(Ordering::Acquire));
        drop(small);
        assert!(d.waiting.granted.load(Ordering::Acquire));
        assert_eq!(scheduler.queue.lock().unwrap().active, 2);
        drop((c, d));
        assert_eq!(scheduler.queue.lock().unwrap().active, 0);
    }
    #[tokio::test]
    async fn cancelled_waiters_do_not_consume_or_leak_task_slots() {
        let scheduler = Scheduler::new(1, 1024 * 1024);
        let busy = scheduler.reserve("a");
        let cancel = CancellationToken::new();
        let future = scheduler.acquire("b", &cancel);
        tokio::pin!(future);
        tokio::select! { _ = &mut future => panic!("slot occupied"), _ = tokio::task::yield_now() => {} }
        cancel.cancel();
        assert!(future.await.is_err());
        assert!(scheduler.queue.lock().unwrap().layers.is_empty());
        drop(busy);
        let next = scheduler
            .acquire("c", &CancellationToken::new())
            .await
            .unwrap();
        drop(next);
        assert_eq!(scheduler.queue.lock().unwrap().active, 0);
    }
}
