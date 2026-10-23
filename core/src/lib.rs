pub mod agent;
pub mod ai;
pub mod alerts;
pub mod cmdhistory;
pub mod command;
pub mod completion;
pub mod config_dir;
pub mod configio;
pub mod connections;
pub mod events;
pub mod exec;
pub mod forward;
pub mod i18n;
pub mod metrics;
pub mod notify_smtp;
pub mod openssh;
pub mod proxy;
pub mod quickcmd;
pub mod reconnect;
pub mod runner;
pub mod session;
pub mod session_layout;
pub mod session_log;
pub mod settings;
pub mod sftp;
pub mod sftp_resume;
pub mod smtp_client;
#[cfg(test)]
mod smtp_client_tests;
pub mod smtp_wire;
pub mod split;
pub mod ssh;
pub mod stream;
pub mod tasks;
pub mod theme;
pub mod update;

/// 应用版本（关于页与诊断信息共用，避免多处硬编码字符串）。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 应用名。
pub const APP_NAME: &str = "RustShell";
