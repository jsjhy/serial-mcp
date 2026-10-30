//! WinAPI GUI 可执行入口：GUI 子系统不弹控制台，与 TUI 共用业务装配代码。
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]
const GUI_BINARY: bool = true;
include!("entry.rs");
