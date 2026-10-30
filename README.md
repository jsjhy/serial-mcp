# Serial Monitor — WinAPI GUI + MCP

Windows 11 原生串口工具：**WinAPI GUI + 人工收发 + 可读文本日志 + MCP server**。界面由 User32 和系统 RichEdit 实现，保留 Rust 串口/MCP 核心，不使用 WebView、Qt 或 Electron。可执行文件名为兼容既有启动配置继续使用 `serial_mcp_tui.exe`，默认启动 GUI，没有控制台窗口。旧 TUI 源码与可选构建保留，迁移前的 Git 基线为 `b8445a5`。

## 功能清单（对应需求）

| 需求 | 实现 |
|---|---|
| 串口双向收发 | 非阻塞读 / 独立写，读线程 + 写线程 channel 解耦，互不阻塞 |
| 人工输入到串口 | GUI 显示区末尾直接输入，回车发送，↑/↓ 历史，Ctrl+H/h 同步切换 input/view 的 ASCII/Hex；文本保留 UTF-8 并支持 `\r \n \t \0 \\ \xNN` 转义，行尾由 EOL 设置 |
| 日志文件保存 | 环形缓冲防丢（计数不静默丢）+ 异步落盘 + 按大小轮转 + 按天数清理，固定 UTF-8，带时间戳 + TX/RX/STATUS 方向 |
| GUI | 原生串口/波特率组合框、连接按钮、RichEdit 显示/输入、串口与 MCP 状态栏 |
| ASCII 视图 ANSI 颜色 | GUI 显示 SGR 前景色（16/256/真彩色），跨包保留 UTF-8/ANSI 状态；时间戳降低对比度；Hex 显示原始字节。GUI 不模拟清屏/光标控制或粗体/下划线/反显 |
| MCP server | rmcp streamable HTTP，独立端口，与 TUI 不抢终端，人工与 AI 共用 TX 队列 |
| Win11 支持 | serial2 Windows 后端 + 自动重连（设备拔出/休眠恢复） |

## 构建

```powershell
cargo build --release --offline
# 或旧 TUI + MCP：cargo build --release --offline --no-default-features --features tui,mcp
# 或仅 MCP：cargo build --no-default-features --features mcp
```

`.cargo/config.toml` 为 Windows x64 GNU/MSVC 配置静态 CRT；GNU 额外传递 `-static`。Rust 代码及第三方库编入 EXE，系统 DLL（User32、GDI、DWM、Winsock、系统 CRT、系统自带 `Msftedit.dll` 等）仍由 Windows 提供，无需附带 VC++、GCC、Qt 或 WebView 运行库。可用 `objdump -p target/release/serial_mcp_tui.exe` 检查 PE 导入表。默认 GUI 构建为 Windows GUI 子系统；需要控制台帮助/旧 TUI 时使用不带 `gui` feature 的构建。

`build.rs` 将 `gui.manifest` 嵌入 EXE，启用系统 Common Controls v6 样式与系统 DPI 感知；GNU 构建需要工具链自带的 `windres`，MSVC 构建需要 Windows SDK 的 `rc.exe`。这些是构建工具，不随运行程序分发。GUI 默认构建不启用终端输入后端，ANSI 解析只复用 ratatui 的文本/样式数据类型。

验证：`cargo test --offline --all-features` 覆盖原生隐藏 RichEdit、GUI 编码/跨包显示、旧 TUI 和 MCP；`./verify-restart.ps1` 验证独立 MCP 进程真实重启，`./verify-restart.ps1 -Mode both` 另外检查重启前后的真实 GUI 窗口。原生控件测试不访问真实串口、不覆盖剪贴板，不代表长期运行验证。

## 运行

```powershell
# GUI + MCP 同时运行（默认）
.\target\release\serial_mcp_tui.exe --port COM3 --baud 115200

# 仅 GUI（人工调试）
.\target\release\serial_mcp_tui.exe --mode gui --port COM3

# 仅 MCP server（给 AI 用，走 HTTP，不占用终端）
.\target\release\serial_mcp_tui.exe --mode mcp --mcp-port 8010
```

### CLI 参数

| 参数 | 默认 | 说明 |
|---|---|---|
| `-p, --port` | 空 | 串口号（如 COM3） |
| `-b, --baud` | 115200 | 波特率 |
| `--mode` | both | gui / mcp / both；tui 需要旧 TUI feature，both 优先选择 GUI |
| `--log-dir` | logs | 日志目录 |
| `--log-rotate` | 10485760 | 单文件轮转上限（字节） |
| `--log-keep-days` | 30 | 日志保留天数 |
| `--log-flush-bytes` | 4096 | 内存缓冲攒到多少字节才写一次盘（减少硬盘写动作） |
| `--log-flush-ms` | 100 | 距上次写盘超过该毫秒数强制落盘 |
| `--mcp-port` | 8010 | MCP HTTP 端口 |
| `--config` | 无 | 配置文件（TOML，字段与 CLI 同名） |
| `--watch-config` | false | 热加载配置：文件变化自动应用串口参数（port/baud） |

