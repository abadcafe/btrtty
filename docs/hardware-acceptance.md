# Hardware acceptance

日期：2026-08-16

自动化环境：macOS 15.7.7（arm64）

## 已自动验证

```bash
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
./scripts/package.sh
```

- 46 项模块/单元测试与 2 项二进制元数据测试通过。
- 单文件 Mach-O 内嵌稳定 `CFBundleIdentifier` 和 `NSBluetoothAlwaysUsageDescription`，无 App Bundle。
- RFCOMM 与 PTY 之间没有握手、控制帧或 payload 转换。
- PTY 往返保持 UTF-8、NUL、ESC/方向键序列原始字节。
- `0x03` 由 PTY line discipline 作为 Ctrl-C 交给 foreground process group。
- `--rows/--cols` 指定的尺寸已经通过 `stty size` 集成测试验证。
- PTY 设置 `TERM=xterm-256color`、`COLORTERM=truecolor`，不安装自定义终端描述。
- MTU 分块且同时只有一个 outstanding RFCOMM write。
- `--max-output-rate` 默认 20 KiB/s，只接受 `1..=200`。
- 空闲不会积累 burst；最多首个 `256 B` PTY chunk 立即发送，后续输出按配置速率调度，进度不会因 Bluetooth 重连重置。
- `256 B` PTY chunk 的 bounded output queue 按限速值动态容纳约 100 ms 数据；在 1/20/200 KiB/s 下容量分别为 1/8/80 个 chunk。
- 取消会 kill/reap PTY child。
- 未配对的 Bluetooth 连接在创建 PTY 前收到 `Close`；已配对连接不额外要求链路加密。
- 单实例锁会拒绝第二个 live btrtty，并在 owner 退出后允许重新获取。
- Bluetooth 生命周期状态机已覆盖：等待 outstanding write、等待 active/rejected channel 的实际 close callback、session close 升级为 shutdown、channel 全部关闭后才允许移除 SDP，以及 SDP 移除后才允许停止 CFRunLoop。
- 启动路径只发布一次 SDP；所有可失败的 CFRunLoop 基础设施先于 publication 创建，incoming notification 注册失败必须检查 SDP rollback。
- 删除了 device-wide `closeConnection()` fallback、自动 drain、固定延时和启动时 publish-remove-republish 伪清理。

## 本机 Bluetooth smoke test（当前二进制待重测）

本机 Bluetooth controller 的历史测试地址是 `10:B5:88:4D:11:18`。当前二进制启动后预期输出以下字段；record handle 和 RFCOMM channel 均必须以当次实际输出为准：

```text
Registered Bluetooth Serial Port
Bluetooth address: 10:B5:88:4D:11:18
SDP record handle: 0x4F491101
RFCOMM channel: 1
Waiting for paired client...
```

- [ ] 当前签名二进制通过 macOS Bluetooth TCC 授权。
- [ ] 启动输出显示本机 Bluetooth controller address。
- [ ] 标准 SPP SDP record 发布成功并取得非零 record handle 与动态 RFCOMM channel。
- [ ] 无 active client 时 SIGINT/SIGTERM 按 SDP removal → listener unregister 顺序退出。
- [ ] 第二个 btrtty 实例在发布 SDP 前被排他锁拒绝。

## 当前环境阻塞项

当前环境没有可控制的 Linux Bluetooth client，因此不能诚实地把下面的双机硬件项标记为通过。

## 双机执行清单

Mac：

```bash
dist/btrtty \
  --rows 24 \
  --cols 80 \
  --max-output-rate 20 \
  --command '/opt/homebrew/bin/tmux -T RGB new-session -A -s btrtty'
```

先在 GNOME/macOS Bluetooth UI 中核对配对数字。Linux 确认：

```bash
bluetoothctl info MAC_BT_ADDRESS
```

应包含 `Paired: yes`，随后使用 Mac 日志中的实际 channel：

```bash
sudo rfcomm -r connect /dev/rfcomm0 MAC_BT_ADDRESS CHANNEL
picocom -b 115200 --flow n /dev/rfcomm0
```

逐项记录：

- [ ] Linux SDP browse 能发现 `SerialPort (0x1101)` / `Btrtty Terminal`。
- [ ] `rfcomm -r connect` 成功、生成 `/dev/rfcomm0`，且 picocom 打开前没有 TTY echo 回环。
- [ ] picocom 出现 bash/tmux prompt。
- [ ] `stty size` 显示 Mac 启动时指定的行列。
- [ ] `echo hello` 正常。
- [ ] Ctrl-C 正常中断 foreground command。
- [ ] Ctrl-Z 正常挂起 foreground command。
- [ ] Tab completion 正常。
- [ ] arrows/history 正常。
- [ ] UTF-8 中文正常。
- [ ] `vim`、`less` 基本操作正常。
- [ ] `$TERM` 为 `xterm-256color`，24-bit 色块肉眼正常。
- [ ] tmux 内 24-bit 色块正常，无颜色降级。
- [ ] Linux disconnect 后 Mac 不 crash，并返回 waiting。
- [ ] reconnect 后重新出现 shell。
- [ ] 先停止 Linux 自动重试但保持一个 active client，再对 Mac btrtty 发 SIGINT：Mac 等 outstanding write 和真实 channel-close callback，随后才移除 SDP/listener；Linux `/dev/rfcomm0` 被断开。
- [ ] Linux 执行 `rfcomm release` 并确认没有新连接进程后，停止并重新启动 btrtty；重新连接不出现 `Connection refused`。
- [ ] 连续执行 20 次 connect → disconnect → reconnect，不需要重启 btrtty、开关 Bluetooth controller 或清理 macOS 全局蓝牙状态。
- [ ] tmux reconnect 后 session 内容保留。
- [ ] `yes` 大量输出时 RSS 稳定在有界范围，停止后能恢复交互。
- [ ] 空闲后最多首个 `256 B` chunk 立即到达，不出现累计的 8 KiB burst。
- [ ] 连续输出及立即重连后的输出按配置的 KiB/s 调度，reconnect 不重置尚未到期的发送进度。
- [ ] 未配对设备不能得到 shell。
- [ ] 从 Mac 配对列表移除设备后，该设备 reconnect 被拒绝。
- [ ] 保存 btrtty stderr 与 macOS Bluetooth 日志，确认最终关闭严格按照 channel-close callback → remove SDP → listener unregister 顺序发生。

最后检查默认日志中没有 terminal payload，并保存 Mac 版本、Linux 发行版、BlueZ 版本、Bluetooth adapter 型号和实际 RFCOMM channel。

## 人工前置条件与平台边界

最终停止 btrtty 前，操作者负责停止 Linux 上所有新的 RFCOMM connect/retry。IOBluetooth 没有公开接口枚举 `bluetoothd` 中尚未打开、尚未交付给应用的 pending incoming connection；本验收不声称程序能够自动 drain 该不可见状态。

启动时只允许创建并回滚本次进程拥有的资源。禁止关闭设备级 ACL、删除配对关系、切换 Bluetooth controller、杀死 `bluetoothd`、删除系统偏好文件或执行 publish-remove-republish 清理循环。若历史版本已经污染 daemon 状态，先人工关闭/打开一次 macOS Bluetooth，再开始本清单。
