# btrtty

`btrtty` 把 macOS PTY 发布成标准 Bluetooth Classic Serial Port Profile（SPP）服务。Linux 将它作为普通 RFCOMM 串口连接后，可以用 `picocom`、`screen`、`minicom` 或其他 raw serial client 操作 Mac 上的 bash/tmux。

```text
Linux terminal                    Mac
picocom / screen                  btrtty
        ↕                              ↕
/dev/rfcomm0 ← raw RFCOMM/SPP → async PTY ←→ zsh/tmux
```

当前 v0.1 使用固定 PTY 尺寸、单个 active client、系统配对认证和限速透明字节流。不会解析 ANSI、转换 CRLF、插入控制帧或记录终端 payload。

## 架构约束

```text
macOS main thread                 Tokio runtime（2 workers）
┌──────────────────────┐         ┌──────────────────────────┐
│ IOBluetooth actor    │ events  │ async supervisor         │
│ SDP / RFCOMM delegate├────────►│ PTY RX / TX / child wait │
│ CFRunLoopSource      │◄────────┤ bounded output queue     │
└──────────────────────┘ commands└──────────────────────────┘
```

- IOBluetooth 对象只存在于 `MainThreadOnly` actor，未实现或伪造 `Send`/`Sync`。
- delegate callback 只复制数据并发送事件；不操作 PTY、不等待、不执行 shell。
- Tokio 命令经 `CFRunLoopSourceSignal` + `CFRunLoopWakeUp` 送回主线程，无轮询。
- RFCOMM 只使用 `writeAsync`，每次不超过 MTU，同时最多一个 outstanding write。
- SDP record 和 incoming listener 与长期运行的 btrtty 进程同寿命；普通 client disconnect 只销毁本次 PTY/bash，绝不重新发布 Bluetooth 服务。
- 每个 active channel 都注册独立的 channel-close notification；actor 必须同时收到 outstanding write completion 和真实 channel-close callback，才会释放 channel 与 pending `Bytes`，两个事件无论谁先到都不能越过另一道屏障。超过单 active-client 限制的已打开 channel 也会被保留并等待真实 close callback，不会直接丢弃 Objective-C 对象。
- 最终关闭顺序固定为：停止 PTY 和新 write → 等待 outstanding write → 请求关闭已知 channel → 等待全部 channel-close callback → 删除 SDP record → 注销 incoming notification → 停止 CFRunLoop。不得提前报告 native shutdown 完成。
- `closeChannel()` 提交失败时不会用设备级 ACL 关闭兜底，也不会假装 channel 已关闭；actor 保留所有权并继续等远端真实 close callback，完成资源清理后以错误状态退出。
- IOBluetooth 不提供枚举或取消 pre-open pending incoming connection 的公开 API。SIGINT/SIGTERM 的明确前置条件是操作者先停止 Linux 侧所有新连接和自动重试；btrtty 不包含虚假的 drain、延时轮询或 publish-remove-republish 清理。
- 进程持有排他单实例锁；第二个 btrtty 会直接报错，不会与旧 listener 竞争或尝试接管其 channel。
- 启动只发布一次 transient SDP record。所有可能失败的 CFRunLoop 基础设施在发布前创建；notification 注册失败时必须成功回滚本次 SDP record，否则启动直接报错。
- Mac→Linux 默认限制为 20 KiB/s；空闲不会积累 burst，最多首个 `256 B` PTY chunk 立即发送，调度进度跨 Bluetooth 重连保留。
- PTY output 使用 `256 B` chunk，bounded queue 按限速值容纳约 100 ms 输出；下游变慢时通过 PTY/kernel 自然反压，不丢字符。

## 构建

需要 macOS、Command Line Tools/Xcode 和 Rust stable：

```bash
cargo build --release
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
```

生成内嵌 Bluetooth 权限说明的 ad-hoc signed 单文件命令行程序：

```bash
./scripts/package.sh
```

输出在 `dist/btrtty`。发布签名时可传入正式身份：

