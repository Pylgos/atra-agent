use anyhow::Result;
use atra_client::{
    CheckpointSubscription, ControllerSubscription, ProcessSubscription, ThreadSubscription,
};
use atra_protocol::{
    CheckpointState, ControllerChange, ControllerState, ProcessChange, ProcessState, ThreadChange,
    ThreadState,
};

pub(crate) enum ControllerSync {
    Live(ControllerSubscription),
    #[cfg(test)]
    Snapshot(ControllerState),
}

impl ControllerSync {
    pub(crate) fn state(&self) -> &ControllerState {
        match self {
            Self::Live(subscription) => subscription.state(),
            #[cfg(test)]
            Self::Snapshot(state) => state,
        }
    }

    pub(crate) async fn receive(&mut self) -> Result<ControllerChange> {
        match self {
            Self::Live(subscription) => subscription.receive().await,
            #[cfg(test)]
            Self::Snapshot(_) => std::future::pending().await,
        }
    }
}

pub(crate) enum ThreadSync {
    Live(ThreadSubscription),
    #[cfg(test)]
    Snapshot(ThreadState),
}

impl ThreadSync {
    pub(crate) fn state(&self) -> &ThreadState {
        match self {
            Self::Live(subscription) => subscription.state(),
            #[cfg(test)]
            Self::Snapshot(state) => state,
        }
    }

    pub(crate) async fn receive(&mut self) -> Result<ThreadChange> {
        match self {
            Self::Live(subscription) => subscription.receive().await,
            #[cfg(test)]
            Self::Snapshot(_) => std::future::pending().await,
        }
    }
}

impl From<ControllerSubscription> for ControllerSync {
    fn from(subscription: ControllerSubscription) -> Self {
        Self::Live(subscription)
    }
}

impl From<ThreadSubscription> for ThreadSync {
    fn from(subscription: ThreadSubscription) -> Self {
        Self::Live(subscription)
    }
}

pub(crate) struct CheckpointSync(CheckpointSubscription);

impl CheckpointSync {
    pub(crate) fn state(&self) -> &CheckpointState {
        self.0.state()
    }
}

impl From<CheckpointSubscription> for CheckpointSync {
    fn from(subscription: CheckpointSubscription) -> Self {
        Self(subscription)
    }
}

pub(crate) struct ProcessSync(ProcessSubscription);

impl ProcessSync {
    pub(crate) fn state(&self) -> &ProcessState {
        self.0.state()
    }

    pub(crate) async fn receive(&mut self) -> Result<ProcessChange> {
        self.0.receive().await
    }
}

impl From<ProcessSubscription> for ProcessSync {
    fn from(subscription: ProcessSubscription) -> Self {
        Self(subscription)
    }
}
