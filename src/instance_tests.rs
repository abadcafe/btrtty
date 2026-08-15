use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::instance::InstanceLock;

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

#[test]
fn test_instance_lock_rejects_second_live_owner() {
    let path = test_path();
    let first = InstanceLock::acquire(&path).unwrap();

    assert!(InstanceLock::acquire(&path).is_err());

    drop(first);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn test_instance_lock_can_be_reacquired_after_owner_exits() {
    let path = test_path();
    let first = InstanceLock::acquire(&path).unwrap();
    drop(first);

    let second = InstanceLock::acquire(&path).unwrap();
    drop(second);
    std::fs::remove_file(path).unwrap();
}

fn test_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "btrtty-instance-test-{}-{}",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed)
    ))
}
