use crate::bluetooth::types::ConnectionId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Armed,
    Busy,
    Awaiting(ConnectionId),
    Stopped,
}

pub(super) struct AcceptanceGate {
    state: State,
}

impl AcceptanceGate {
    pub(super) const fn new() -> Self {
        Self {
            state: State::Armed,
        }
    }

    pub(super) fn arm(&mut self, after: ConnectionId) -> bool {
        if matches!(self.state, State::Awaiting(expected) if expected == after) {
            self.state = State::Armed;
            true
        } else {
            false
        }
    }

    pub(super) fn is_awaiting(&self, after: ConnectionId) -> bool {
        matches!(self.state, State::Awaiting(expected) if expected == after)
    }

    pub(super) const fn is_armed(&self) -> bool {
        matches!(self.state, State::Armed)
    }

    pub(super) fn begin_connection(&mut self) -> bool {
        if self.state != State::Armed {
            return false;
        }
        self.state = State::Busy;
        true
    }

    pub(super) fn cancel_begin(&mut self) {
        if self.state == State::Busy {
            self.state = State::Armed;
        }
    }

    pub(super) fn connection_closed(
        &mut self,
        connection: ConnectionId,
    ) -> Result<(), &'static str> {
        match self.state {
            State::Busy => {
                self.state = State::Awaiting(connection);
                Ok(())
            }
            State::Stopped => Ok(()),
            State::Armed | State::Awaiting(_) => {
                Err("acceptance gate observed a close without a busy connection")
            }
        }
    }

    pub(super) fn stop(&mut self) {
        self.state = State::Stopped;
    }

    pub(super) const fn is_stopped(&self) -> bool {
        matches!(self.state, State::Stopped)
    }
}
