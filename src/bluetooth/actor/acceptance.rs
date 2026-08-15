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

    pub(super) fn arm(&mut self, after: ConnectionId) {
        if matches!(self.state, State::Awaiting(expected) if expected == after) {
            self.state = State::Armed;
        }
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

#[cfg(test)]
mod tests {
    use super::AcceptanceGate;
    use crate::bluetooth::types::ConnectionId;

    #[test]
    fn initial_acceptance_is_armed_before_publication() {
        let mut gate = AcceptanceGate::new();
        assert!(gate.begin_connection());
        assert!(!gate.begin_connection());
    }

    #[test]
    fn reconnect_requires_exact_closed_connection() {
        let mut gate = busy_gate();
        gate.connection_closed(connection(7)).unwrap();

        gate.arm(connection(6));
        assert!(!gate.begin_connection());
        gate.arm(connection(7));
        assert!(gate.begin_connection());
    }

    #[test]
    fn duplicate_reconnect_permit_cannot_open_two_connections() {
        let mut gate = busy_gate();
        gate.connection_closed(connection(8)).unwrap();
        gate.arm(connection(8));
        gate.arm(connection(8));

        assert!(gate.begin_connection());
        assert!(!gate.begin_connection());
    }

    #[test]
    fn shutdown_permanently_closes_gate() {
        let mut gate = AcceptanceGate::new();
        gate.stop();
        gate.arm(connection(1));

        assert!(gate.is_stopped());
        assert!(!gate.begin_connection());
    }

    fn busy_gate() -> AcceptanceGate {
        let mut gate = AcceptanceGate::new();
        assert!(gate.begin_connection());
        gate
    }

    fn connection(value: u64) -> ConnectionId {
        ConnectionId::new(value).unwrap()
    }
}