```bash
CODESIGN_IDENTITY='Developer ID Application: Example (TEAMID)' \
  ./scripts/package.sh
```

需要全局使用时，将这一个文件安装到 `PATH`：

```bash
install -m 755 dist/btrtty /usr/local/bin/btrtty
```

## 配对准入

btrtty 不使用自定义静态 PIN，也不保存配对码。先由 macOS 与 GNOME 的系统 Bluetooth UI 完成 Secure Simple Pairing：

1. 在 Mac 打开 `System Settings → Bluetooth`。
2. 在 Linux 打开 `GNOME Settings → Bluetooth`，选择该 Mac。
3. 两边显示同一数字时，在两边确认。
4. Linux 可用 `bluetoothctl info MAC_BT_ADDRESS` 检查 `Paired: yes`。

配对码只在首次配对时用于建立系统配对关系，reconnect 不会再次要求输入。Mac 在创建 PTY 前检查设备的系统配对状态；未配对的连接不会得到 shell。btrtty 不额外要求当前 RFCOMM 链路启用加密。

安全边界是“仍在这台 Mac 配对列表中的 Bluetooth 设备”，不再维护地址 allowlist。撤销权限时，请在 macOS Bluetooth 设置中 Forget/Remove 该设备。

## macOS 使用

PTY 大小在启动时通过 `--rows` 和 `--cols` 指定，连接期间保持不变：

```bash
dist/btrtty \
  --rows 24 \
  --cols 80 \
  --max-output-rate 20 \
  --command '/opt/homebrew/bin/tmux -T RGB new-session -A -s btrtty'
```

`--max-output-rate` 的单位是 KiB/s，取值范围为 `1..=200`，只限制 Mac→Linux 的输出。空闲期间不会积累发送额度；空闲后的首个至多 `256 B` PTY chunk 可以立即发送，后续 chunk 按配置速率调度。限速器属于整个进程，重连不会重置尚未到期的发送进度。

PTY output queue 根据该参数动态容纳约 100 ms 的数据：在 1、20（默认）和 200 KiB/s 下分别为 `1 × 256 B`、`8 × 256 B` 和 `80 × 256 B`。这是有界反压队列，不是额外的可立即发送额度。

默认值是 `exec /bin/bash -l`、24 行、80 列（即通常所说的 `80×24`）和 20 KiB/s。正常启动会显示系统动态分配的 RFCOMM channel：

```text
Registered Bluetooth Serial Port
Bluetooth address: 10:B5:88:4D:11:18
SDP record handle: 0x4F491101
RFCOMM channel: 11
Waiting for paired client...
```

客户端断开时，btrtty 必须同时等到 outstanding write completion（若存在）和 IOBluetooth 的实际 channel-close callback，随后才销毁本次 PTY 并重新进入等待；SDP/listener 全程不动。SIGINT/SIGTERM 也必须跨过这两道屏障，之后才依次删除 SDP、注销 listener 并退出。

### 长期运行与退出契约

推荐在 tmux 中启动一次 btrtty，日常只 detach tmux；Linux 可以反复 connect/disconnect，不需要重启 btrtty。

IOBluetooth 只在 RFCOMM channel 已经打开以后通知应用，btrtty 无法观察 `bluetoothd` 已收到但尚未交付的 pending connection。因此停止服务前，操作者必须先停止 Linux 上的 `rfcomm connect` 以及任何自动重试，再向 btrtty 发送 SIGINT/SIGTERM。程序不会用等待固定秒数、关闭设备 ACL、切换 Bluetooth controller 或杀死 `bluetoothd` 来假装清理完成。

若旧版或异常退出已经使 macOS Bluetooth daemon 留下不可见的 RFCOMM 状态，新进程无法通过公开 IOBluetooth API 枚举或删除它。一次性恢复流程是：停止 Linux 连接、退出 btrtty、在 macOS 关闭并重新打开 Bluetooth，然后启动当前版本。该全局操作只用于人工故障恢复，不属于 btrtty 启动或关闭流程。

