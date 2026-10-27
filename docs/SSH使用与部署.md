# SyncUI 0.2 SSH 使用与部署

SSH 模式通过远端本地扫描生成清单，再用 rsync 直接传输选中的文件。选择远端真实目录，例如 `/home/nvidia/ros2_ws`；不要填写 `/run/user/.../gvfs/...` 挂载路径。原有本地与挂载目录模式继续可用。

## 首次连接

1. 两端安装 OpenSSH 和 rsync 3.2.0 或以上版本。Ubuntu/Debian 可自行执行 `sudo apt install openssh-client rsync`，远端还需运行 SSH 服务。应用不会执行 sudo。
2. 启动 SyncUI，选择本地目录，将“远端连接方式”改为“SSH 服务器”。
3. 填写主机或 `~/.ssh/config` 中的别名。用户名、端口、密钥文件留空时使用 SSH 配置，填写后覆盖对应配置。自动认证支持 ssh-agent；也可明确选择密钥或密码。
4. 点击“连接并检测”。首次连接核对主机指纹后接受。密钥有口令时，在弹窗中输入。主机密钥变化会阻止连接，应先向服务器管理员核实；应用不会自动替换记录。
5. 点击“部署 / 升级工具”。应用自动选择 Linux x86_64 或 ARM64 Agent，校验 SHA-256 和协议后切换版本。远端不需要 Rust、Node.js、Python、桌面或常驻服务。
6. 输入远端真实目录，或通过“浏览目录”选择。工具就绪后连接表单自动收起，可随时展开。
7. 设置模式、忽略规则和哈希选项，点击对比，检查并勾选变更，执行同步。SSH 每个方向使用一个 rsync 批次，并发选项只作用于挂载模式。

“记住密码”默认关闭。开启后使用当前 Linux 登录会话的 Secret Service（libsecret）保存，不写入设置 JSON。密钥环不可用或解锁取消时，仅使用本次会话输入并显示未保存提示；可用“删除保存的密码”清除。SSH 私钥仍由 OpenSSH 读取，应用不导入或保存其内容。

## 同步与编辑规则

- 后端保存比较结果并生成操作计划，前端只提交比较 ID、勾选路径和冲突策略。比较结果有效期 30 分钟；更换目录、服务器、比较选项或保存编辑后，需重新比较。
- 扫描错误、非 UTF-8 文件名、协议中断不会产生可执行清单。符号链接和特殊文件跳过，也不会据此传播删除。
- 上传、下载、删除依次执行。传输失败或取消会停止后续阶段；已完成文件不回滚，不自动重试写入。
- 使用精确 NUL 文件列表，只操作选中普通文件与必要父目录。不会执行整目录 `--delete`，不会使用 `--inplace`、`--append` 或保留断点文件。
- 删除只针对已选普通文件，不递归删除目录。文本编辑限 5 MiB，保存前验证读取版本，通过同目录临时文件替换。
- 同步前、每阶段前、阶段后和最终刷新检查版本。外部程序并发写入无法组成跨设备事务；检测到变化会要求重新比较。
- 正常结束后两端各扫描一次，复用清单验证结果、合并基线并刷新界面。不会再从前端自动发起第二轮完整对比。
- 成功并验证一致的文件更新基线；未选择、冲突和失败项保留历史。SSH 基线与挂载基线隔离，切换 SSH 后首次两端不同的文件显示为冲突。
- 首版全应用串行执行修改任务，防止同步和编辑交叉。初始一致但从未成功同步的文件尚无删除历史，首次单侧消失会按新增候选处理。

## 进度与日志

输出框约每 500 ms 更新 KB/s 或 MB/s。SSH “已传输”统计 rsync 与 SSH 之间双向协议数据，含 rsync 开销，不含 SSH 加密封装。“文件处理量”来自 rsync 重建数据量，可能包含目标端复用的内容；它与网络传输量不同。取消时计数是最后成功上报的值。

日志位于 `~/.config/com.syncui.app/diagnostics/`，每次连接、比较、同步各有日志。包含认证/复用耗时、清单接收与扫描、哈希决策、rsync 退出码与字节数、删除、基线保存、最终刷新和总耗时。日志不包含密码或口令，但可能包含目录和文件名。

设置位于同目录的 `settings.json` 与 `ssh_profiles.json`，服务器指纹记录在 `ssh_known_hosts`，基线在 `snapshots/`。保存的凭据位于系统密钥环，以连接参数导出的标识查询。旧设置未包含 SSH 字段时默认读取为挂载模式。

## 手动部署 Agent

分别提供 `syncui-agent-0.2.0-x86_64.tar.gz` 和 `syncui-agent-0.2.0-aarch64.tar.gz`。在远端解压对应包，运行：

```bash
sha256sum -c SHA256SUMS
./syncui-agent --version
install -d "$HOME/.local/share/syncui/agent/0.2.0-manual"
install -m 700 syncui-agent "$HOME/.local/share/syncui/agent/0.2.0-manual/syncui-agent"
ln -s 0.2.0-manual "$HOME/.local/share/syncui/agent/.current-manual"
mv -Tf "$HOME/.local/share/syncui/agent/.current-manual" "$HOME/.local/share/syncui/agent/current"
```

输出应包含 `protocol=1`。GUI 部署使用版本加摘要的独立目录，并先自检再原子切换 `current`；失败保留原版本。系统需有常见 shell 工具及 `sha256sum`。删除旧 Agent 版本请在任务结束、确认回退不再需要后手动处理。

## 源码构建

```bash
npm ci
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
./scripts/build-tools.sh
./run-dev.sh
# 发布 Linux 桌面安装包及两种架构 Agent
./build-release.sh
```

Agent 使用 musl 静态链接，无远端运行时依赖。桌面仍需原项目的 GTK/WebKit 构建依赖。Release 包包含本机架构 bridge 和两种 Agent。不能只执行 `npm run tauri build` 跳过工具构建，否则干净检出缺少资源。

## 开发接口

核心位于 `crates/sync-core`；工具位于 `crates/sync-tools`；桌面协调器、连接管理和凭据存储位于 `src-tauri/src/coordinator.rs`、`ssh.rs`、`credentials.rs`；前端类型集中在 `src/api.ts`。

Agent `serve` 每次处理一个 JSON Lines 请求。请求含 `version`、`id`、`root`、`action`；响应含相同版本/ID、`event`、`data`。清单每 256 项发送 `entries`，以 `done` 结束；`error` 为失败。最大帧 40 MiB。客户端在接到结束帧前保持 stdin 打开，关闭 stdin 通知取消。stdout 只输出协议。

协议操作包括 `hello`、`scan`、`stats`、`hashes`、`read`、`write`、`delete`、`browse`。根目录、相对路径、忽略规则只在 JSON 请求体中传递；远程执行命令使用固定工具路径和 shell 参数转义。

## 参考

- [OpenSSH 手册](https://man.openbsd.org/ssh.1)：配置继承、ControlMaster 与 SSH_ASKPASS。
- [rsync 官方手册](https://download.samba.org/pub/rsync/rsync.1)：精确清单、远程 shell、差量传输与进度含义。
- [Secret Service 规范](https://specifications.freedesktop.org/secret-service/latest-single/)：登录会话凭据存储与解锁。
