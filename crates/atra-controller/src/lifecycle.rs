use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow, bail};
use atra_protocol::{InteractionId, ProcessHandle, QuestionAnswer, ThreadId};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::Runner;

pub(super) struct TurnLifecycle {
    threads: Arc<StdMutex<ThreadActivity>>,
    next_interaction_id: AtomicU64,
}

struct ThreadActivity {
    active: HashMap<ThreadId, Arc<ActiveTurn>>,
    observation_pins: HashMap<ThreadId, usize>,
    interactions: HashMap<InteractionId, PendingInteraction>,
}

type PinnedTurns = (Vec<ObservationPin>, Vec<Option<Arc<ActiveTurn>>>);

pub(super) struct ActiveTurn {
    cancellation: CancellationToken,
    process: StdMutex<Option<(Arc<Runner>, ProcessHandle)>>,
    finished: watch::Sender<bool>,
    agent_root: Option<ThreadId>,
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
            threads: Arc::new(StdMutex::new(ThreadActivity {
                active: HashMap::new(),
                observation_pins: HashMap::new(),
                interactions: HashMap::new(),
            })),
            next_interaction_id: AtomicU64::new(0),
        }
    }

    pub(super) fn start(&self, thread_id: ThreadId) -> Result<Arc<ActiveTurn>> {
        let mut threads = self.threads.lock().unwrap();
        if threads.active.contains_key(&thread_id) {
            bail!("thread already has an active turn");
        }
        if threads.observation_pins.contains_key(&thread_id) {
            bail!("thread is being observed");
        }
        let turn = Arc::new(ActiveTurn::new(None));
        threads.active.insert(thread_id, Arc::clone(&turn));
        Ok(turn)
    }

    pub(super) fn start_agent(
        &self,
        thread_id: ThreadId,
        root: ThreadId,
    ) -> Result<Arc<ActiveTurn>> {
        let mut threads = self.threads.lock().unwrap();
        if threads.active.contains_key(&thread_id) {
            bail!("thread already has an active turn");
        }
        if threads.observation_pins.contains_key(&thread_id) {
            bail!("thread is unavailable");
        }
        if threads
            .active
            .values()
            .filter(|turn| turn.agent_root == Some(root))
            .count()
            >= 8
        {
            bail!("at most eight descendant agent turns may run concurrently");
        }
        let turn = Arc::new(ActiveTurn::new(Some(root)));
        threads.active.insert(thread_id, Arc::clone(&turn));
        Ok(turn)
    }

    pub(super) fn get(&self, thread_id: ThreadId) -> Option<Arc<ActiveTurn>> {
        self.threads.lock().unwrap().active.get(&thread_id).cloned()
    }

    pub(super) fn ensure_mutable(&self, thread_id: ThreadId) -> Result<()> {
        let threads = self.threads.lock().unwrap();
        if threads.active.contains_key(&thread_id) {
            bail!("thread has an active turn");
        }
        if threads.observation_pins.contains_key(&thread_id) {
            bail!("thread is being observed");
        }
        Ok(())
    }

    pub(super) fn begin_cancellation(&self, thread_id: ThreadId) -> Option<Arc<ActiveTurn>> {
        let turn = self
            .threads
            .lock()
            .unwrap()
            .active
            .get(&thread_id)
            .cloned()?;
        turn.signal_cancellation();
        Some(turn)
    }

    pub(super) fn begin_cancellation_many(&self, thread_ids: &[ThreadId]) -> Vec<ThreadId> {
        let threads = self.threads.lock().unwrap();
        thread_ids
            .iter()
            .filter_map(|thread_id| {
                let turn = threads.active.get(thread_id)?.clone();
                turn.signal_cancellation();
                Some(*thread_id)
            })
            .collect()
    }

    pub(super) fn finish(&self, thread_id: ThreadId) {
        let mut threads = self.threads.lock().unwrap();
        let turn = threads.active.remove(&thread_id);
        threads
            .interactions
            .retain(|_, interaction| interaction.thread_id != thread_id);
        if let Some(turn) = turn {
            turn.finished.send_replace(true);
        }
    }

    pub(super) fn ensure_deletable(&self, thread_ids: &[ThreadId]) -> Result<()> {
        let threads = self.threads.lock().unwrap();
        for thread_id in thread_ids {
            if threads.active.contains_key(thread_id) {
                bail!("cannot delete thread {thread_id} with an active turn");
            }
            if threads.observation_pins.contains_key(thread_id) {
                bail!("thread {thread_id} is being observed");
            }
        }
        Ok(())
    }

    pub(super) fn pin_many(&self, thread_ids: &[ThreadId]) -> Result<PinnedTurns> {
        let mut threads = self.threads.lock().unwrap();
        let active = thread_ids
            .iter()
            .map(|thread_id| threads.active.get(thread_id).cloned())
            .collect();
        for thread_id in thread_ids {
            *threads.observation_pins.entry(*thread_id).or_default() += 1;
        }
        let pins = thread_ids
            .iter()
            .map(|thread_id| ObservationPin {
                threads: Arc::clone(&self.threads),
                thread_id: *thread_id,
                released: false,
            })
            .collect();
        Ok((pins, active))
    }

    pub(super) fn register_approval(
        &self,
        thread_id: ThreadId,
    ) -> Result<(InteractionId, oneshot::Receiver<ApprovalDecision>)> {
        let (decision, receiver) = oneshot::channel();
        let mut threads = self.threads.lock().unwrap();
        self.current_interaction_turn(&threads, thread_id)?;
        let id = self.next_interaction();
        threads.interactions.insert(
            id,
            PendingInteraction {
                thread_id,
                waiter: InteractionWaiter::Approval(decision),
            },
        );
        Ok((id, receiver))
    }

    pub(super) fn register_question(
        &self,
        thread_id: ThreadId,
    ) -> Result<(InteractionId, oneshot::Receiver<Vec<QuestionAnswer>>)> {
        let (answers, receiver) = oneshot::channel();
        let mut threads = self.threads.lock().unwrap();
        self.current_interaction_turn(&threads, thread_id)?;
        let id = self.next_interaction();
        threads.interactions.insert(
            id,
            PendingInteraction {
                thread_id,
                waiter: InteractionWaiter::Questions(answers),
            },
        );
        Ok((id, receiver))
    }

    fn current_interaction_turn(
        &self,
        threads: &ThreadActivity,
        thread_id: ThreadId,
    ) -> Result<Arc<ActiveTurn>> {
        let turn = threads
            .active
            .get(&thread_id)
            .context("thread has no active turn")?;
        if turn.is_cancelling() {
            bail!("thread cancellation is in progress");
        }
        Ok(Arc::clone(turn))
    }

    fn next_interaction(&self) -> InteractionId {
        InteractionId(self.next_interaction_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub(super) fn claim_approval(
        &self,
        id: InteractionId,
    ) -> Result<(ThreadId, InteractionWaiter)> {
        self.claim(
            id,
            |waiter| matches!(waiter, InteractionWaiter::Approval(_)),
            "approval",
        )
    }

    pub(super) fn claim_questions(
        &self,
        id: InteractionId,
    ) -> Result<(ThreadId, InteractionWaiter)> {
        self.claim(
            id,
            |waiter| matches!(waiter, InteractionWaiter::Questions(_)),
            "question request",
        )
    }

    fn claim(
        &self,
        id: InteractionId,
        expected: impl FnOnce(&InteractionWaiter) -> bool,
        kind: &str,
    ) -> Result<(ThreadId, InteractionWaiter)> {
        let mut threads = self.threads.lock().unwrap();
        let pending = threads
            .interactions
            .get(&id)
            .with_context(|| format!("interaction {id} is not pending"))?;
        if !expected(&pending.waiter) {
            bail!("interaction {id} is not a pending {kind}");
        }
        let pending = threads.interactions.remove(&id).unwrap();
        Ok((pending.thread_id, pending.waiter))
    }

    pub(super) fn clear_interactions(&self, thread_id: ThreadId) {
        self.threads
            .lock()
            .unwrap()
            .interactions
            .retain(|_, interaction| interaction.thread_id != thread_id);
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
    fn new(agent_root: Option<ThreadId>) -> Self {
        let (finished, _) = watch::channel(false);
        Self {
            cancellation: CancellationToken::new(),
            process: StdMutex::new(None),
            finished,
            agent_root,
        }
    }

    pub(super) async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub(super) fn is_cancelling(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub(super) fn set_process(&self, runner: Arc<Runner>, process_handle: ProcessHandle) {
        *self.process.lock().unwrap() = Some((runner, process_handle));
    }

    pub(super) fn clear_process(&self) {
        self.process.lock().unwrap().take();
    }

    pub(super) async fn request_cancellation(&self) -> Result<()> {
        let process = self.process.lock().unwrap().take();
        if let Some((runner, process_handle)) = process {
            runner.stop(process_handle).await?;
        }
        Ok(())
    }

    pub(super) fn signal_cancellation(&self) {
        self.cancellation.cancel();
    }

    pub(super) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.subscribe()
    }
}

pub(super) struct ObservationPin {
    threads: Arc<StdMutex<ThreadActivity>>,
    thread_id: ThreadId,
    released: bool,
}

impl ObservationPin {
    pub(super) fn release(mut self) {
        release_pin(&self.threads, self.thread_id);
        self.released = true;
    }
}

impl Drop for ObservationPin {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        release_pin(&self.threads, self.thread_id);
    }
}

fn release_pin(threads: &StdMutex<ThreadActivity>, thread_id: ThreadId) {
    let mut threads = threads.lock().unwrap();
    if let Some(count) = threads.observation_pins.get_mut(&thread_id) {
        *count -= 1;
        if *count == 0 {
            threads.observation_pins.remove(&thread_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn active_turn_blocks_deletion() {
        let lifecycle = TurnLifecycle::new();
        let thread_id = ThreadId(1);
        lifecycle.start(thread_id).unwrap();

        assert!(lifecycle.ensure_deletable(&[thread_id]).is_err());

        lifecycle.finish(thread_id);
        assert!(lifecycle.ensure_deletable(&[thread_id]).is_ok());
    }

    #[tokio::test]
    async fn dropping_a_claimed_approval_releases_the_waiting_turn() {
        let lifecycle = TurnLifecycle::new();
        let thread_id = ThreadId(1);
        lifecycle.start(thread_id).unwrap();
        let (approval_id, waiting) = lifecycle.register_approval(thread_id).unwrap();

        let claimed = lifecycle.claim_approval(approval_id).unwrap();
        drop(claimed);

        assert!(waiting.await.is_err());
        assert!(lifecycle.claim_approval(approval_id).is_err());
    }

    #[tokio::test]
    async fn resolving_a_question_returns_answers_to_the_waiting_turn() {
        let lifecycle = TurnLifecycle::new();
        let thread_id = ThreadId(1);
        lifecycle.start(thread_id).unwrap();
        let (request_id, waiting) = lifecycle.register_question(thread_id).unwrap();
        let (_, claimed) = lifecycle.claim_questions(request_id).unwrap();
        let answers = vec![QuestionAnswer {
            selected_option: Some("A".to_owned()),
            note: "because".to_owned(),
        }];

        claimed
            .resolve_questions(request_id, answers.clone())
            .expect("question should resolve");

        assert_eq!(waiting.await.unwrap(), answers);
        assert!(lifecycle.claim_questions(request_id).is_err());
    }

    #[tokio::test]
    async fn finished_turn_cannot_register_an_interaction() {
        let lifecycle = TurnLifecycle::new();
        let thread_id = ThreadId(1);
        lifecycle.start(thread_id).unwrap();
        lifecycle.finish(thread_id);

        assert!(lifecycle.register_question(thread_id).is_err());
        assert!(lifecycle.register_approval(thread_id).is_err());
    }

    #[tokio::test]
    async fn cancelling_turn_cannot_register_a_new_interaction() {
        let lifecycle = TurnLifecycle::new();
        let thread_id = ThreadId(1);
        lifecycle.start(thread_id).unwrap();
        lifecycle.begin_cancellation(thread_id).unwrap();

        assert!(lifecycle.register_question(thread_id).is_err());
        assert!(lifecycle.register_approval(thread_id).is_err());
    }

    #[tokio::test]
    async fn cancellation_is_observed_when_requested_before_waiting() {
        let lifecycle = TurnLifecycle::new();
        let thread_id = ThreadId(1);
        let turn = lifecycle.start(thread_id).unwrap();
        lifecycle.begin_cancellation(thread_id).unwrap();

        tokio::time::timeout(std::time::Duration::from_millis(50), turn.cancelled())
            .await
            .expect("latched cancellation was not observed");
    }

    #[tokio::test]
    async fn agent_concurrency_limit_is_atomic_per_root() {
        let lifecycle = TurnLifecycle::new();
        let root = ThreadId(1);
        for id in 2..=9 {
            lifecycle.start_agent(ThreadId(id), root).unwrap();
        }
        assert!(lifecycle.start_agent(ThreadId(10), root).is_err());
        assert!(lifecycle.start_agent(ThreadId(10), ThreadId(99)).is_ok());
    }

    #[tokio::test]
    async fn observation_pin_blocks_turns_and_deletion_until_dropped() {
        let lifecycle = TurnLifecycle::new();
        let id = ThreadId(1);
        let (mut pins, _) = lifecycle.pin_many(&[id]).unwrap();
        let pin = pins.pop().unwrap();
        assert!(lifecycle.start(id).is_err());
        assert!(lifecycle.ensure_deletable(&[id]).is_err());
        assert!(lifecycle.ensure_mutable(id).is_err());
        drop(pin);
        tokio::task::yield_now().await;
        assert!(lifecycle.ensure_mutable(id).is_ok());
        assert!(lifecycle.start(id).is_ok());
    }

    #[tokio::test]
    async fn multi_thread_observation_captures_turns_and_releases_every_pin() {
        let lifecycle = TurnLifecycle::new();
        let first = ThreadId(1);
        let second = ThreadId(2);
        lifecycle.start(first).unwrap();

        let (pins, captured) = lifecycle.pin_many(&[first, second]).unwrap();
        assert!(captured[0].is_some());
        assert!(captured[1].is_none());
        assert!(lifecycle.ensure_deletable(&[first, second]).is_err());

        for pin in pins {
            pin.release();
        }
        lifecycle.finish(first);
        assert!(lifecycle.ensure_deletable(&[first, second]).is_ok());
    }

    #[tokio::test]
    async fn releasing_completed_target_pin_does_not_release_other_wait_target() {
        let lifecycle = TurnLifecycle::new();
        let first = ThreadId(1);
        let second = ThreadId(2);
        lifecycle.start(first).unwrap();
        lifecycle.start(second).unwrap();
        let (mut pins, captured) = lifecycle.pin_many(&[first, second]).unwrap();

        lifecycle.finish(first);
        pins.remove(0).release();
        let later = lifecycle.start(first).unwrap();

        assert!(*captured[0].as_ref().unwrap().finished().borrow());
        assert!(!*later.finished().borrow());
        assert!(lifecycle.start(second).is_err());
        assert!(lifecycle.ensure_deletable(&[second]).is_err());

        pins.pop().unwrap().release();
        lifecycle.finish(second);
    }
}
