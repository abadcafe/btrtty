# Hardware acceptance

日期：2026-08-16

自动化环境：macOS 15.7.7（arm64）

## 已自动验证

```bash
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
./scripts/package.sh
```

- 38 项测试通过。
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
- 关闭屏障状态机已覆盖：等待 outstanding write、等待实际 channel-close callback、session close 升级为 shutdown、remote close 后允许下一连接。

## 本机 Bluetooth smoke test

本机（Bluetooth 地址 `10:B5:88:4D:11:18`）成功发布标准 SPP service，IOBluetooth 动态分配 RFCOMM channel 1：

```text
Published Bluetooth Serial Port
Bluetooth address: 10:B5:88:4D:11:18
RFCOMM channel: 1
Waiting for paired client...
```

- [x] macOS Bluetooth TCC 授权通过。
- [x] 启动输出显示本机 Bluetooth controller address。
- [x] 标准 SPP SDP record 发布成功并取得动态 RFCOMM channel。
- [x] SIGINT/SIGTERM 正常退出，随后可立即重新发布同一服务。
- [x] 重复发布/停止没有残留进程或 channel registration 冲突。
- [x] 第二个 btrtty 实例在发布 SDP 前被排他锁拒绝。

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
- [ ] client 连接期间对 Mac btrtty 发 SIGINT：Mac 等到 Linux `/dev/rfcomm0` 实际断开后才退出；立即启动新 btrtty 并连接，不出现 `Connection refused`。
- [ ] 连续执行 20 次 connect → disconnect → reconnect，不需要重启 btrtty、开关 Bluetooth controller 或清理 macOS 全局蓝牙状态。
- [ ] tmux reconnect 后 session 内容保留。
- [ ] `yes` 大量输出时 RSS 稳定在有界范围，停止后能恢复交互。
- [ ] 空闲后最多首个 `256 B` chunk 立即到达，不出现累计的 8 KiB burst。
- [ ] 连续输出及立即重连后的输出按配置的 KiB/s 调度，reconnect 不重置尚未到期的发送进度。
- [ ] 未配对设备不能得到 shell。
- [ ] 从 Mac 配对列表移除设备后，该设备 reconnect 被拒绝。
- [x] SIGINT/SIGTERM 后进程干净退出，服务可立即重新发布。

最后检查默认日志中没有 terminal payload，并保存 Mac 版本、Linux 发行版、BlueZ 版本、Bluetooth adapter 型号和实际 RFCOMM channel。
