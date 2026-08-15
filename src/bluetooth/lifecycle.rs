use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LifecycleAction {
    Wait,
    CloseChannel,
    NotifyDisconnected,
    WithdrawService,
    StopRunLoop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Unpublished,
    Idle,
    Active(ActiveState),
    WaitingForRejectedShutdown,
    WithdrawingService,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveMode {
    Open,
    ClosingSession,
    ClosingForShutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveState {
    mode: ActiveMode,
    writing: bool,
    channel_closed: bool,
}

pub(super) struct Lifecycle {
    state: State,
    rejected_channels: usize,
}

impl Lifecycle {
    pub(super) const fn new() -> Self {
        Self {
            state: State::Unpublished,
            rejected_channels: 0,
        }
    }

    pub(super) fn service_published(&mut self) -> Result<()> {
        if self.state != State::Unpublished {
            bail!("Bluetooth service can only be published once");
        }
        self.state = State::Idle;
        Ok(())
    }

    pub(super) fn channel_opened(&mut self) -> Result<()> {
        if self.state != State::Idle {
            bail!("cannot accept an RFCOMM channel unless the service is idle");
        }
        self.state = State::Active(ActiveState {
            mode: ActiveMode::Open,
            writing: false,
            channel_closed: false,
        });
        Ok(())
    }

    pub(super) fn rejected_channel_opened(&mut self) -> Result<()> {
        match self.state {
            State::Idle | State::Active(_) | State::WaitingForRejectedShutdown => {
                self.rejected_channels = self
                    .rejected_channels
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("rejected RFCOMM channel count overflow"))?;
                Ok(())
            }
            State::WithdrawingService => {
                self.rejected_channels = self
                    .rejected_channels
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("rejected RFCOMM channel count overflow"))?;
                self.state = State::WaitingForRejectedShutdown;
                Ok(())
            }
            State::Unpublished | State::Stopped => {
                bail!("cannot track an RFCOMM channel without a live listener")
            }
        }
    }

    pub(super) fn rejected_channel_closed(&mut self) -> Result<LifecycleAction> {
        if self.rejected_channels == 0 {
            bail!("rejected RFCOMM close without a tracked channel");
        }
        self.rejected_channels -= 1;
        if self.state == State::WaitingForRejectedShutdown && self.rejected_channels == 0 {
            self.state = State::WithdrawingService;
            Ok(LifecycleAction::WithdrawService)
        } else {
            Ok(LifecycleAction::Wait)
        }
    }

    pub(super) fn write_started(&mut self) -> Result<()> {
        match self.state {
            State::Active(ActiveState {
                mode: ActiveMode::Open,
                writing: false,
                channel_closed: false,
            }) => {
                self.state = State::Active(ActiveState {
                    mode: ActiveMode::Open,
                    writing: true,
                    channel_closed: false,
                });
                Ok(())
            }
            State::Active(ActiveState {
                mode: ActiveMode::Open,
                writing: true,
                channel_closed: false,
            }) => bail!("RFCOMM write already outstanding"),
            State::Unpublished
            | State::Idle
            | State::Active(_)
            | State::WaitingForRejectedShutdown
            | State::WithdrawingService
            | State::Stopped => bail!("cannot write without an open RFCOMM channel"),
        }
    }

    pub(super) fn write_completed(&mut self) -> Result<LifecycleAction> {
        match self.state {
            State::Active(mut active) if active.writing => {
                active.writing = false;
                match (active.mode, active.channel_closed) {
                    (ActiveMode::Open, false) => {
                        self.state = State::Active(active);
                        Ok(LifecycleAction::Wait)
                    }
                    (ActiveMode::ClosingSession | ActiveMode::ClosingForShutdown, false) => {
                        self.state = State::Active(active);
                        Ok(LifecycleAction::CloseChannel)
                    }
                    (ActiveMode::ClosingSession, true) => {
                        self.state = State::Idle;
                        Ok(LifecycleAction::NotifyDisconnected)
                    }
                    (ActiveMode::ClosingForShutdown, true) => Ok(self.begin_service_withdrawal()),
                    (ActiveMode::Open, true) => {
                        bail!("open RFCOMM channel cannot already be closed")
                    }
                }
            }
            State::Unpublished
            | State::Idle
            | State::Active(_)
            | State::WaitingForRejectedShutdown
            | State::WithdrawingService
            | State::Stopped => bail!("RFCOMM write completion without an outstanding write"),
        }
    }

    pub(super) fn begin_session_close(&mut self) -> Result<LifecycleAction> {
        match self.state {
            State::Active(mut active) if active.mode == ActiveMode::Open => {
                active.mode = ActiveMode::ClosingSession;
                self.state = State::Active(active);
                Ok(if active.writing {
                    LifecycleAction::Wait
                } else {
                    LifecycleAction::CloseChannel
                })
            }
            State::Active(_) | State::WaitingForRejectedShutdown | State::WithdrawingService => {
                Ok(LifecycleAction::Wait)
            }
            State::Unpublished | State::Idle | State::Stopped => {
                bail!("cannot close an RFCOMM session without an active channel")
            }
        }
    }

    pub(super) fn begin_shutdown(&mut self) -> Result<LifecycleAction> {
        match self.state {
            State::Idle => Ok(self.begin_service_withdrawal()),
            State::Active(mut active) => {
                let was_closing = active.mode != ActiveMode::Open;
                active.mode = ActiveMode::ClosingForShutdown;
                self.state = State::Active(active);
                if active.channel_closed && !active.writing {
                    Ok(self.begin_service_withdrawal())
                } else if active.writing || was_closing {
                    Ok(LifecycleAction::Wait)
                } else {
                    Ok(LifecycleAction::CloseChannel)
                }
            }
            State::WaitingForRejectedShutdown | State::WithdrawingService => {
                Ok(LifecycleAction::Wait)
            }
            State::Unpublished => bail!("cannot stop an unpublished Bluetooth service"),
            State::Stopped => bail!("Bluetooth service is already stopped"),
        }
    }

    pub(super) fn channel_closed(&mut self) -> Result<LifecycleAction> {
        match self.state {
            State::Active(mut active) if !active.channel_closed => {
                active.channel_closed = true;
                if active.writing {
                    if active.mode == ActiveMode::Open {
                        active.mode = ActiveMode::ClosingSession;
                    }
                    self.state = State::Active(active);
                    Ok(LifecycleAction::Wait)
                } else if active.mode == ActiveMode::ClosingForShutdown {
                    Ok(self.begin_service_withdrawal())
                } else {
                    self.state = State::Idle;
                    Ok(LifecycleAction::NotifyDisconnected)
                }
            }
            State::Unpublished
            | State::Idle
            | State::Active(_)
            | State::WaitingForRejectedShutdown
            | State::WithdrawingService
            | State::Stopped => bail!("RFCOMM close notification without an active channel"),
        }
    }

    pub(super) fn service_withdrawn(&mut self) -> Result<LifecycleAction> {
        if self.state != State::WithdrawingService {
            bail!("Bluetooth service withdrawal completed out of order");
        }
        if self.rejected_channels != 0 {
            bail!("Bluetooth service withdrawn with rejected channels still closing");
        }
        self.state = State::Stopped;
        Ok(LifecycleAction::StopRunLoop)
    }

    pub(super) fn ensure_stopped(&self) -> Result<()> {
        if self.state != State::Stopped {
            bail!("Bluetooth lifecycle did not reach stopped state");
        }
        if self.rejected_channels != 0 {
            bail!("Bluetooth lifecycle stopped with rejected channels still tracked");
        }
        Ok(())
    }

    pub(super) const fn is_withdrawing(&self) -> bool {
        matches!(self.state, State::WithdrawingService)
    }

    fn begin_service_withdrawal(&mut self) -> LifecycleAction {
        if self.rejected_channels == 0 {
            self.state = State::WithdrawingService;
            LifecycleAction::WithdrawService
        } else {
            self.state = State::WaitingForRejectedShutdown;
            LifecycleAction::Wait
        }
    }
}
