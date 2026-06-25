# SyncUI · 跨平台目录对比与同步

一个基于 **Tauri v2（Rust + React/TS）** 的桌面应用：把本地目录和远程/挂载目录
（sftp、smb 等挂载点）拖进窗口，对比差异，勾选后一键同步。无需在终端敲 `rsync`，
也不用复制粘贴时逐个点"覆盖/跳过"。

## 核心设计

挂载后的远程目录对程序来说就是一个普通文件系统路径，因此引擎只需操作两个路径，
不关心底层是 sftp/smb/nfs。同步支持两种模式：

- **镜像 mirror（默认）**：单向，让远程与本地一致（本地→远程推送）。
- **双向 twoway**：基于"上次同步快照"的三方对比（本地/远程/基线），能准确区分
  「删除」与「新增」，并识别冲突。

复制采用临时文件 + 原子重命名，并保留源文件 mtime 以保证**幂等**。

```text
三方对比 (twoway)    L=本地  R=远程  B=上次同步快照
  L vs B   R vs B        动作
  变了      没变          ↑ 上传
  没变      变了          ↓ 下载
  没了      没变          ✗ 删除（确认是删除而非新增）
  都变了                  ⚠ 冲突 → 按策略（较新/本地/远程/跳过）

React 前端 (拖拽/动作列表/进度)
        │  invoke / event
        ▼
Tauri 命令层  src-tauri/src/lib.rs
        │  compare_dirs / sync_entries（scan-progress / sync-progress 事件）
        ▼
同步引擎      src-tauri/src/engine.rs   +   快照  src-tauri/src/snapshot.rs
        ├── scan()         ← 遍历，剪枝忽略目录，容错跳过坏项
        ├── compare()      ← 镜像/三方决策 → Upload/Download/Delete/Conflict
        ├── apply_ops()    ← 并行 worker 池执行，原子复制
        └── build_snapshot ← 同步后重建基线
```

## 目录结构

```text
sync_ui/
├── environment.yml          ← conda 环境定义（rust/node/webkit 全部隔离）
├── .npmrc                   ← npm 淘宝镜像（仅本项目）
├── .cargo/config.toml       ← crates 镜像 + 单线程编译（规避 LLVM 段错误）
├── package.json / vite.config.ts / tsconfig*.json
├── index.html
├── src/                     ← 前端
│   ├── main.tsx
│   ├── App.tsx              ← 主界面：拖拽区/选项/差异表/进度/日志
│   ├── api.ts               ← 与 Rust 命令的类型化桥接
│   └── styles.css
├── src-tauri/               ← 后端
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/default.json
│   ├── icons/
│   └── src/
│       ├── main.rs
│       ├── lib.rs           ← Tauri 命令 + 进度事件
│       └── engine.rs        ← 扫描/对比/同步引擎（含单元测试）
├── run-dev.sh               ← 开发模式启动
└── build-release.sh         ← 打包 .deb/.AppImage
```

## 环境与运行

环境通过 conda 隔离，**不污染本地系统**。

```bash
# 1) 创建环境（首次）
conda env create -f environment.yml

# 2) 开发模式（热重载 + 弹出窗口）
./run-dev.sh

# 3) 打包发布版（Linux 产出 .deb / .AppImage）
./build-release.sh
```

## 已实现

- [x] Rust 同步引擎：扫描（剪枝/容错）、镜像 + 三方双向对比、原子复制、保留 mtime
- [x] 上次同步快照（基线持久化于应用配置目录，按本地↔远程哈希分文件）
- [x] 动作模型：上传/下载/删本地/删远程/冲突/一致
- [x] 冲突策略：较新优先 / 用本地 / 用远程 / 跳过
- [x] 并行同步（有界 worker 池，并发数可调）+ 增量哈希缓存
- [x] 异步命令（不卡界面）+ 扫描/同步实时进度
- [x] 引擎单元测试（6 项，含三方决策矩阵、并行执行、坏符号链接）
- [x] GUI：双拖拽区、模式/冲突/并发/哈希/忽略选项、动作列表、进度、日志
- [x] conda 隔离环境，跨平台工程结构（Ubuntu 已验证）

## 后续计划（Phase 2）

- [ ] 自动同步：启动时 / 定时 / 文件变更监听（notify）
- [ ] 同步任务保存与一键复用（本地/远程/选项持久化）+ 托盘
- [ ] 差异文件树视图（按目录批量勾选）
- [ ] glob 忽略规则（gitignore 风格）与 `.syncignore`
- [ ] 超大文件分块复制 + 断点续传；保留双份冲突
- [ ] Windows 打包验证（.exe / .msi）与路径/盘符回归

## 环境备注（本机踩坑记录）

- conda-forge 的 rustc(LLVM) 在本机**并行 codegen 会 SIGSEGV**，已在
  `.cargo/config.toml` 固定 `jobs = 1` 并在脚本里设 `RUST_MIN_STACK`。
- Tauri 在 Linux 依赖 webkit2gtk/gtk3 等，已全部通过 conda-forge 安装
  （含补装的 `zlib`、`expat`），系统层无需 apt。
