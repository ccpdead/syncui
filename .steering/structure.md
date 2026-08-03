# 项目结构与约定

## 目录结构

```text
sync_ui/
├── .npmrc                   ← npm 镜像（npmmirror，仅本项目生效）
├── .cargo/config.toml       ← crates 镜像（rsproxy，仅本项目生效）
├── run-dev.sh               ← 开发启动脚本（设 WebKit 环境变量后 npm run tauri dev）
├── build-release.sh         ← 打包脚本（npm run tauri build → .deb / .AppImage）
├── package.json             ← 前端依赖与 npm scripts（dev/build/preview/tauri）
├── vite.config.ts           ← 固定 dev 端口 1420、忽略 src-tauri 变更
├── tsconfig*.json
├── index.html
├── src/                     ← 前端（React + TypeScript）
│   ├── main.tsx             ← React 挂载入口
│   ├── App.tsx              ← 主界面：拖拽区 / 选项 / 差异表 / 进度 / 日志（唯一大组件）
│   ├── api.ts               ← 与 Rust 命令的类型化桥接层（invoke 封装 + 全部类型定义）
│   └── styles.css
└── src-tauri/               ← 后端（Rust）
    ├── Cargo.toml           ← crate 名 syncui（deb 包名），lib 名 sync_ui_lib
    ├── tauri.conf.json      ← 应用配置：identifier com.syncui.app、窗口、bundle
    ├── build.rs
    ├── capabilities/
    │   └── default.json     ← Tauri 权限配置（文件访问 / 对话框等）
    ├── gen/、icons/、target/ ← 生成物 / 图标 / 构建产物（勿手改）
    └── src/
        ├── main.rs          ← 二进制入口，调用 sync_ui_lib::run()
        ├── lib.rs           ← Tauri 命令注册 + 进度事件发射 + 快照路径计算
        ├── engine.rs        ← 核心：扫描 / 三方对比 / 并行同步引擎（含单元测试）
        └── snapshot.rs      ← 快照持久化（双向同步基线，JSON）
```

## 关键模块 / 命令 / 事件入口

| 类型 | 名称 | 位置 | 说明 |
|------|------|------|------|
| Tauri 命令 | `compare_dirs` | `lib.rs` | 对比两目录，返回 `CompareResult`；`spawn_blocking` 跑重活 |
| Tauri 命令 | `sync_entries` | `lib.rs` | 并行执行勾选操作，完成后重建基线快照 |
| 事件 | `scan-progress` | `lib.rs` → 前端 | 扫描进度，`{ phase: "local"｜"remote", count }`，节流 ~120ms |
| 事件 | `sync-progress` | `lib.rs` → 前端 | 同步进度 `OpProgress`，节流 ~100ms，最后一条必发 |
| 引擎 | `compare_with_progress` / `apply_ops` / `build_snapshot` | `engine.rs` | 扫描剪枝、决策矩阵、并行 worker 池、基线重建 |
| 快照 | `load` / `save` / `Snapshot` | `snapshot.rs` | 基线读写，缺失或损坏视为空基线 |
| 前端桥接 | `compareDirs` / `syncEntries` | `api.ts` | `invoke` 封装 + 所有 TS 类型 |

## 关键约定

### 前后端契约（改任一端必须同步另一端）
- 新增/修改命令要三处同步：`lib.rs`（`#[tauri::command]` + `invoke_handler` 注册）、`api.ts`（类型 + `invoke` 封装）、`App.tsx`（调用）。
- 序列化用 serde `rename_all = "camelCase"`；Rust struct 字段（snake_case）到前端自动变 camelCase，`api.ts` 类型须与之一致（如 `relPath`、`deleteLocalCount`）。
- 动作枚举分两层：UI 展示用 `Action`（含 `same`/`conflict`），发给后端执行用 `Op`（`upload`/`download`/`delLocal`/`delRemote`）。

### 同步决策约定
- 镜像模式以本地为准；双向模式依赖基线 B 做三方对比，首次无基线时降级为镜像。
- 快照按 `blake3(local\0remote)` 哈希分文件存于应用配置目录 `snapshots/`，绝不写入被同步目录。Linux 路径：`~/.config/com.syncui.app/snapshots/`。

### 前端结构约定
- 主界面集中在单个 `App.tsx`；类型与后端调用集中在 `api.ts`，不要把 `invoke` 散落到组件里。

## 常见坑

- **忘记同步 camelCase 类型**：Rust 改了字段名/枚举，`api.ts` 未跟改 → 前端拿到 `undefined`。改一端务必改另一端。
- **删除/冲突项默认不勾选**：这是安全设计，别为了「省事」改成默认预选。
- **快照位置**：不要把基线写进用户目录；沿用 `snapshot_path()` 的哈希方案。
- **`src-tauri/` 被 Vite 监听**：`vite.config.ts` 已忽略，改前端热重载配置时勿破坏该忽略。
- **生成物目录**：`gen/`、`target/`、`dist/`、`node_modules/` 为产物，勿手工编辑或提交无关变更。
