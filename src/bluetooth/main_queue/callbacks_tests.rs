use std::ffi::c_void;

use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext, CFRunLoopTimer,
    CFRunLoopTimerContext,
};

use super::callbacks::RunLoopCallbacks;

#[test]
fn test_drop_invalidates_source_and_timer() {
    let (source, timer, run_loop) = callback_objects();
    let source_probe = source.clone();
    let timer_probe = timer.clone();
    let mut callbacks = RunLoopCallbacks::new(source, timer, run_loop).unwrap();
    callbacks.activate();

    assert!(source_probe.is_valid());
    assert!(timer_probe.is_valid());
    drop(callbacks);

    assert!(!source_probe.is_valid());
    assert!(!timer_probe.is_valid());
}

#[test]
fn test_detach_is_idempotent() {
    let (source, timer, run_loop) = callback_objects();
    let source_probe = source.clone();
    let timer_probe = timer.clone();
    let mut callbacks = RunLoopCallbacks::new(source, timer, run_loop).unwrap();
    callbacks.activate();

    callbacks.detach();
    callbacks.detach();

    assert!(!source_probe.is_valid());
    assert!(!timer_probe.is_valid());
}

fn callback_objects() -> (
    objc2_core_foundation::CFRetained<CFRunLoopSource>,
    objc2_core_foundation::CFRetained<CFRunLoopTimer>,
    objc2_core_foundation::CFRetained<CFRunLoop>,
) {
    let mut source_context = CFRunLoopSourceContext {
        version: 0,
        info: std::ptr::null_mut(),
        retain: None,
        release: None,
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: Some(noop_source),
    };
    // SAFETY: The context is fully initialized and the callback ignores info.
    let source = unsafe { CFRunLoopSource::new(None, 0, &raw mut source_context) }.unwrap();
    let mut timer_context = CFRunLoopTimerContext {
        version: 0,
        info: std::ptr::null_mut(),
        retain: None,
        release: None,
        copyDescription: None,
    };
    // SAFETY: The context is fully initialized and the callback ignores info.
    let timer = unsafe {
        CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent() + 60.0,
            60.0,
            0,
            0,
            Some(noop_timer),
            &raw mut timer_context,
        )
    }
    .unwrap();
    let run_loop = CFRunLoop::current().unwrap();
    (source, timer, run_loop)
}

unsafe extern "C-unwind" fn noop_source(_info: *mut c_void) {}

unsafe extern "C-unwind" fn noop_timer(_timer: *mut CFRunLoopTimer, _info: *mut c_void) {}
