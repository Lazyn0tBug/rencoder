# 在别的宿主机上跑测试

这份文档说清：在什么机器上跑、跑什么、哪些地方**按平台不一样**。

## 需要什么

| 东西 | 需要吗 | 缺了会怎样 |
|------|--------|------------|
| Rust stable | **必须** | —— |
| `ffmpeg` / `ffprobe` | 集成测试要 | 集成测试**跳过**（`skip_if_no_ffmpeg` ✅），其余照跑 |
| webview 开发包（Linux） | 只有 GUI 那趟要 | GUI 编不过；core 那趟照跑 |
| NVIDIA GPU | 不要 | NVENC 的实机验收（§8 row 87）跑不了，其余不受影响 |

`c-probe`（原生 libavformat 探测）是**可选特性** ✅ —— 默认走 `ffprobe` 子进程 ✅。
核心闸门不开它 ✅，所以核心闸门验的正是用户实际会跑的那条路径 ✅。
但它**不是没人编** ✅ ✅：另有一个 `c-probe (ubuntu)` 的 job ✅ ✅，每次 push 都编一遍 ✅ ✅
（装 `pkg-config` + `libavformat-dev` ✅ ✅）——没人编的 feature 会静默腐烂 ✅ ✅，
一个改错的 C 签名要等到某人在本地开 feature 才发现 ✅ ✅。

## 闸门

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 各宿主机

| 宿主机 | 谁跑 | 覆盖 |
|--------|------|------|
| macOS | 本地开发 + CI | 全部（lib + CLI + GUI）✅ |
| Linux | CI | 全部（lib + CLI + GUI）✅ |
| Windows | CI | 全部（lib + CLI + GUI ✅ ✅——GUI 没有 Unix 专属代码 ✅ ✅，§8 row 35 ✅ ✅） |

CI 的 job 是分开的 ✅：`core`（lib + CLI，三台宿主机 ✓）、`c-probe`（可选 FFI ✅，Linux ✅）与
`ui`（整个 workspace ✅，macOS + Linux ✅）。分开是因为 webview 依赖比核心重得多 ✅ ——
它坏了不该把核心闸门一起带下水 ✅；`c-probe` 要系统库 ✅ ✅，同理 ✅ ✅。

## 按平台不一样的地方

- **Windows 的信号处理**：`rencoder_cli::signal` 的 Windows 分支用
  `SetConsoleCtrlHandler` ✅（Unix 用 `signal-hook` ✅）。这个分支以前**从未被编译过**
  ✅（写的时候手边没有 Windows 工具链 ✅）——CI 的 Windows job 现在会编它 ✅ ✅。
- **主动退出键 Ctrl-D** ✅ ✅：Unix 上终端要切成**非规范模式**才能收到它 ✅ ✅
  （规范模式下 Ctrl-D 是行规程的 EOF ✅ ✅，应用收不到 ✅ ✅）；Windows 上它不是控制台
  事件 ✅ ✅，靠 `ReadConsoleInput` 读按键事件 ✅ ✅。所以这个键**没法在 CI 里端到端测**
  ✅ ✅——决策逻辑（哪些键算退出 ✅ ✅）有单元测试 ✅ ✅，读键的那一段靠人手验证 ✅ ✅。
- **控制台事件只到 app，不到编码器**：Windows 的控制台事件发给整个**进程组** ✅ ✅，
  Unix 的信号只发给一个进程 ✅ ✅。所以 lib 启动 ffmpeg 时在 Windows 上给它单独建组
  （`CREATE_NEW_PROCESS_GROUP` ✅ ✅，关在 `rencoder_lib::platform::process` ✅ ✅）——
  否则 Ctrl-C 会把 ffmpeg 一起杀掉 ✅ ✅，那个文件会被记成 failed ✅ ✅ 而不是整批
  cancelled ✅ ✅，退出码也不是 130 ✅ ✅。
- **文件系统大小写**：Linux 区分大小写 ✅，macOS/Windows 不区分 ✅。代码里一律按
  **不区分大小写**比较 ✅（`platform::fs_semantics` ✅，`utils::fs::is_same_file` ✅ 用它）
  ——在 Linux 上偏保守 ✅（把两个不同的文件当成同一个 ✅），不会出错 ✅ ✅。
- **mtime 粒度**：预演失效要比较输入的"大小 + mtime" ✅（§3.10.5 ✅）。某些文件系统
  的 mtime 精度只到秒 ✅ ——同秒内改内容且大小不变时可能漏判 ✅，大小变了就能抓住 ✅ ✅。
- **文件锁**：守卫用的是内核文件锁 ✅（Unix 上是 `flock` ✅，Windows 上是打开时的共享模式
  ✅ ✅）——内核在进程死时释放 ✅，所以"进程被 kill"不会留下死锁 ✅ ✅。机制不同 ✅ ✅，
  契约相同 ✅ ✅：五条性质 ✅ ✅（原子抢占 / 死亡自动释放 / 可探测不干扰 / 不挡读 / 按路径
  定位 ✅ ✅）由**同一份**契约测试在两个平台上断言 ✅ ✅（`platform::lock` ✅ ✅）。

## 手动在别的机器上跑

```bash
git clone <repo> && cd rencoder
rustup update stable

# Debian / Ubuntu
sudo apt-get install -y ffmpeg            # 集成测试要
sudo apt-get install -y libwebkit2gtk-4.1-dev libxdo-dev \
  libayatana-appindicator3-dev librsvg2-dev   # 只有 GUI 要

# macOS
brew install ffmpeg

cargo test --workspace
```

Windows 上 GUI 编不过是**预期**的 ✅（§8 row 35 ✅）✅ ——只跑：

```bash
cargo test -p rencoder_lib -p rencoder_cli
```

## 还没被别的主机验证的东西

- **真实 GPU 的 NVENC 验收**（§8 row 87 ✅）——需要一台带 NVIDIA 显卡的机器 ✅，
  CI 跑不了 ✅（GitHub 的 runner 没有 GPU ✅）✅。
- **Windows 的 GUI 能不能真的开窗** ✅ ❌——CI 只能证"编得过、测试过" ✅ ✅（测试无头的 ✅ ✅），"窗口开得起来"要人在 Windows 上跑一遍 ✅ ✅。
