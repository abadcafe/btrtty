use anyhow::{Context, Result};
use objc2_core_foundation::{
    CFRetained, CFRunLoop, CFRunLoopMode, CFRunLoopSource, CFRunLoopTimer, kCFRunLoopCommonModes,
};

/// Owns every Core Foundation callback registration that can reach the boxed
/// Bluetooth main-thread context.
///
/// Invalidating this guard is the mandatory fence before that context may be
/// destroyed. The operation is idempotent so normal teardown and unwinding
/// share exactly one ownership path.
pub(super) struct RunLoopCallbacks {
    source: CFRetained<CFRunLoopSource>,
    timer: CFRetained<CFRunLoopTimer>,
    run_loop: CFRetained<CFRunLoop>,
    common_modes: &'static CFRunLoopMode,
    active: bool,
}

impl RunLoopCallbacks {
    pub(super) fn new(
        source: CFRetained<CFRunLoopSource>,
        timer: CFRetained<CFRunLoopTimer>,
        run_loop: CFRetained<CFRunLoop>,
    ) -> Result<Self> {
        // SAFETY: Core Foundation exports this process-lifetime constant on
        // every supported macOS release.
        let common_modes =
            unsafe { kCFRunLoopCommonModes }.context("obtain kCFRunLoopCommonModes")?;
        Ok(Self {
            source,
            timer,
            run_loop,
            common_modes,
            active: false,
        })
    }

    pub(super) fn activate(&mut self) {
        if self.active {
            return;
        }
        // Mark ownership before either registration so unwinding from any
        // native wrapper still takes the full detach path.
        self.active = true;
        self.run_loop
            .add_source(Some(&self.source), Some(self.common_modes));
        self.run_loop
            .add_timer(Some(&self.timer), Some(self.common_modes));
    }

    pub(super) fn detach(&mut self) {
        if !self.active {
            self.timer.invalidate();
            self.source.invalidate();
            return;
        }

        // Invalidate first: no future run-loop iteration may enter either
        // callback while their context is being destroyed.
        self.timer.invalidate();
        self.source.invalidate();
        self.run_loop
            .remove_timer(Some(&self.timer), Some(self.common_modes));
        self.run_loop
            .remove_source(Some(&self.source), Some(self.common_modes));
        self.active = false;
    }

    pub(super) fn source(&self) -> &CFRunLoopSource {
        &self.source
    }

    pub(super) fn run_loop(&self) -> &CFRunLoop {
        &self.run_loop
    }
}

impl Drop for RunLoopCallbacks {
    fn drop(&mut self) {
        self.detach();
    }
}
