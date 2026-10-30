//! serial-mcp-tui：串口调试工具。
//!
//! 能力：人工输入（TX）/ 输出（RX） / 环形缓冲日志（防丢+轮转） / TUI / MCP server（HTTP）。
//! 平台：Win11（Windows Terminal 下运行 TUI 效果最佳）。
//! 参数：命令行 > 配置文件（--config xxx.toml）> 内置默认。
//!
//! 启动顺序：解析 CLI → 发现/读取配置 → 合并参数 → 初始化诊断输出和 Tokio →
//! 创建日志线程与串口核心 → 尝试连接 → 启动配置监视、MCP 和 TUI。
//! TUI 的键盘读取是阻塞 API，必须放在单独的 OS 线程；MCP 与配置监视运行在 Tokio。
//! TUI 模式等待界面退出通知后返回；仅 MCP 模式轮询重启信号保持服务存活。
//! 本文件负责装配依赖，串口读写、数据编码和文件格式分别由对应模块负责。

mod ansi;
mod codec;
mod config;
mod logger;
mod restart;
mod serial_core;
#[cfg(feature = "tui")]
mod tui;
#[cfg(feature = "tui")]
mod selection;
#[cfg(feature = "mcp")]
mod mcp;

use std::path::PathBuf;

use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};
use logger::{LogConfig, LogWriter};
use serial_core::{SerialConfig, SerialCore};

use config::{file_mtime, load_file_config};

/// 命令行参数模型。这里的默认值用于 clap 展示及无文件配置时的回退。
/// 合并时还必须查询 ValueSource，不能仅凭字段值判断用户是否显式传参。
#[derive(Parser, Debug)]
#[command(
    name = "serial-mcp-tui",
    version,
    about = "串口调试工具：人工收发 + 环形缓冲日志 + TUI + MCP server"
)]
struct Cli {
    /// 串口号（Win11 下如 COM3）。不填则 TUI 内可输入。
    #[arg(short, long, value_name = "PORT")]
    port: Option<String>,

    /// 波特率
    #[arg(short, long, default_value_t = 115200)]
    baud: u32,

    /// 运行模式
    #[arg(long, value_enum, default_value_t = Mode::Both)]
    mode: Mode,

    /// 日志保存目录
    #[arg(long, default_value = "logs")]
    log_dir: PathBuf,

    /// 单日志文件轮转上限（字节）
    #[arg(long, default_value_t = 10 * 1024 * 1024)]
    log_rotate: u64,

    /// 日志保留天数
    #[arg(long, default_value_t = 30)]
    log_keep_days: u32,

    /// 内存缓冲攒到多少字节才写一次盘（减少硬盘写动作）
    #[arg(long, default_value_t = 4096)]
    log_flush_bytes: u64,

    /// 距上次写盘超过该毫秒数强制落盘（保证实时性）
    #[arg(long, default_value_t = 100)]
    log_flush_ms: u64,

    /// MCP server HTTP 端口
    #[arg(long, default_value_t = 8010)]
    mcp_port: u16,

    /// 配置文件（TOML，字段与 CLI 同名；优先级：命令行 > 配置文件 > 默认）
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// 热加载配置文件：变化时自动应用串口参数（port/baud），日志参数仍重启生效
    #[arg(long)]
    watch_config: bool,
}

/// 运行时选择与 Cargo feature 是两个维度：前者决定想启动什么，后者决定编译了什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Mode {
    /// TUI + MCP 同时运行
    Both,
    /// 仅 TUI
    Tui,
    /// 仅 MCP server
    Mcp,
}

