# SyncUI · 跨平台目录对比与同步

一个基于 **Tauri v2（Rust + React/TS）** 的桌面应用：把本地目录和远程/挂载目录
（sftp、smb 等挂载点）拖进窗口，对比差异，勾选后一键同步。无需在终端敲 `rsync`，
也不用复制粘贴时逐个点"覆盖/跳过"。

## 核心设计

挂载后的远程目录对程序来说就是一个普通文件系统路径，因此引擎只需操作两个路径，
不关心底层是 sftp 还是 smb。同步采用**基于元数据的对比**（大小 + 修改时间，
可选内容哈希），只复制有差异的文件，并在复制后保留源文件修改时间以保证**幂等**。

```text
React 前端 (拖拽/差异列表/进度)
        │  invoke / event
        ▼
Tauri 命令层  src-tauri/src/lib.rs
        │  compare_dirs / sync_entries(+sync-progress 事件)
        ▼
同步引擎      src-tauri/src/engine.rs
        ├── scan()              ← 遍历目录，记录 size+mtime
        ├── compare()           ← 生成 新增/修改/远程独有/一致
        └── copy_file_atomic()  ← 临时文件+原子重命名+保留 mtime
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

## 已实现（M1 + M2 + M3 部分）

- [x] Rust 同步引擎：扫描、对比（新增/修改/远程独有/一致）、原子复制、保留 mtime
- [x] 忽略规则、可选内容哈希校验
- [x] 引擎单元测试（4 项，全部通过）
- [x] GUI：双拖拽区（支持系统文件夹拖入 + 浏览按钮）
- [x] 差异列表：分组、颜色、勾选、全选/清空
- [x] 同步执行 + 实时进度条 + 日志，可选"同步删除"
- [x] conda 隔离环境，跨平台工程结构（Ubuntu 已验证）

## 后续计划（M4 / M5）

- [ ] Windows 上打包验证（.exe / .msi）与路径/盘符兼容回归
- [ ] 同步任务保存与一键复用（本地/远程/过滤/策略持久化）
- [ ] 删除前二次确认 + 回收站/备份
- [ ] 定时同步 / 文件变更监听
- [ ] 双向同步与冲突合并

## 环境备注（本机踩坑记录）

- conda-forge 的 rustc(LLVM) 在本机**并行 codegen 会 SIGSEGV**，已在
  `.cargo/config.toml` 固定 `jobs = 1` 并在脚本里设 `RUST_MIN_STACK`。
- Tauri 在 Linux 依赖 webkit2gtk/gtk3 等，已全部通过 conda-forge 安装
  （含补装的 `zlib`、`expat`），系统层无需 apt。