## 配置文件与快速重启

**优先级：命令行 > 配置文件 > 内置默认**。典型 `config.toml`：

```toml
# stm32.toml
port = "COM9"
baud = 9600
mode = "both"
log_dir = "logs"
log_rotate = 10485760
log_flush_bytes = 4096
log_flush_ms = 100
mcp_port = 8010
```

```powershell
# 用配置启动（可存多套场景：stm32.toml / esp32.toml）
.\target\release\serial_mcp_tui.exe --config stm32.toml

# CLI 覆盖配置文件（临时改波特率）
.\target\release\serial_mcp_tui.exe --config stm32.toml --baud 115200

# 热加载：改配置文件保存即自动重连串口（500ms 轮询）
.\target\release\serial_mcp_tui.exe --config stm32.toml --watch-config
```

**快速重启三档**：

| 方式 | 触发 | 生效范围 |
|---|---|---|
| TUI 内 `Ctrl+R` | 手动 | 重读配置，热重连串口（port/baud），TUI 历史保留 |
| `--watch-config` | 自动（改文件保存） | 同上 |
| 重启进程 | 手动 | 全部参数 |

**边界（文档约束）**：热更新只应用**串口参数**（port/baud）；日志参数（`log_dir/log_rotate/log_keep_days/log_flush_*/mcp_port`）**重启进程生效**——环形缓冲与落盘线程在启动时固定，避免并发竞态。配置解析失败（文件缺失/TOML 非法）直接退出并报错，不带病运行。

## GUI 操作

顶部选择或输入串口、波特率，点击 `Connect` / `Disconnect`；`Refresh` 刷新端口列表（后台也每秒刷新）。串口状态为 `CONNECTED`、`DISCONNECTED`、`NOT_FOUND`，操作错误显示在状态旁；`log:` 显示当前文本日志文件名。底部单行显示完整 MCP 服务/会话信息。

直接在显示区末尾设备提示符之后输入，不设独立输入行。`Enter` 等待串口驱动写入回执后清空草稿，失败保留；空输入在文本模式下也发送配置行尾。`Ctrl+H` 切换 ASCII/Hex，`Ctrl+E` 循环行尾，`Ctrl+K` 或 `Clear` 清空显示并保留草稿，`Ctrl+R` 按顶部当前端口/波特率连接，`Ctrl+X` 退出。↑/↓ 浏览命令历史；左右键、Home/End 使用原生编辑行为，鼠标滚轮和原生横纵滚动条查看历史/长行。窗口最顶部独立显示快捷键提示，启动后输入焦点位于显示区末尾。

鼠标拖动选择文字，右键或 `Ctrl+C` 复制；历史不可修改，点击历史后直接打字会跳至草稿末尾。选中历史期间暂停显示刷新，文件日志和 MCP 继续；超出广播容量时提示显示跳过事件，原始文件日志不依赖这个广播。接收插入在草稿之前，保留原生选区与阅读滚动位置。跨接收批次的撤销与 Ctrl+Backspace 删词被禁用，以保护历史/草稿边界。

为兼容现有配置，GUI 继续读取 `[tui]` 下的 `theme`、`line_ending`、`input_hex`/`hex_view`、`max_lines`。GUI 输入与显示统一模式，`input_hex` 优先；`max_lines` 在 GUI 中换算为历史字符预算（每行 1024 个 UTF-16 单位，限制 50–2000 行预算），达到预算后按整行裁剪前半段。主题只作用于文本显示区，工具栏使用系统控件；修改主题后重启，日志仍为纯文本。

## 旧 TUI 按键（仅 tui feature 构建）

