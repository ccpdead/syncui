# SyncUI

> 跨平台目录对比与同步桌面应用 · Tauri v2 + React + Rust

[![Tauri](https://img.shields.io/badge/Tauri-v2-blue?logo=tauri)](https://tauri.app/)
[![React](https://img.shields.io/badge/React-18-61DAFB?logo=react)](https://react.dev/)
[![Rust](https://img.shields.io/badge/Rust-2021-orange?logo=rust)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

把本地目录和远程/挂载目录拖进窗口，对比差异，勾选后一键同步。无需手敲 `rsync`，
也不用复制粘贴时逐个点"覆盖/跳过"。

挂载后的远程目录（sftp、smb、nfs 等）对程序而言就是普通文件系统路径——引擎只操作两个路径，
不关心底层协议。

---

## 目录

- [功能特性](#功能特性)
- [截图](#截图)
- [架构概览](#架构概览)
- [快速开始](#快速开始)
  - [前置依赖](#前置依赖)
  - [获取代码](#获取代码)
  - [安装依赖](#安装依赖)
  - [开发模式](#开发模式)
  - [打包发布](#打包发布)
- [使用说明](#使用说明)
- [同步模式详解](#同步模式详解)
- [目录结构](#目录结构)
- [开发者指南](#开发者指南)
- [常见问题](#常见问题)
- [路线图](#路线图)
- [贡献指南](#贡献指南)
- [License](#license)

---

## 功能特性

- **镜像同步（mirror）**：单向，将本地推送至远程，保持远端与本地一致。
- **双向同步（twoway）**：基于"上次同步快照"做三方对比（本地 / 远程 / 基线），
  能准确区分「删除」与「新增」，并识别冲突。
- **冲突策略**：较新优先 / 强制用本地 / 强制用远程 / 跳过，四种策略自由选择。
- **拖拽选目录**：两端目录直接拖入，或点击浏览器选择。
- **差异列表**：按动作（上传 / 下载 / 删除 / 冲突 / 一致）可视化展示，支持逐项勾选。
- **实时进度**：扫描与同步均有进度条和日志输出，不卡界面。
- **原子写入**：复制采用临时文件 + 原子重命名，保留源 mtime，保证幂等。
- **哈希校验**：可选 BLAKE3 内容哈希，精确判断文件是否真正变更。
- **并行执行**：有界 worker 池并发同步，并发数可调（默认 4）。
- **忽略规则**：逗号分隔的名称列表，自动剪枝 `.git`、`node_modules` 等。

---

## 截图

> TODO：合并 PR 时附上截图 `docs/screenshot.png`。

---

## 架构概览

```text
React 前端（拖拽 / 选项 / 差异表 / 进度 / 日志）
        │  invoke / event
        ▼
Tauri 命令层  src-tauri/src/lib.rs
        │  compare_dirs()   ─── scan-progress 事件
        │  sync_entries()   ─── sync-progress 事件
        ▼
同步引擎  src-tauri/src/engine.rs
        ├── scan()         ← 遍历目录，剪枝忽略项，容错跳过坏项
        ├── compare()      ← 镜像 / 三方决策 → Upload / Download / Delete / Conflict
        ├── apply_ops()    ← 并行 worker 池执行，原子复制，保留 mtime
        └── build_snapshot ← 同步后重建基线（持久化至 AppConfig 目录）
快照层  src-tauri/src/snapshot.rs
        └── 按本地↔远程路径哈希分文件存储，实现双向基线持久化
```

---

## 快速开始

### 前置依赖

| 依赖 | 说明 |
|------|------|
| **Linux（Ubuntu 20.04+ / 22.04+）** | 当前已在 Ubuntu 验证；macOS / Windows 理论支持，待验证 |
| **Rust（rustup）** | ≥ 1.77，通过 [rustup](https://rustup.rs/) 安装 |
| **Node.js** | ≥ 20，用于前端构建与 Tauri CLI |
| **系统库（apt）** | WebKitGTK、GTK、OpenSSL 等 Tauri Linux 构建依赖 |

#### 1. 安装系统依赖（Debian / Ubuntu）

```bash
sudo apt update
sudo apt install -y \
  libwebkit2gtk-4.1-dev \
  build-essential \
  curl \
  wget \
  file \
  libxdo-dev \
  libssl-dev \
  libdbus-1-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev \
  pkg-config
```

> 其他发行版请参考 [Tauri Prerequisites](https://v2.tauri.app/start/prerequisites/)。

#### 2. 安装 Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# 重新打开终端，或执行：
source "$HOME/.cargo/env"
rustc --version   # 确认 ≥ 1.77
```

#### 3. 安装 Node.js（≥ 20）

任选其一：

```bash
# 方式 A：NodeSource（Debian / Ubuntu）
curl -fsSL https://deb.nodesource.com/setup_20.x | sudo -E bash -
sudo apt install -y nodejs

# 方式 B：已有 nvm
nvm install 20
nvm use 20

node -v   # 确认 ≥ v20
npm -v
```

### 获取代码

```bash
git clone https://github.com/your-username/sync_ui.git
cd sync_ui
```

### 安装依赖

首次克隆后执行一次，安装前端 npm 包（Rust crates 会在首次编译时自动拉取）：

```bash
npm install
```

> **网络提示**：`.npmrc` 已配置 npmmirror；`.cargo/config.toml` 已配置 rsproxy 镜像加速 crates 下载。
> 如在境外，可删除 `.cargo/config.toml` 中的 `replace-with` 配置还原官方源。

### 开发模式

```bash
./run-dev.sh
```

该脚本会设置必要的环境变量，并以热重载模式启动 Tauri 窗口。
修改 `src/` 下的 React 代码后，前端即时刷新；修改 Rust 代码后会自动重新编译。

也可直接：

```bash
npm run tauri dev
```

### 打包发布

Linux 产出 `.deb` 和 `.AppImage`：

```bash
./build-release.sh
```

产物路径：

```text
src-tauri/target/release/bundle/
├── deb/    ← sync-ui_0.1.0_amd64.deb
└── appimage/  ← sync-ui_0.1.0_amd64.AppImage
```

安装 `.deb`：

```bash
sudo dpkg -i src-tauri/target/release/bundle/deb/sync-ui_*.deb
```

或直接运行 AppImage（无需安装）：

```bash
chmod +x src-tauri/target/release/bundle/appimage/sync-ui_*.AppImage
./src-tauri/target/release/bundle/appimage/sync-ui_*.AppImage
```

---

## 使用说明

```text
1. 启动应用后，将「本地目录」拖入左侧拖放区
   （或点击"浏览…"按钮选择）

2. 将「远程/挂载目录」拖入右侧拖放区
   sftp/smb/nfs 挂载点同样适用

3. 按需调整选项：
   ┌─────────────┬────────────────────────────────────────┐
   │ 模式         │ 镜像（本地→远程）/ 双向（三方对比）         │
   │ 冲突策略      │ 较新优先 / 用本地 / 用远程 / 跳过         │
   │ 并发数        │ 1–32（默认 4）                          │
   │ 哈希校验      │ 开启后用 BLAKE3 精确判断是否变更           │
   │ 忽略规则      │ 逗号分隔的目录/文件名，默认过滤常见编译产物  │
   └─────────────┴────────────────────────────────────────┘

4. 点击「对比 Compare」，等待扫描完成

5. 差异列表中勾选要同步的条目
   - 上传/下载项默认预选
   - 删除和冲突项需手动勾选（防止误操作）

6. 点击「同步选中 (n)」执行，进度条和日志实时更新

7. 同步完成后自动重新对比，确认结果
```

---

## 同步模式详解

### 镜像模式（mirror）

单向推送，让远程与本地完全一致。本地新增 → 上传；本地删除 → 删远程。

### 双向模式（twoway）

基于上次同步快照（基线 B）做三方对比：

```text
L（本地）vs B    R（远程）vs B     决策
  变了              没变           ↑ 上传
  没变              变了           ↓ 下载
  删了              没变           ✗ 删除远程（确认是删除而非另一端新增）
  没变              删了           ✗ 删除本地
  两边都变了                       ⚠ 冲突 → 按冲突策略处理
```

快照存储位置（Linux）：`~/.config/com.syncui.app/snapshots/`

---

## 目录结构

```text
sync_ui/
├── .npmrc                   ← npm 配置（仅本项目生效）
├── .cargo/config.toml       ← crates 镜像配置
├── run-dev.sh               ← 开发模式启动脚本
├── build-release.sh         ← 打包 .deb / .AppImage 脚本
├── package.json
├── vite.config.ts
├── tsconfig*.json
├── index.html
├── src/                     ← 前端（React + TypeScript）
│   ├── main.tsx
│   ├── App.tsx              ← 主界面：拖拽区 / 选项 / 差异表 / 进度 / 日志
│   ├── api.ts               ← 与 Rust 命令的类型化桥接层
│   └── styles.css
└── src-tauri/               ← 后端（Rust）
    ├── Cargo.toml
    ├── tauri.conf.json
    ├── capabilities/
    │   └── default.json     ← Tauri 权限配置
    └── src/
        ├── main.rs          ← 入口
        ├── lib.rs           ← Tauri 命令注册 + 进度事件发射
        ├── engine.rs        ← 核心：扫描 / 对比 / 同步引擎（含单元测试）
        └── snapshot.rs      ← 快照持久化（双向同步基线）
```

---

## 开发者指南

### 运行单元测试

```bash
cd src-tauri
cargo test
```

引擎包含 6 项测试，覆盖三方决策矩阵、并行执行和坏符号链接容错。

### 添加 Tauri 命令

1. 在 `src-tauri/src/lib.rs` 中用 `#[tauri::command]` 定义函数
2. 在 `invoke_handler` 中注册
3. 在 `src/api.ts` 中添加对应的 TypeScript 类型和 `invoke` 调用

### 修改权限

`src-tauri/capabilities/default.json` 控制 Tauri 的权限范围（文件访问、对话框等），
按需调整后重启开发服务器生效。

### 环境变量说明

| 变量 | 用途 |
|------|------|
| `WEBKIT_DISABLE_DMABUF_RENDERER=1` | 修复部分 Linux 驱动下 WebKit 白屏问题 |
| `WEBKIT_DISABLE_COMPOSITING_MODE=1` | 同上，强制使用软件合成路径 |

---

## 常见问题

**Q: 运行 `./run-dev.sh` 提示 `npm: command not found` / `cargo: command not found`**

请确认已按[前置依赖](#前置依赖)安装 Node.js 与 Rust，并重新打开终端使 `PATH` 生效。

---

**Q: 编译时报缺少 `webkit2gtk-4.1` / `pkg-config` 相关错误**

系统依赖未装全。在 Debian / Ubuntu 上重新执行[前置依赖](#前置依赖)中的 `apt install` 命令。

---

**Q: 应用窗口空白 / 白屏**

WebKitGTK 在某些 Linux 驱动下 DMABUF 渲染路径异常。`run-dev.sh` 已设置
`WEBKIT_DISABLE_DMABUF_RENDERER=1` 和 `WEBKIT_DISABLE_COMPOSITING_MODE=1`。
如手动启动时遇到白屏，请确保这两个变量已导出。

---

**Q: 双向模式第一次同步没有基线怎么处理？**

首次双向同步时不存在快照，引擎会降级为镜像模式（以本地为准），
同步完成后自动生成基线，后续即可正常三方对比。

---

**Q: 如何在 smb / nfs 挂载目录上使用？**

先用系统工具挂载远端：

```bash
# smb 示例
sudo mount -t cifs //server/share /mnt/remote -o username=user,password=pass

# nfs 示例
sudo mount -t nfs server:/export /mnt/remote
```

然后把 `/mnt/remote` 拖入 SyncUI 的右侧拖放区即可。

---

## 路线图

- [ ] 同步任务持久化（保存本地/远程路径 + 选项，一键复用）+ 系统托盘
- [ ] 自动同步：定时触发 / 文件变更监听（`notify` crate）
- [ ] 差异文件树视图（按目录层级批量勾选）
- [ ] glob 忽略规则（`.syncignore`，gitignore 风格）
- [ ] 超大文件分块复制 + 断点续传
- [ ] 冲突双份保留（`file.conflict.local` / `file.conflict.remote`）
- [ ] Windows 打包验证（`.exe` / `.msi`）
- [ ] macOS 打包验证（`.dmg`）

---

## 贡献指南

欢迎 PR 和 Issue！

1. Fork 本仓库，基于 `main` 新建分支（`feat/your-feature` 或 `fix/your-bug`）
2. 按照[快速开始](#快速开始)搭建环境
3. 修改代码，确保 `cargo test` 通过
4. 提交 PR，描述变更内容和测试方法

Bug 报告请包含：操作系统版本、Node / Rust 版本、复现步骤和错误日志。

---

## License

[MIT](LICENSE) © 2024 Contributors
