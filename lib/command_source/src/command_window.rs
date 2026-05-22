use std::collections::VecDeque;
use zksync_os_sequencer::model::blocks::{BlockCommandType, CommandAck};

pub const DEFAULT_COMMAND_WINDOW_CAPACITY: usize = 2;

#[derive(Debug)]
pub(crate) struct CommandWindow {
    capacity: usize,
    pending: VecDeque<BlockCommandType>,
}

impl CommandWindow {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "command window capacity must be non-zero");
        Self {
            capacity,
            pending: VecDeque::new(),
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn can_send(&self, command_type: BlockCommandType) -> bool {
        self.pending.len() < self.capacity
            && (!matches!(command_type, BlockCommandType::Produce)
                || !self
                    .pending
                    .iter()
                    .any(|pending| matches!(pending, BlockCommandType::Produce)))
    }

    pub fn push(&mut self, command_type: BlockCommandType) {
        assert!(
            self.can_send(command_type),
            "command window is full for {command_type:?}"
        );
        self.pending.push_back(command_type);
    }

    pub fn acknowledge(&mut self, ack: CommandAck) -> anyhow::Result<()> {
        let expected = self
            .pending
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("received {ack:?} with no pending command"))?;
        anyhow::ensure!(
            ack.command_type() == expected,
            "received {ack:?} while waiting for {expected:?} command acknowledgement"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zksync_os_sequencer::model::blocks::BlockCommandType;

    #[test]
    fn permits_multiple_replay_or_rebuild_commands() {
        let mut window = CommandWindow::new(2);

        assert!(window.can_send(BlockCommandType::Replay));
        window.push(BlockCommandType::Replay);
        assert!(window.can_send(BlockCommandType::Rebuild));
        window.push(BlockCommandType::Rebuild);

        assert!(!window.can_send(BlockCommandType::Replay));
    }

    #[test]
    fn permits_only_one_pending_produce_command() {
        let mut window = CommandWindow::new(2);

        assert!(window.can_send(BlockCommandType::Produce));
        window.push(BlockCommandType::Produce);

        assert!(!window.can_send(BlockCommandType::Produce));
        assert!(window.can_send(BlockCommandType::Replay));
    }
}
