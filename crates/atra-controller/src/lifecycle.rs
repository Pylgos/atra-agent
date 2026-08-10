use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow, bail};
use atra_protocol::{ApprovalId, ProcessHandle, ThreadId};
use tokio::sync::{Mutex, oneshot, watch};

use crate::Runner;

pub(super) struct TurnLifecycle {
    threads: Mutex<ThreadActivity>,
    approvals: Mutex<HashMap<ApprovalId, PendingApproval>>,
    next_approval_id: AtomicU64,
}

struct ThreadActivity {
    active: HashMap<ThreadId, Arc<ActiveTurn>>,
    deleting: HashSet<ThreadId>,
}

pub(super) struct ActiveTurn {
    cancel_requested: watch::Sender<bool>,
    cancellation: watch::Sender<Option<Result<(), String>>>,
    cancelling: AtomicBool,
    process: StdMutex<Option<(Arc<Runner>, ProcessHandle)>>,
}

pub(super) struct ApprovalDecision {
    pub(super) allowed: bool,
    pub(super) reason: Option<String>,
}

struct PendingApproval {
    thread_id: ThreadId,
    decision: oneshot::Sender<ApprovalDecision>,
}

impl TurnLifecycle {
    pub(super) fn new() -> Self {
        Self {
            threads: Mutex::new(ThreadActivity {
                active: HashMap::new(),
                deleting: HashSet::new(),
            }),
            approvals: Mutex::new(HashMap::new()),
            next_approval_id: AtomicU64::new(0),
        }
    }

    pub(super) async fn start(&self, thread_id: ThreadId) -> Result<Arc<ActiveTurn>> {
        let mut threads = self.threads.lock().await;
        if threads.active.contains_key(&thread_id) {
            bail!("thread already has an active turn");
        }
        if threads.deleting.contains(&thread_id) {
            bail!("thread is being deleted");
        }
        let turn = Arc::new(ActiveTurn::new());
        threads.active.insert(thread_id, Arc::clone(&turn));
        Ok(turn)
    }

    pub(super) async fn get(&self, thread_id: ThreadId) -> Option<Arc<ActiveTurn>> {
        self.threads.lock().await.active.get(&thread_id).cloned()
    }

    pub(super) async fn begin_cancellation(&self, thread_id: ThreadId) -> Option<Arc<ActiveTurn>> {
        let turn = self.threads.lock().await.active.get(&thread_id).cloned()?;
        turn.cancelling.store(true, Ordering::Release);
        Some(turn)
    }

    pub(super) async fn finish(&self, thread_id: ThreadId, turn: &Arc<ActiveTurn>) {
        let mut threads = self.threads.lock().await;
        if threads
            .active
            .get(&thread_id)
            .is_some_and(|current| Arc::ptr_eq(current, turn))
        {
            threads.active.remove(&thread_id);
        }
        drop(threads);
        self.clear_approvals(thread_id).await;
    }

    pub(super) async fn begin_delete(&self, thread_id: ThreadId) -> Result<()> {
        let mut threads = self.threads.lock().await;
        if threads.active.contains_key(&thread_id) {
            bail!("cannot delete a thread with an active turn");
        }
        if !threads.deleting.insert(thread_id) {
            bail!("thread deletion is already in progress");
        }
        Ok(())
    }

    pub(super) async fn finish_delete(&self, thread_id: ThreadId) {
        self.threads.lock().await.deleting.remove(&thread_id);
    }

    pub(super) async fn register_approval(
        &self,
        thread_id: ThreadId,
    ) -> Result<(ApprovalId, oneshot::Receiver<ApprovalDecision>)> {
        let threads = self.threads.lock().await;
        let turn = threads
            .active
            .get(&thread_id)
            .context("thread has no active turn")?;
        if turn.is_cancelling() {
            bail!("thread cancellation is in progress");
        }
        let approval_id = ApprovalId(self.next_approval_id.fetch_add(1, Ordering::Relaxed) + 1);
        let (decision, receiver) = oneshot::channel();
        self.approvals.lock().await.insert(
            approval_id,
            PendingApproval {
                thread_id,
                decision,
            },
        );
        Ok((approval_id, receiver))
    }

    pub(super) async fn resolve_approval(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<()> {
        let pending = self
            .approvals
            .lock()
            .await
            .remove(&approval_id)
            .with_context(|| format!("approval {approval_id} is not pending"))?;
        pending
            .decision
            .send(decision)
            .map_err(|_| anyhow!("turn ended before approval {approval_id} was resolved"))
    }

    pub(super) async fn clear_approvals(&self, thread_id: ThreadId) {
        self.approvals
            .lock()
            .await
            .retain(|_, approval| approval.thread_id != thread_id);
    }

    pub(super) async fn ensure_no_pending_approval(&self, thread_id: ThreadId) -> Result<()> {
        if self
            .approvals
            .lock()
            .await
            .values()
            .any(|approval| approval.thread_id == thread_id)
        {
            bail!("thread has a pending approval");
        }
        Ok(())
    }
}

impl ActiveTurn {
    fn new() -> Self {
        let (cancel_requested, _) = watch::channel(false);
        let (cancellation, _) = watch::channel(None);
        Self {
            cancel_requested,
            cancellation,
            cancelling: AtomicBool::new(false),
            process: StdMutex::new(None),
        }
    }

    pub(super) fn cancel_requested(&self) -> watch::Receiver<bool> {
        self.cancel_requested.subscribe()
    }

    pub(super) fn cancellation(&self) -> watch::Receiver<Option<Result<(), String>>> {
        self.cancellation.subscribe()
    }

    pub(super) fn is_cancelling(&self) -> bool {
        self.cancelling.load(Ordering::Acquire)
    }

    pub(super) fn set_process(&self, runner: Arc<Runner>, process_handle: ProcessHandle) {
        *self.process.lock().unwrap() = Some((runner, process_handle));
    }

    pub(super) fn clear_process(&self) {
        self.process.lock().unwrap().take();
    }

    pub(super) async fn request_cancellation(&self) -> Result<()> {
        let process = self.process.lock().unwrap().take();
        self.cancel_requested.send_replace(true);
        if let Some((runner, process_handle)) = process {
            runner.stop(process_handle).await?;
        }
        Ok(())
    }

    pub(super) fn complete_cancellation(&self, outcome: Result<(), String>) {
        self.cancellation.send_replace(Some(outcome));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deletion_and_turns_are_mutually_exclusive() {
        let lifecycle = TurnLifecycle::new();
        let thread_id = ThreadId(1);
        let turn = lifecycle.start(thread_id).await.unwrap();

        assert!(lifecycle.begin_delete(thread_id).await.is_err());

        lifecycle.finish(thread_id, &turn).await;
        lifecycle.begin_delete(thread_id).await.unwrap();
        assert!(lifecycle.start(thread_id).await.is_err());

        lifecycle.finish_delete(thread_id).await;
        assert!(lifecycle.start(thread_id).await.is_ok());
    }
}
