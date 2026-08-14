use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow, bail};
use atra_protocol::{InteractionId, ProcessHandle, QuestionAnswer, ThreadId};
use tokio::sync::{Mutex, oneshot, watch};

use crate::Runner;

pub(super) struct TurnLifecycle {
    threads: Mutex<ThreadActivity>,
    interactions: Mutex<HashMap<InteractionId, PendingInteraction>>,
    next_interaction_id: AtomicU64,
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

struct PendingInteraction {
    thread_id: ThreadId,
    waiter: InteractionWaiter,
}

pub(super) enum InteractionWaiter {
    Approval(oneshot::Sender<ApprovalDecision>),
    Questions(oneshot::Sender<Vec<QuestionAnswer>>),
}

impl TurnLifecycle {
    pub(super) fn new() -> Self {
        Self {
            threads: Mutex::new(ThreadActivity {
                active: HashMap::new(),
                deleting: HashSet::new(),
            }),
            interactions: Mutex::new(HashMap::new()),
            next_interaction_id: AtomicU64::new(0),
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
        self.clear_interactions(thread_id).await;
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
    ) -> Result<(InteractionId, oneshot::Receiver<ApprovalDecision>)> {
        let id = self.next_interaction(thread_id).await?;
        let (decision, receiver) = oneshot::channel();
        self.interactions.lock().await.insert(
            id,
            PendingInteraction {
                thread_id,
                waiter: InteractionWaiter::Approval(decision),
            },
        );
        Ok((id, receiver))
    }

    pub(super) async fn register_question(
        &self,
        thread_id: ThreadId,
    ) -> Result<(InteractionId, oneshot::Receiver<Vec<QuestionAnswer>>)> {
        let id = self.next_interaction(thread_id).await?;
        let (answers, receiver) = oneshot::channel();
        self.interactions.lock().await.insert(
            id,
            PendingInteraction {
                thread_id,
                waiter: InteractionWaiter::Questions(answers),
            },
        );
        Ok((id, receiver))
    }

    async fn next_interaction(&self, thread_id: ThreadId) -> Result<InteractionId> {
        let threads = self.threads.lock().await;
        let turn = threads
            .active
            .get(&thread_id)
            .context("thread has no active turn")?;
        if turn.is_cancelling() {
            bail!("thread cancellation is in progress");
        }
        Ok(InteractionId(
            self.next_interaction_id.fetch_add(1, Ordering::Relaxed) + 1,
        ))
    }

    pub(super) async fn claim_approval(&self, id: InteractionId) -> Result<InteractionWaiter> {
        self.claim(
            id,
            |waiter| matches!(waiter, InteractionWaiter::Approval(_)),
            "approval",
        )
        .await
    }

    pub(super) async fn claim_questions(&self, id: InteractionId) -> Result<InteractionWaiter> {
        self.claim(
            id,
            |waiter| matches!(waiter, InteractionWaiter::Questions(_)),
            "question request",
        )
        .await
    }

    async fn claim(
        &self,
        id: InteractionId,
        expected: impl FnOnce(&InteractionWaiter) -> bool,
        kind: &str,
    ) -> Result<InteractionWaiter> {
        let mut interactions = self.interactions.lock().await;
        let pending = interactions
            .get(&id)
            .with_context(|| format!("interaction {id} is not pending"))?;
        if !expected(&pending.waiter) {
            bail!("interaction {id} is not a pending {kind}");
        }
        Ok(interactions.remove(&id).unwrap().waiter)
    }

    pub(super) async fn clear_interactions(&self, thread_id: ThreadId) {
        self.interactions
            .lock()
            .await
            .retain(|_, interaction| interaction.thread_id != thread_id);
    }

    pub(super) async fn ensure_no_pending_interaction(&self, thread_id: ThreadId) -> Result<()> {
        if self
            .interactions
            .lock()
            .await
            .values()
            .any(|interaction| interaction.thread_id == thread_id)
        {
            bail!("thread has a pending interaction");
        }
        Ok(())
    }
}

impl InteractionWaiter {
    pub(super) fn resolve_approval(
        self,
        id: InteractionId,
        allowed: bool,
        reason: Option<String>,
    ) -> Result<()> {
        let Self::Approval(decision) = self else {
            bail!("interaction {id} is not an approval");
        };
        decision
            .send(ApprovalDecision { allowed, reason })
            .map_err(|_| anyhow!("turn ended before approval {id} was resolved"))
    }

    pub(super) fn resolve_questions(
        self,
        id: InteractionId,
        answers: Vec<QuestionAnswer>,
    ) -> Result<()> {
        let Self::Questions(sender) = self else {
            bail!("interaction {id} is not a question request");
        };
        sender
            .send(answers)
            .map_err(|_| anyhow!("turn ended before question request {id} was resolved"))
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

    #[tokio::test]
    async fn dropping_a_claimed_approval_releases_the_waiting_turn() {
        let lifecycle = TurnLifecycle::new();
        let thread_id = ThreadId(1);
        lifecycle.start(thread_id).await.unwrap();
        let (approval_id, waiting) = lifecycle.register_approval(thread_id).await.unwrap();

        let claimed = lifecycle.claim_approval(approval_id).await.unwrap();
        drop(claimed);

        assert!(waiting.await.is_err());
        assert!(lifecycle.claim_approval(approval_id).await.is_err());
    }

    #[tokio::test]
    async fn resolving_a_question_returns_answers_to_the_waiting_turn() {
        let lifecycle = TurnLifecycle::new();
        let thread_id = ThreadId(1);
        lifecycle.start(thread_id).await.unwrap();
        let (request_id, waiting) = lifecycle.register_question(thread_id).await.unwrap();
        let claimed = lifecycle.claim_questions(request_id).await.unwrap();
        let answers = vec![QuestionAnswer {
            selected_option: Some("A".to_owned()),
            note: "because".to_owned(),
        }];

        claimed
            .resolve_questions(request_id, answers.clone())
            .expect("question should resolve");

        assert_eq!(waiting.await.unwrap(), answers);
        assert!(lifecycle.claim_questions(request_id).await.is_err());
    }
}