| 按键 | 功能 |
|---|---|
| `Enter` | 发送当前输入（Port 模式下为打开该串口） |
| `↑` / `↓` | 历史命令 |
| `←` / `→` | 数据区左右滚动，每次 8 列 |
| `Home` | 数据区回到最左侧 |
| 鼠标滚轮 / `PageUp` / `PageDown` | 上下查看历史；新数据到来时保留当前阅读位置 |
| `End` | 回到最新数据，恢复自动跟随 |
| 点击或拖动滚动条 | 右侧查看上下历史，底部查看长行 |
| 内容区鼠标左键拖动 | 选取可见文字，选区期间固定画面，后台收数与日志继续 |
| 选中后右键 / `Ctrl+C` | 复制选区文字到 Windows 剪贴板 |
| 选中后 `Esc` | 取消选区；滚轮、翻页或输入也会取消选区 |
| `Ctrl+E` | 文本命令行尾循环切换 CRLF / none / CR / LF，默认 CRLF；Hex 不追加行尾 |
| `Ctrl+H` / `Ctrl+h` | 同步切换 input 与 view 的 ascii / hex（ascii 视图渲染设备 ANSI 颜色）；不转换已有历史显示和待发送输入。端口名输入态保持 Port，只切换 view；Tab 不再切换编码 |
| `Ctrl+O` | 手动输入串口名并打开（如 `COM9`），配置文件未写 `port` 时用 |
| `Ctrl+R` | 连接/重连：优先用配置文件端口；无配置且只有一个可用串口时直接连；多个则提示用 `Ctrl+O` |
| `Ctrl+L` | 重新枚举可用串口 |
| `Ctrl+K` | 清空 TUI 显示历史、选区与滚动位置，保留输入、命令历史、计数、MCP 日志缓冲和日志文件；继续显示后续数据 |
| `Esc` | 清空输入（Port 模式下取消输入） |
| `Ctrl+X` | 退出程序（同时停止 MCP 和串口连接） |

所有功能快捷键都带 `Ctrl`：可打印字符（含 `h` / `l` / `o` / `q`）一律进入输入区，不会被快捷键吞掉。未连接时按 `Enter` 不会丢数据，输入会保留并提示连接方式。

TUI 文本模式输入 `help` 后按 Enter，默认发送 `help\r\n`；已有 CR 或 LF 结尾时不重复追加。可在 `[tui]` 设置 `line_ending = "crlf"`（也支持 `none`、`cr`、`lf`）。MCP `send` 保持显式字节语义，需要自行填写 `help\r\n`。发送结果确认实际串口写入，设备执行结果需查看 RX。独立 MCP 状态行显示是否启用、监听地址及服务启动错误。

快捷键提示位于窗口最顶部（选区复制提示也在此显示），其下为串口状态和数据区。MCP 状态嵌入窗口下边框，状态、会话数、客户端名称和地址保持同行，不占用独立数据行。输入跟随终端内容，设备提示符未换行时直接在其后输入，不保留底部固定输入行。文本模式空输入按 Enter 也会发送配置的行尾（默认 CRLF），可用于唤出设备提示符；行尾为 none 时空输入不发送。

MCP 下边框状态栏区分服务和客户端状态：`LISTENING | waiting for client` 表示服务已监听、尚无已初始化客户端；`CONNECTED | sessions:N | 客户端名称` 表示 N 个逻辑会话完成了 `notifications/initialized` 握手，显示为绿色。显式关闭会话（HTTP DELETE）或传输关闭后会自动更新。Streamable HTTP 不要求一直保持 TCP 连接；异常退出且未关闭会话的客户端可能暂时保留登记，因此该状态不是实时在线心跳检测，也不代表串口已经连接。

串口状态栏内容中的 `log:` 显示当前日志文件名，文件轮转后自动更新；首次写盘前显示 `pending`。MCP 状态、会话数、客户端名称和地址固定在下边框同一行；横向滚动条只使用状态右侧的剩余空间，没有余量时仍可用左右键滚动；顶部快捷键、串口信息和操作提示按窗口宽度自动换行，并优先分配足够高度，数据区使用剩余空间。建议至少 120 列 × 24 行。所需列数至少为 40，并随完整 MCP 信息及边框留白长度增加；宽度或高度不足时显示 `Window too small` 以及所需列数/行数，扩大窗口后恢复，避免 MCP 信息被折行或截断。

串口状态分为三种：`CONNECTED`（已打开）、`DISCONNECTED`（设备存在但未连接）、`NOT_FOUND`（所选设备未被系统枚举到）。设备枚举每秒刷新；枚举失败时保守显示 DISCONNECTED 并保留错误原因。原第三行操作/错误提示合并到连接状态旁，例如 `DISCONNECTED (open COM8 failed: Access is denied...)`，不再单独预留提示行。

配色在配置文件的 `[tui]` 表设置 `theme = "nord"`，支持 `dark`（默认）、`light`、`nord`、`solarized`，修改后重启生效；设备输出的 ANSI 颜色保留。

TUI 时间戳使用正文与背景的中间色，降低约一半对比度；正文与日志文件不受影响。

## MCP 接入

端点为 `http://127.0.0.1:<port>/mcp`。8 个工具：`list_ports / open / close / send / read / set_dtr_rts / export_log / restart`。

