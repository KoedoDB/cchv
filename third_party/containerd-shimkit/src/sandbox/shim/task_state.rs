use crate::sandbox::Error::FailedPrecondition;
use crate::sandbox::Result;

#[derive(Debug, Clone, Copy)]
pub(super) enum TaskState {
    Created,
    Starting,
    Started,
    Exited,
    Deleting,
}

impl TaskState {
    pub fn start(&mut self) -> Result<()> {
        *self = match self {
            Self::Created => Ok(Self::Starting),
            _ => state_transition_error(*self, Self::Starting),
        }?;
        Ok(())
    }

    pub fn kill(&mut self) -> Result<()> {
        *self = match self {
            Self::Started => Ok(Self::Started),
            // Allow kill on already-exited tasks — containerd's CRI plugin calls
            // StopPodSandbox → Kill after the task has already exited (e.g. liveness
            // probe restart). Rejecting this transition causes an infinite retry loop
            // that bricks the node.
            Self::Exited => Ok(Self::Exited),
            _ => state_transition_error(*self, "Killing"),
        }?;
        Ok(())
    }

    pub fn delete(&mut self) -> Result<()> {
        *self = match self {
            Self::Created | Self::Exited => Ok(Self::Deleting),
            // Allow idempotent delete — containerd may retry delete after a
            // TaskExit event races with an explicit delete call.
            Self::Deleting => Ok(Self::Deleting),
            _ => state_transition_error(*self, Self::Deleting),
        }?;
        Ok(())
    }

    pub fn started(&mut self) -> Result<()> {
        *self = match self {
            Self::Starting => Ok(Self::Started),
            _ => state_transition_error(*self, Self::Started),
        }?;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        *self = match self {
            Self::Started | Self::Starting => Ok(Self::Exited),
            // This is for potential failure cases where we want delete to be able to be retried.
            Self::Deleting => Ok(Self::Exited),
            _ => state_transition_error(*self, Self::Exited),
        }?;
        Ok(())
    }
}

fn state_transition_error<T>(from: impl std::fmt::Debug, to: impl std::fmt::Debug) -> Result<T> {
    Err(FailedPrecondition(format!(
        "invalid state transition: {from:?} => {to:?}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_from_started_stays_started() {
        let mut s = TaskState::Started;
        s.kill().expect("kill from Started should succeed");
        assert!(matches!(s, TaskState::Started));
    }

    #[test]
    fn kill_from_exited_stays_exited() {
        let mut s = TaskState::Exited;
        s.kill().expect("kill from Exited should succeed (no-op)");
        assert!(matches!(s, TaskState::Exited));
    }

    #[test]
    fn kill_from_created_fails() {
        let mut s = TaskState::Created;
        assert!(s.kill().is_err());
    }

    #[test]
    fn full_lifecycle_with_kill_after_exit() {
        let mut s = TaskState::Created;
        s.start().unwrap();
        assert!(matches!(s, TaskState::Starting));
        s.started().unwrap();
        assert!(matches!(s, TaskState::Started));
        s.kill().unwrap();
        assert!(matches!(s, TaskState::Started));
        s.stop().unwrap();
        assert!(matches!(s, TaskState::Exited));
        // This is the critical path: kill after exit must not fail
        s.kill().unwrap();
        assert!(matches!(s, TaskState::Exited));
        s.delete().unwrap();
        assert!(matches!(s, TaskState::Deleting));
    }

    #[test]
    fn delete_is_idempotent() {
        let mut s = TaskState::Exited;
        s.delete().unwrap();
        assert!(matches!(s, TaskState::Deleting));
        // Retry delete while already deleting must not fail
        s.delete().unwrap();
        assert!(matches!(s, TaskState::Deleting));
    }
}