/// 进程入口。启动配置错误直接返回/退出，串口打开失败则允许界面继续运行以便重试。
/// MCP 后台任务失败会写入共享状态，供 TUI 显示；不强制关闭人工调试界面。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if restart::run_helper()? { return Ok(()); }
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    // 判断某参数是否由命令行显式给出（区别于 clap 默认值）
    let explicit = |name: &str| matches.value_source(name) == Some(ValueSource::CommandLine);

    // 配置文件（可选；解析失败直接退出，避免带病运行）。
    // 显式 --config 优先；未指定时自动发现 exe 同目录 config.toml
    // （双击 exe 启动也能加载配置，无需命令行参数）。
    let config_path = match &cli.config {
        Some(p) => Some(p.clone()),
        None => std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("config.toml")))
            .filter(|p| p.exists()),
    };
    let file = match &config_path {
        Some(p) => match load_file_config(p) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        None => None,
    };
    let fc = file.as_ref();

    // 优先级合并：命令行 > 配置文件 > 默认
    // 例如配置 baud=9600、CLI 没有 --baud 时，不能让 clap 自动填入的 115200 覆盖文件值。
    // 同样，用户显式传 --baud 115200 时，即使恰好等于默认值，也必须覆盖配置文件。
    let port = if explicit("port") {
        cli.port.clone()
    } else {
        fc.and_then(|f| f.port.clone())
    };
    let baud = if explicit("baud") {
        cli.baud
    } else {
        fc.and_then(|f| f.baud).unwrap_or(115200)
    };
    let mode = if explicit("mode") {
        cli.mode
    } else {
        match fc.and_then(|f| f.mode.as_deref()) {
            Some("tui") => Mode::Tui,
            Some("mcp") => Mode::Mcp,
            _ => Mode::Both,
        }
    };
    let log_dir = if explicit("log_dir") {
        cli.log_dir.clone()
    } else {
        fc.and_then(|f| f.log_dir.clone()).unwrap_or(PathBuf::from("logs"))
    };
    let log_rotate = if explicit("log_rotate") {
        cli.log_rotate
    } else {
        fc.and_then(|f| f.log_rotate).unwrap_or(10 * 1024 * 1024)
    };
    let log_keep_days = if explicit("log_keep_days") {
        cli.log_keep_days
    } else {
        fc.and_then(|f| f.log_keep_days).unwrap_or(30)
    };
    let log_flush_bytes = if explicit("log_flush_bytes") {
        cli.log_flush_bytes
    } else {
        fc.and_then(|f| f.log_flush_bytes).unwrap_or(4096)
    };
    let log_flush_ms = if explicit("log_flush_ms") {
        cli.log_flush_ms
    } else {
        fc.and_then(|f| f.log_flush_ms).unwrap_or(100)
    };
    // 仅 TUI 构建时该值无用（MCP 未编译），不视为错误配置
    #[cfg_attr(not(feature = "mcp"), allow(unused_variables))]
    let mcp_port = if explicit("mcp_port") {
        cli.mcp_port
    } else {
        fc.and_then(|f| f.mcp_port).unwrap_or(8010)
    };

    // TUI 独占终端；后台诊断写文件，防止破坏 ratatui 的屏幕差分状态。
    let diagnostics: Box<dyn std::io::Write + Send> = if cfg!(feature = "tui") && matches!(mode, Mode::Both | Mode::Tui) {
        std::fs::create_dir_all(&log_dir)?;
        Box::new(std::fs::OpenOptions::new().create(true).append(true).open(log_dir.join("application.log"))?)
    } else {
        Box::new(std::io::stderr())
    };
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(diagnostics))
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .thread_name("serial-rt")
        .enable_all()
        .build()?;
    // Handle 只提供进入运行时的能力，真正的 Runtime 由 main 持有，覆盖所有工作线程的运行期。
    #[cfg(feature = "tui")]
    let handle = rt.handle().clone();

    rt.block_on(async move {
        // 日志系统（环形缓冲 + 异步落盘 + 轮转）。日志参数仅启动生效（文档说明）。
        let log = LogWriter::start(
            LogConfig {
                dir: log_dir.clone(),
                rotate_size: log_rotate,
                keep_days: log_keep_days,
                flush_bytes: log_flush_bytes,
                flush_interval: std::time::Duration::from_millis(log_flush_ms),
            },
            4096, // 环形缓冲可容纳记录条数
        );

        // 串口核心（启动读写线程）
        let core = SerialCore::start(Some(log.clone()));

        // 指定端口优先；未指定时自动连接唯一串口，多个端口时交给用户选择。
        let startup_port = match port {
            Some(port) => Some(port),
            None => match SerialCore::available_ports() {
                Ok(ports) if ports.len() == 1 => ports.into_iter().next(),
                Ok(ports) if ports.is_empty() => {
                    tracing::info!("no serial ports found; connect a device and press Ctrl+R");
                    None
                }
                Ok(ports) => {
                    tracing::info!("multiple serial ports: {}; use Ctrl+O to select", ports.join(", "));
                    None
                }
                Err(e) => {
                    tracing::warn!("scan serial ports failed: {e}");
                    None
                }
            },
        };
        if let Some(port) = &startup_port {
            let cfg = SerialConfig {
                port: port.clone(),
                baud,
            };
            if let Err(e) = core.open(cfg).await {
                tracing::warn!("{e}");
            }
        }

        // 配置文件热加载：变化时自动应用串口参数（port/baud），日志参数重启生效
        if cli.watch_config {
            match &config_path {
                Some(path) => {
                    let path = path.clone();
                    let core2 = core.clone();
                    tokio::spawn(async move {
                        // 比较 mtime 而非文件内容；删除/重建文件也会改变 Option<SystemTime>。
                        // 这里只重读文件中的 port/baud，不重新合并 CLI 覆盖项，也不重建日志/MCP。
                        let mut last = file_mtime(&path);
                        loop {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let now = file_mtime(&path);
                            if now != last {
                                // 先记下本次版本：解析失败也不会每 500ms 重复报同一个错误。
                                // 再次保存文件（mtime 改变）后才会重试应用。
                                last = now;
                                match load_file_config(&path) {
                                    Ok(fc) => {
                                        if let Some(p) = &fc.port {
                                            let b = fc.baud.unwrap_or(115200);
                                            match core2.open(SerialConfig { port: p.clone(), baud: b }).await {
                                                Ok(()) => tracing::info!("watch reload: {p} @ {b} baud"),
                                                Err(e) => tracing::warn!("watch reload failed: {e}"),
                                            }
                                        }
                                    }
                                    Err(e) => tracing::warn!("watch reload: {e}"),
                                }
                            }
                        }
                    });
                }
                None => tracing::warn!("--watch-config 需要 --config 指定配置文件"),
            }
        }

        let need_tui = matches!(mode, Mode::Both | Mode::Tui);
        let need_mcp = matches!(mode, Mode::Both | Mode::Mcp);
        #[allow(unused_mut)]
        let mut started = false;

        // MCP server 跑在 tokio 内
        #[cfg(feature = "mcp")]
        if need_mcp {
            // 服务监听是异步过程，先发布 starting；bind 成功后由 mcp::run 发布 listening。
            core.set_mcp_status(format!("starting :{mcp_port}"));
            let mcp_core = core.clone();
            tokio::spawn(async move {
                if let Err(e) = mcp::run(mcp_core.clone(), mcp_port).await {
                    mcp_core.set_mcp_status(format!("failed: {e}"));
                    tracing::error!("MCP server failed: {e}");
                } else {
                    mcp_core.set_mcp_status("stopped".into());
                }
            });
            started = true;
        }
        // 需要 MCP 但构建时未启用 feature：明确告警，不能假装已在监听
        #[cfg(not(feature = "mcp"))]
        if need_mcp {
            tracing::warn!("this build has no `mcp` feature: MCP server not started");
        }

        // TUI 需要单独线程（crossterm 阻塞式事件），用 runtime handle 与其通信
        #[cfg(feature = "tui")]
        let (tui_exit_tx, tui_exit_rx) = tokio::sync::oneshot::channel();
        // oneshot 同时传回正常退出和 io::Error；若线程意外退出，接收端也能收到关闭错误。
        #[cfg(feature = "tui")]
        if need_tui {
            let handle = handle.clone();
            let core2 = core.clone();
            let config_path = config_path.clone();
            let tui_cfg = fc.and_then(|f| f.tui.clone()).unwrap_or_default();
            std::thread::Builder::new()
                .name("tui".into())
                .spawn(move || {
                    let result = tui::run(&handle, core2, config_path, &tui_cfg);
                    let _ = tui_exit_tx.send(result);
                })?;
            started = true;
        }
        #[cfg(not(feature = "tui"))]
        if need_tui {
            tracing::warn!("this build has no `tui` feature: TUI not started");
        }

        if !started {
            return Err("nothing to run: rebuild with the `tui` and/or `mcp` feature".into());
        }
        #[cfg(feature = "tui")]
        if need_tui {
            // TUI 退出后结束进程，避免界面关闭但 MCP 和串口仍在后台占用。
            tui_exit_rx.await??;
            if restart::ready() {
                core.close().await;
                log.shutdown(std::time::Duration::from_secs(3))?;
            }
            return Ok(());
        }
        // 仅 MCP 模式保持 runtime 存活。
        while !restart::ready() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        core.close().await;
        log.shutdown(std::time::Duration::from_secs(3))?;
        Ok(())
    })
}