`restart` 无参数，重启整个串口工具。返回 `restart scheduled` 仅表示已安排；预留一秒发送回复后，关闭界面/串口，最多等待三秒确认日志队列尾部写入与文件刷新（不等同于断电持久化）。辅助进程隐藏运行，最多等待旧进程退出 30 秒，然后使用原命令行参数、工作目录重新启动；GUI 构建不创建控制台，旧 TUI 构建创建可见控制台。启动时重新读取配置文件；未保存的输入和内存历史不会恢复。重复请求返回 `restart already scheduled`，不会重复启动；辅助进程创建失败返回错误并允许重试。旧 MCP 会话会断开，客户端必须重新连接；成功回复不保证新进程启动成功。此工具与现有串口工具共用 HTTP 访问权限，能够访问该端点的客户端也能够重启应用。

Windows 下先运行 `cargo build --release --offline`，再运行 `./verify-restart.ps1` 可验证真实进程的 MCP 重启和重新连接。脚本使用独立临时端口、无效串口名，不连接真实设备；结束时只清理该测试实例。单元测试另外验证日志结束屏障前未换行的尾部文本已写入文件。这些检查不代表长期运行或真实串口验证。

### Claude Code

```bash
claude mcp add serial -- "C:\path\to\serial_mcp_tui.exe" --mode mcp
```

### Cursor / 自定义客户端（streamable HTTP）

```json
{
  "mcpServers": {
    "serial": {
      "type": "http",
      "url": "http://127.0.0.1:8010/mcp"
    }
  }
}
```

### 用法示例（AI 对话）

- 列出串口：`list_ports`
- 打开 COM3 @ 115200：`open { "port": "COM3", "baud": 115200 }`
- 发送 AT 指令：`send { "ascii": "AT\\r\\n" }`（ascii 支持 `\r \n \t \0 \\ \xNN` 转义）
- 发送 hex 帧：`send { "hex": "01 03 00 00" }`
- 读最近数据：`read { "limit": 20 }`
- 复位 ESP32/STM32：`set_dtr_rts { "dtr": true, "rts": false }`

日志采用可读 UTF-8 文本：接收为 `[时间戳] 文本`，发送为 `[时间戳] TX: 文本`，状态为 `[时间戳] STATUS: 文本`。界面和日志均省略 RX 标记。同一方向的数据按设备换行拼接，跨包保留中文，去除 ANSI 颜色和终端控制序列；其他不可打印字节使用 `\xNN` 表示。修改只影响新生成的日志，既有日志保留。

## 发布产物

`dist/` 是可直接分发的目录（先 `cargo build --release`，再将 `target/release/serial_mcp_tui.exe` 与 README 复制到此目录）：

```
dist/
├── serial_mcp_tui.exe   # 单文件可执行程序
├── config.toml          # 示例配置（与 exe 同目录时自动加载）
└── README.md
```

双击 `serial_mcp_tui.exe` 即可启动（自动读取同目录 `config.toml`，无配置文件则默认 `--mode both`）。

启动时优先连接配置或命令行指定的端口；未指定且系统只有一个可用串口时自动连接，波特率使用合并后的配置。存在多个串口时通过 GUI 顶部组合框选择；旧 TUI 使用 Ctrl+O。

## 模块结构

```
src/
├── main.rs         # CLI + 配置合并（命令行>配置>默认）+ 模式编排 + watch 热加载
├── config.rs       # TOML 配置文件解析与优先级合并
├── gui.rs          # WinAPI 原生窗口 / RichEdit / 输入边界保护（feature: gui）
├── codec.rs        # hex 解析 + ascii 转义解码（TUI/MCP 共用）
├── serial_core.rs  # 串口核心：双向收发 / TX队列 / DTR·RTS / 自动重连 / 事件广播
├── logger.rs       # 环形缓冲 + 异步落盘 + 按大小轮转 + 按天清理 + 双阈值
├── restart.rs      # MCP 重启协调 + 隐藏辅助进程 + 旧进程退出等待
├── tui.rs          # ratatui 界面（feature: tui），含 Ctrl+R 热重连
└── mcp.rs          # rmcp MCP server，streamable HTTP（feature: mcp）
```

## 设计要点

- **串口并发**：serial2 `&self` 支持多线程并发读写，读/写独立线程 + channel 解耦，高波特率不阻塞 UI。
- **日志防丢**：读线程直接 push 权威日志（不经过可能丢数据的广播），环形缓冲满时计数 dropped 并显式丢弃最旧，绝不静默丢。
- **MCP 不抢终端**：走 streamable HTTP 独立端口，与 TUI 无冲突；人工输入与 AI 发送共用同一 TX 队列，日志按时间序混排。
- **重连**：设备拔出/休眠恢复后自动带退避重连。
