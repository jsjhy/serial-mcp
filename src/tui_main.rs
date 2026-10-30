//! TUI 可执行入口：保持控制台子系统和原有快捷键，独立于 GUI 进程运行。
const GUI_BINARY: bool = false;
include!("entry.rs");
