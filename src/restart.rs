//! 应用重启协调：MCP 仅提出请求，主线程与 TUI 在宽限期后正常退出。
//! 独立的隐藏辅助进程等待旧进程退出，随后用原参数、工作目录启动新实例。
//! 辅助进程最多等待 30 秒；它不持有串口或监听器，结束后由系统回收资源。

use std::sync::{Mutex, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

static REQUESTED: AtomicBool = AtomicBool::new(false);
/// None 表示未请求或辅助进程启动尚未完成；锁内不执行进程或文件操作。
static DEADLINE: Mutex<Option<Instant>> = Mutex::new(None);

/// 启动辅助进程后才接受请求；失败释放门闩，可再次尝试。返回仅表示已安排，
/// 一秒宽限期给 HTTP 回复留出时间，并不保证客户端收到回复或新实例启动成功。
pub fn request() -> Result<&'static str, String> {
    if REQUESTED.swap(true, Ordering::SeqCst) {
        return Ok("restart already scheduled");
    }
    match spawn_helper() {
        Ok(()) => {
            *DEADLINE.lock().unwrap() = Some(Instant::now() + Duration::from_secs(1));
            Ok("restart scheduled; reconnect MCP after restart")
        }
        Err(e) => { REQUESTED.store(false, Ordering::SeqCst); Err(e.to_string()) }
    }
}

/// 由 TUI 和主线程轮询；只在辅助进程启动成功且 HTTP 宽限期结束后触发退出。
pub fn ready() -> bool {
    DEADLINE.lock().unwrap().is_some_and(|deadline| Instant::now() >= deadline)
}

#[cfg(windows)]
fn spawn_helper() -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(std::env::current_exe()?)
        .arg("--internal-restart-helper").arg(std::process::id().to_string())
        .args(std::env::args_os().skip(1))
        .creation_flags(0x08000000) // CREATE_NO_WINDOW：辅助进程不显示控制台。
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn spawn_helper() -> std::io::Result<()> {
    Err(std::io::Error::other("restart is supported on Windows only"))
}

/// 在 CLI、Tokio、日志和串口初始化之前调用；内部参数只供本程序辅助进程使用。
/// 参数通过 Command::args 逐项传递，不拼接 shell 命令，保留空格和非 ASCII 路径。
pub fn run_helper() -> std::io::Result<bool> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--internal-restart-helper")) {
        return Ok(false);
    }
    let pid: u32 = args.next().and_then(|v| v.to_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| std::io::Error::other("invalid parent PID"))?;
    wait_and_launch(pid, args.collect())?;
    Ok(true)
}

#[cfg(windows)]
fn wait_and_launch(pid: u32, args: Vec<std::ffi::OsString>) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    // 只申请 SYNCHRONIZE 权限；所有等待结果都关闭句柄，超时不强杀旧进程或启动副本。
    let result = unsafe {
        let handle = OpenProcess(0x00100000, 0, pid);
        if handle.is_null() { return Err(std::io::Error::last_os_error()); }
        let result = WaitForSingleObject(handle, 30_000);
        CloseHandle(handle);
        result
    };
    if result != 0 { return Err(std::io::Error::other("parent exit wait failed or timed out")); }
    // GUI 子系统自身创建窗口；旧 TUI 构建才需要新的控制台。
    let flags = if cfg!(feature = "gui") { 0 } else { 0x00000010 };
    std::process::Command::new(std::env::current_exe()?).args(args)
        .creation_flags(flags)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn wait_and_launch(_: u32, _: Vec<std::ffi::OsString>) -> std::io::Result<()> {
    Err(std::io::Error::other("restart is supported on Windows only"))
}
