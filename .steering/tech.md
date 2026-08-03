# 技术栈与常用命令

## ⚠️ 前置条件

改代码 / 运行前必须先具备：

- **Node.js ≥ 20** 与 **Rust ≥ 1.77（rustup）**，`node`、`npm`、`cargo` 都在 `PATH`。
- **Tauri Linux 系统库**（Debian/Ubuntu，一次性）：

```bash
sudo apt update
sudo apt install -y \
  libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libdbus-1-dev \
  libayatana-appindicator3-dev librsvg2-dev pkg-config
```

- **首次拉代码后**安装前端依赖（Rust crates 首次编译自动拉取）：

```bash
npm install
```

- **镜像加速**：`.npmrc`（npmmirror）、`.cargo/config.toml`（rsproxy）已配置国内镜像。境外环境如遇问题，可移除 `.cargo/config.toml` 中的 `replace-with` 还原官方源。
- **WebKit 白屏规避**：Linux 下须导出 `WEBKIT_DISABLE_DMABUF_RENDERER=1`、`WEBKIT_DISABLE_COMPOSITING_MODE=1`。`run-dev.sh` / `build-release.sh` 已自动设置；打包后的二进制在 `lib.rs::apply_webkit_workarounds()` 里兜底设置。手动 `npm run tauri dev` 时须自行导出。

## 核心技术栈

```text
Tauri v2         ← 桌面外壳 / 命令 & 事件桥接 / 打包
React 18 + TS 5  ← 前端 UI（Vite 5 构建，dev 端口固定 1420）
Rust 2021        ← 同步引擎（edition 2021，release 用 lto + opt-level "s" + strip）
walkdir          ← 目录遍历
blake3           ← 内容哈希（可选校验 + 快照 key）
filetime         ← 保留/设置 mtime
serde/serde_json ← 前后端序列化 + 快照持久化
tauri-plugin-dialog ← 文件/目录选择对话框
```

## 构建

```bash
# 前端类型检查 + 打包（一般由 tauri build 自动触发）
npm run build

# 完整发布包（Linux 产出 .deb / .AppImage）
./build-release.sh
```

产物路径：

```text
src-tauri/target/release/bundle/
├── deb/       ← SyncUI_0.1.0_amd64.deb（Package: syncui）
└── appimage/  ← SyncUI_0.1.0_amd64.AppImage
```

## 运行

```bash
# 开发模式（热重载前端 + 自动重编 Rust）
./run-dev.sh
# 等价于：npm run tauri dev

# 安装 / 运行发布包
sudo dpkg -r sync-ui 2>/dev/null || true
sudo dpkg -i src-tauri/target/release/bundle/deb/SyncUI_*.deb
# 或免安装运行 AppImage
chmod +x src-tauri/target/release/bundle/appimage/SyncUI_*.AppImage
./src-tauri/target/release/bundle/appimage/SyncUI_*.AppImage
```

## 测试

```bash
cd src-tauri
cargo test    # 引擎单元测试，覆盖三方决策矩阵、并行执行、坏符号链接容错
```

提交 PR 前须保证 `cargo test` 通过。

## 命令执行注意事项

- **首次编译较慢**：Rust 依赖 + Tauri 全量编译耗时，属正常，勿中途误判为卡死。
- **`tauri dev` 为长驻进程**：会持续占用终端（热重载 watcher），需要在独立终端启动，勿在同一阻塞调用里等待其「结束」。
- **不要提交生成物**：`target/`、`dist/`、`gen/`、`node_modules/` 不应带入变更。
- **端口 1420 固定**：Vite `strictPort`，被占用会直接失败；先释放端口而非改端口（Tauri devUrl 与之绑定）。