Linux 窗口变化不会修改 Mac PTY；需要其他尺寸时，使用新的 `--rows/--cols` 重启 server。tmux 模式下，Bluetooth 断线只销毁本次 PTY client；tmux session 本身保留，重连会重新 attach。

### Bluetooth 权限

`btrtty` 可执行文件的 Mach-O `__TEXT,__info_plist` section 内嵌了 `CFBundleIdentifier=dev.btrtty.terminal` 和 `NSBluetoothAlwaysUsageDescription`。首次运行时请处理系统 Bluetooth 授权提示，或到：

```text
System Settings → Privacy & Security → Bluetooth
```

允许 `btrtty` 访问 Bluetooth。不要用 `sudo` 启动。开发时如需重置权限，使用：

```bash
tccutil reset BluetoothAlways dev.btrtty.terminal
```

## Linux 连接

完成 GNOME 配对后，使用 Mac 启动日志显示的 Bluetooth address 和 RFCOMM channel：

```bash
sudo rfcomm -r connect /dev/rfcomm0 MAC_BT_ADDRESS 11
```

另一个终端运行：

```bash
picocom -b 115200 --flow n /dev/rfcomm0
```

`-r` 必须位于 `connect` 前面。它让 BlueZ 在创建 RFCOMM TTY 后立即切换到 raw mode，避免 picocom 打开设备前，Linux TTY 的默认 canonical/echo line discipline 把 Mac 的初始 prompt 回送给 Mac shell，形成输出回声环。这里的 `115200` 只是 serial client 需要的名义波特率，不控制 Bluetooth 吞吐。退出后可释放设备：

```bash
sudo rfcomm release /dev/rfcomm0
```

准备停止 Mac 上长期运行的 btrtty 时，必须先执行 release，并确认没有 `rfcomm connect` 或其他客户端在自动重试，然后再向 btrtty 发送 SIGINT/SIGTERM。

BlueZ 上游把传统 `rfcomm` CLI 放在 deprecated tools 构建选项中，因此部分发行版不会默认安装。先检查：

```bash
command -v rfcomm
```

若发行版没有该命令，可以安装其 legacy/deprecated BlueZ tools 包；也可以使用 BlueZ/BlueR 项目的 `rfcat connect --raw MAC_BT_ADDRESS` 直接连接标准 SerialPort profile。后者不创建 `/dev/rfcomm0`。

## Truecolor

RFCOMM 是透明字节流，`ESC[38;2;R;G;Bm` 等 truecolor 序列会原样到达 Linux terminal。btrtty 在 Mac PTY 中统一设置：

```text
TERM=xterm-256color
COLORTERM=truecolor
```

`btrtty` 使用系统已有的 `xterm-256color`，不安装自定义终端描述，也不设置 `TERMINFO`。这使嵌套 SSH、sudo、容器和远端主机继续使用通用终端类型。`COLORTERM=truecolor` 告知支持该约定的程序可以输出 24-bit color。

Linux 端必须使用实际支持 truecolor 的终端，例如 GNOME Terminal、Kitty、WezTerm 等。可在 Mac shell 中检查：

```bash
test "$TERM" = xterm-256color
printf '\033[48;2;255;80;40m truecolor \033[0m\n'
```

tmux 必须明确获知外层支持 RGB，因此推荐命令使用 `tmux -T RGB`。tmux 内部将 `TERM` 设置为 `tmux-256color`，这是正常行为。

## 透明性与日志

RFCOMM payload 与 PTY payload 一一对应：Ctrl-C、Ctrl-Z、Tab、方向键、UTF-8、NUL 和 ANSI 序列均不被 btrtty 解释。标准 `rfcomm`、`picocom`、`screen` 和 `minicom` 不需要了解 btrtty。

默认日志为 info 且不包含 terminal payload。调试状态机可用：

```bash
RUST_LOG=btrtty=debug btrtty ...
```

真实 Mac + Linux Bluetooth 硬件验收步骤记录在 [docs/hardware-acceptance.md](docs/hardware-acceptance.md)。
