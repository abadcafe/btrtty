#[cfg(target_os = "macos")]
pub mod app;
#[cfg(target_os = "macos")]
pub mod bluetooth;
pub mod cli;
pub mod egress;
#[cfg(target_os = "macos")]
pub mod session;
#[cfg(target_os = "macos")]
pub mod shutdown;

#[cfg(all(test, target_os = "macos"))]
mod app_tests;
#[cfg(all(test, target_os = "macos"))]
mod cli_tests;
#[cfg(all(test, target_os = "macos"))]
mod egress_tests;
#[cfg(all(test, target_os = "macos"))]
mod session_tests;
