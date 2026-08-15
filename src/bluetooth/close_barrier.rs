use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CloseRequest {
    Session,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CloseCompletion {
    Remote,
    Session,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CloseAction {
    Wait,
    CloseChannel,
    Complete(CloseCompletion),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Waiting,
    Open {
        writing: bool,
    },
    Closing {
        request: CloseRequest,
        writing: bool,
    },
}

pub(super) struct CloseBarrier {
    state: State,
}

impl CloseBarrier {
    pub(super) const fn new() -> Self {
        Self {
            state: State::Waiting,
        }
    }

    pub(super) fn channel_opened(&mut self) -> Result<()> {
        if self.state != State::Waiting {
            bail!("cannot open a second RFCOMM channel");
        }
        self.state = State::Open { writing: false };
        Ok(())
    }

    pub(super) fn write_started(&mut self) -> Result<()> {
        match self.state {
            State::Open { writing: false } => {
                self.state = State::Open { writing: true };
                Ok(())
            }
            State::Open { writing: true } => bail!("RFCOMM write already outstanding"),
            State::Waiting => bail!("cannot write without an RFCOMM channel"),
            State::Closing { .. } => bail!("cannot write while RFCOMM channel is closing"),
        }
    }

    pub(super) fn write_completed(&mut self) -> Result<CloseAction> {
        match self.state {
            State::Open { writing: true } => {
                self.state = State::Open { writing: false };
                Ok(CloseAction::Wait)
            }
            State::Closing {
                request,
                writing: true,
            } => {
                self.state = State::Closing {
                    request,
                    writing: false,
                };
                Ok(CloseAction::CloseChannel)
            }
            State::Open { writing: false } | State::Closing { writing: false, .. } => {
                bail!("RFCOMM write completion without outstanding write")
            }
            State::Waiting => bail!("RFCOMM write completed without a channel"),
        }
    }

    pub(super) fn begin_close(&mut self, request: CloseRequest) -> CloseAction {
        match self.state {
            State::Waiting => CloseAction::Complete(completion(request)),
            State::Open { writing } => {
                self.state = State::Closing { request, writing };
                if writing {
                    CloseAction::Wait
                } else {
                    CloseAction::CloseChannel
                }
            }
            State::Closing {
                request: current,
                writing,
            } => {
                let request = if request == CloseRequest::Shutdown {
                    CloseRequest::Shutdown
                } else {
                    current
                };
                self.state = State::Closing { request, writing };
                CloseAction::Wait
            }
        }
    }

    pub(super) fn channel_closed(&mut self) -> Result<CloseCompletion> {
        let completion = match self.state {
            State::Waiting => bail!("RFCOMM close notification without an active channel"),
            State::Open { .. } => CloseCompletion::Remote,
            State::Closing { request, .. } => completion(request),
        };
        self.state = State::Waiting;
        Ok(completion)
    }
}

const fn completion(request: CloseRequest) -> CloseCompletion {
    match request {
        CloseRequest::Session => CloseCompletion::Session,
        CloseRequest::Shutdown => CloseCompletion::Shutdown,
    }
}
