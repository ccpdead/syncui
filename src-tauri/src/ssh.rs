use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener, process::CommandExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};
use sync_core::{
    bridge::{AuthReply, BridgeConfig, Message},
    endpoint::{self, Endpoint, Inventory, Manifest, Stamp, TextFile},
    protocol::{self, Frame, Operation, Request},
};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SshProfile {
    pub id: String,
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub identity_file: String,
    #[serde(default = "default_auth")]
    pub auth: String,
    #[serde(default)]
    pub remember_password: bool,
}
fn default_auth() -> String {
    "auto".into()
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionInfo {
    pub id: String,
    pub user: String,
    pub home: String,
    pub arch: String,
    pub host_key: String,
    pub rsync_ready: bool,
    pub agent_ready: bool,
    pub message: String,
}
#[derive(Default)]
pub struct SshState {
    pub connections: Mutex<BTreeMap<String, Arc<Connection>>>,
    pub pending: Arc<Mutex<BTreeMap<String, mpsc::Sender<Option<String>>>>>,
}

pub fn random_id() -> String {
    let mut b = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("OS random source");
    b.iter().map(|x| format!("{x:02x}")).collect()
}
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
pub fn credential_key(p: &SshProfile) -> String {
    blake3::hash(format!("{}\0{}\0{:?}\0{}", p.host, p.user, p.port, p.auth).as_bytes())
        .to_hex()
        .to_string()
}
fn validate(p: &SshProfile) -> Result<(), String> {
    if p.host.is_empty()
        || p.host.starts_with('-')
        || p.host.chars().any(|c| c.is_control() || c.is_whitespace())
        || p.host.contains('@')
        || p.port == Some(0)
        || !["auto", "password", "key"].contains(&p.auth.as_str())
    {
        return Err("SSH 主机、端口或认证方式无效".into());
    }
    if [&p.user, &p.identity_file, &p.id]
        .iter()
        .any(|v| v.contains('\0') || v.contains('\n'))
    {
        return Err("SSH 配置含无效字符".into());
    }
    Ok(())
}
fn resource(app: &AppHandle, name: &str) -> Result<PathBuf, String> {
    let archive = format!("{name}.gz");
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join(&archive);
    let packaged = app
        .path()
        .resource_dir()
        .map_err(|e| e.to_string())?
        .join("resources")
        .join(&archive);
    if packaged.is_file() {
        Ok(packaged)
    } else if cfg!(debug_assertions) && dev.is_file() {
        Ok(dev)
    } else {
        Err(format!(
            "安装包缺少 {name}；开发环境请运行 scripts/build-tools.sh"
        ))
    }
}
fn resource_bytes(app: &AppHandle, name: &str) -> Result<Vec<u8>, String> {
    let path = resource(app, name)?;
    let archive = fs::File::open(path).map_err(|e| format!("无法读取 {name}: {e}"))?;
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(archive)
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("无法解压 {name}: {e}"))?;
    if bytes.len() > 32 * 1024 * 1024 || !bytes.starts_with(b"\x7fELF") {
        return Err(format!("安装包中的 {name} 无效，请重新安装"));
    }
    Ok(bytes)
}

pub struct Connection {
    pub profile: SshProfile,
    pub info: Mutex<ConnectionInfo>,
    pub directory: PathBuf,
    pub bridge: PathBuf,
    pub config: PathBuf,
    pub control: PathBuf,
    pub ssh_args: Vec<String>,
    pub bytes: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    master: Mutex<Child>,
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(c) = self.master.get_mut() {
            if c.try_wait().ok().flatten().is_none() {
                kill(c);
            }
            let _ = c.wait();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}
pub const DISCONNECTED: &str = "SSH 主连接已断开，请重新连接服务器并重新对比后再同步";

/// Observe a stop without consuming the child's exit status used by Child.
fn stopped_signal(child: &Child) -> Option<i32> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    unsafe {
        if libc::waitid(
            libc::P_PID,
            child.id(),
            info.as_mut_ptr(),
            libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT,
        ) != 0
        {
            return None;
        }
        let info = info.assume_init();
        (info.si_pid() != 0 && info.si_code == libc::CLD_STOPPED).then(|| info.si_status())
    }
}
pub fn kill(c: &mut Child) {
    unsafe {
        libc::kill(-(c.id() as i32), libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(100));
    unsafe {
        libc::kill(-(c.id() as i32), libc::SIGKILL);
    }
    let _ = c.kill();
}
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub success: bool,
}
// All tasks drain both pipes while polling cancellation; bounded stdout prevents runaway peers.
pub fn process(
    cmd: Command,
    input: Vec<u8>,
    cancel: &AtomicBool,
    tick: &mut dyn FnMut(),
) -> Result<Output, String> {
    process_live(cmd, input, cancel, tick, &mut |_| {})
}
pub fn process_live(
    mut cmd: Command,
    input: Vec<u8>,
    cancel: &AtomicBool,
    tick: &mut dyn FnMut(),
    output: &mut dyn FnMut(&[u8]),
) -> Result<Output, String> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().unwrap();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let (output_tx, output_rx) = mpsc::channel();
    let overflow = Arc::new(AtomicBool::new(false));
    let output_overflow = overflow.clone();
    let reader = std::thread::spawn(move || {
        let mut out = out;
        let mut all = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            let n = out.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buffer[..n]);
            let _ = output_tx.send(buffer[..n].to_vec());
            if all.len() > 256 * 1024 * 1024 {
                output_overflow.store(true, Ordering::Relaxed);
                return Err(std::io::Error::other("output limit"));
            }
        }
        Ok::<_, std::io::Error>(all)
    });
    let errors = std::thread::spawn(move || {
        let mut tail = Vec::new();
        let mut b = [0u8; 8192];
        let mut r = err;
        while let Ok(n) = r.read(&mut b) {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&b[..n]);
            if tail.len() > 65536 {
                tail.drain(..tail.len() - 65536);
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });
    let mut cancelled = false;
    let mut failure = None;
    let status = loop {
        for chunk in output_rx.try_iter() {
            output(&chunk);
        }
        if let Some(signal) = stopped_signal(&child) {
            failure = Some(format!(
                "同步子进程被系统暂停（信号 {signal}），任务已停止，请重新连接并重新对比"
            ));
        } else if overflow.load(Ordering::Relaxed) {
            failure = Some("远端输出超过限制".into());
        }
        if failure.is_some() || cancel.load(Ordering::Relaxed) {
            cancelled = true;
            kill(&mut child);
            break child.wait().map_err(|e| e.to_string())?;
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Err(e) => {
                kill(&mut child);
                return Err(e.to_string());
            }
            _ => {}
        }
        tick();
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = writer.join();
    let stdout = reader
        .join()
        .map_err(|_| "输出读取线程异常")?
        .map_err(|e| e.to_string())?;
    for chunk in output_rx.try_iter() {
        output(&chunk);
    }
    let stderr = errors.join().unwrap_or_default();
    if let Some(error) = failure {
        return Err(error);
    }
    if cancelled {
        return Err(crate::engine::CANCELLED.into());
    }
    if stdout.len() > 256 * 1024 * 1024 {
        return Err("远端输出超过限制".into());
    }
    Ok(Output {
        code: status.code(),
        stdout,
        stderr,
        success: status.success(),
    })
}
/// System SSH/rsync must not load GTK/AppImage's bundled shared libraries.
pub fn system_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    if std::env::var_os("APPIMAGE").is_some() || std::env::var_os("APPDIR").is_some() {
        cmd.env_remove("LD_LIBRARY_PATH").env_remove("LD_PRELOAD");
    }
    cmd
}
impl Connection {
    pub fn ensure_alive(&self) -> Result<(), String> {
        let mut master = self.master.lock().map_err(|_| DISCONNECTED)?;
        if self.stop.load(Ordering::Relaxed)
            || master.try_wait().map_err(|e| e.to_string())?.is_some()
            || !self.control.exists()
            || stopped_signal(&master).is_some()
        {
            self.stop.store(true, Ordering::Relaxed);
            return Err(DISCONNECTED.into());
        }
        Ok(())
    }
    pub fn command(&self, remote: &str) -> Command {
        let mut c = sync_core::bridge::multiplex_command(
            &self.profile.host,
            &self.ssh_args,
            &self.bridge,
            &self.config,
        );
        c.arg(remote);
        c
    }
    pub fn exec(
        &self,
        remote: &str,
        input: Vec<u8>,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>, String> {
        self.ensure_alive()?;
        let out = process(self.command(remote), input, cancel, &mut || {})?;
        if !out.success {
            self.ensure_alive()?;
            Err(format!("SSH 命令失败: {}", out.stderr.trim()))
        } else {
            Ok(out.stdout)
        }
    }
    pub fn agent_path(&self) -> String {
        format!(
            "{}/.local/share/syncui/agent/current/syncui-agent",
            self.info.lock().unwrap().home
        )
    }
    pub fn request(
        &self,
        root: &str,
        action: Operation,
        cancel: &AtomicBool,
        entries: &mut dyn FnMut(Vec<(String, Stamp)>),
    ) -> Result<Value, String> {
        self.ensure_alive()?;
        let id = random_id();
        let request = Request {
            version: protocol::VERSION,
            id: id.clone(),
            root: root.into(),
            action,
        };
        let mut cmd = self.command(&format!("{} serve", quote(&self.agent_path())));
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
        let (writer_done, writer_wait) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            let r = stdin.write_all(&bytes).and_then(|_| stdin.write_all(b"\n"));
            let _ = writer_wait.recv();
            drop(stdin);
            r
        });
        let (tx, rx) = mpsc::sync_channel(8);
        let reader = std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match r
                    .by_ref()
                    .take(protocol::MAX_FRAME + 1)
                    .read_line(&mut line)
                {
                    Ok(0) => break,
                    Ok(_) if line.len() as u64 <= protocol::MAX_FRAME => {
                        if tx
                            .send(
                                serde_json::from_str::<Frame>(&line)
                                    .map_err(|e| format!("Agent 协议错误: {e}")),
                            )
                            .is_err()
                        {
                            break;
                        }
                    }
                    _ => {
                        let _ = tx.send(Err("Agent 帧超限或读取失败".into()));
                        break;
                    }
                }
            }
        });
        let error_reader = std::thread::spawn(move || {
            let mut all = Vec::new();
            let mut buf = [0; 4096];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                all.extend_from_slice(&buf[..n]);
                if all.len() > 32768 {
                    all.drain(..all.len() - 32768);
                }
            }
            String::from_utf8_lossy(&all).into_owned()
        });
        let mut result = Err("Agent 未返回完整结束标记".into());
        loop {
            if cancel.load(Ordering::Relaxed) {
                result = Err(crate::engine::CANCELLED.into());
                break;
            }
            if let Err(error) = self.ensure_alive() {
                result = Err(error);
                break;
            }
            if let Some(signal) = stopped_signal(&child) {
                result = Err(format!(
                    "SSH 子进程被系统暂停（信号 {signal}），请重新连接并重新对比"
                ));
                break;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(f)) if f.id == id && f.version == protocol::VERSION => match f.event.as_str()
                {
                    "entries" => match serde_json::from_value(f.data) {
                        Ok(batch) => entries(batch),
                        Err(e) => {
                            result = Err(format!("无效文件清单: {e}"));
                            break;
                        }
                    },
                    "done" => {
                        result = Ok(f.data);
                        break;
                    }
                    "error" => {
                        result = Err(f.data["message"]
                            .as_str()
                            .unwrap_or("Agent 操作失败")
                            .into());
                        break;
                    }
                    _ => {
                        result = Err("未知 Agent 事件".into());
                        break;
                    }
                },
                Ok(Ok(_)) => {
                    result = Err("Agent 响应 ID 或版本不匹配".into());
                    break;
                }
                Ok(Err(e)) => {
                    result = Err(e);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }
        drop(rx);
        let _ = writer_done.send(());
        if result.is_err() {
            kill(&mut child);
        }
        let until = Instant::now() + Duration::from_secs(2);
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Err(_) => break None,
                _ => {
                    if Instant::now() > until {
                        kill(&mut child);
                        break child.wait().ok();
                    }
                    std::thread::sleep(Duration::from_millis(20))
                }
            }
        };
        let _ = writer.join();
        let _ = reader.join();
        let errors = error_reader.join().unwrap_or_default();
        if result.is_err() && !errors.trim().is_empty() {
            return Err(format!("{}: {}", result.unwrap_err(), errors.trim()));
        }
        if result.is_ok() && !status.map(|s| s.success()).unwrap_or(false) {
            return Err(format!("Agent 异常退出: {}", errors.trim()));
        }
        result
    }
}
pub fn connect(
    app: &AppHandle,
    state: &SshState,
    profile: SshProfile,
    password: Option<String>,
) -> Result<Arc<Connection>, String> {
    validate(&profile)?;
    let handle = app.clone();
    let dir = app
        .path()
        .app_config_dir()
        .ok()
        .map(|p| p.join("diagnostics"));
    let log = crate::diagnostics::Diagnostics::new_for_version(
        "ssh-connect",
        dir.as_deref(),
        env!("CARGO_PKG_VERSION"),
        move |s| {
            let _ = handle.emit("diagnostic-log", s);
        },
    );
    log.log(format!("SSH 客户端版本={}", env!("CARGO_PKG_VERSION")));
    let started = Instant::now();
    let old = state.connections.lock().unwrap().get(&profile.id).cloned();
    if let Some(old) = old {
        if old.profile == profile && old.ensure_alive().is_ok() {
            log.log("复用已有 SSH ControlMaster 连接");
            return Ok(old);
        }
    }
    state.connections.lock().unwrap().remove(&profile.id);
    let bundled_bridge = resource_bytes(app, "syncui-bridge")?;
    let directory = std::env::temp_dir().join(format!("syncui-{}", random_id()));
    fs::create_dir(&directory).map_err(|e| e.to_string())?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    // Execute askpass from our private runtime, independently of the AppImage mount.
    let bridge = directory.join("syncui-bridge");
    if let Err(e) = fs::write(&bridge, &bundled_bridge)
        .and_then(|_| fs::set_permissions(&bridge, fs::Permissions::from_mode(0o700)))
    {
        let _ = fs::remove_dir_all(&directory);
        return Err(format!("无法准备 SSH 身份验证辅助程序: {e}"));
    }
    log.log(format!("身份验证辅助程序已准备：{}", bridge.display()));
    let control = directory.join("master");
    let socket = directory.join("bridge.sock");
    let config = directory.join("bridge.json");
    let token = random_id();
    let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let known = app
        .path()
        .app_config_dir()
        .map_err(|e| e.to_string())?
        .join("ssh_known_hosts");
    fs::create_dir_all(known.parent().unwrap()).map_err(|e| e.to_string())?;
    let mut args = vec![
        "-T".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-o".into(),
        "BatchMode=no".into(),
        "-o".into(),
        "StrictHostKeyChecking=ask".into(),
        "-o".into(),
        format!("UserKnownHostsFile={}", known.display()),
        "-o".into(),
        format!("ControlPath={}", control.display()),
    ];
    if !profile.user.is_empty() {
        args.extend(["-l".into(), profile.user.clone()])
    }
    if let Some(port) = profile.port {
        args.extend(["-p".into(), port.to_string()])
    }
    if !profile.identity_file.is_empty() {
        args.extend(["-i".into(), profile.identity_file.clone()])
    }
    if profile.auth == "password" {
        args.extend([
            "-o".into(),
            "PreferredAuthentications=password,keyboard-interactive".into(),
        ])
    } else if profile.auth == "key" {
        args.extend(["-o".into(), "PreferredAuthentications=publickey".into()])
    }
    let conf = BridgeConfig {
        socket: socket.to_string_lossy().into(),
        token: token.clone(),
        ssh_args: args.clone(),
        host: profile.host.clone(),
    };
    fs::write(&config, serde_json::to_vec(&conf).unwrap()).map_err(|e| e.to_string())?;
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let bytes = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let mut warning = String::new();
    let password = if password.as_ref().is_some_and(|s| !s.is_empty()) {
        password
    } else if profile.remember_password {
        match crate::credentials::access(&credential_key(&profile), None, false) {
            Ok(p) => p,
            Err(e) => {
                warning = e;
                None
            }
        }
    } else {
        None
    };
    let secret = Arc::new(Mutex::new(password));
    let last_password = Arc::new(Mutex::new(None::<String>));
    let pending = state.pending.clone();
    let app_auth = app.clone();
    let stop_auth = stop.clone();
    let count = bytes.clone();
    let secret_auth = secret.clone();
    let last_auth = last_password.clone();
    let connection_id = profile.id.clone();
    let auth_log = log.clone();
    let auth_requests = Arc::new(AtomicU64::new(0));
    let auth_count = auth_requests.clone();
    std::thread::spawn(move || {
        while !stop_auth.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut line = String::new();
                    if BufReader::new(&mut stream)
                        .take(65536)
                        .read_line(&mut line)
                        .is_err()
                    {
                        continue;
                    }
                    match serde_json::from_str::<Message>(&line) {
                        Ok(Message::Bytes { token: t, count: n }) if t == token => {
                            count.fetch_add(n, Ordering::Relaxed);
                            let _ = stream.write_all(b"ok\n");
                        }
                        Ok(Message::Auth { token: t, prompt }) if t == token => {
                            auth_count.fetch_add(1, Ordering::Relaxed);
                            let is_host_key = prompt.contains("yes/no");
                            auth_log.log(if is_host_key {
                                "收到主机指纹确认请求"
                            } else {
                                "收到 SSH 身份验证请求"
                            });
                            let is_password = prompt.to_lowercase().contains("password:");
                            let supplied = if is_password {
                                secret_auth.lock().unwrap().take()
                            } else {
                                None
                            };
                            let answer = if supplied.is_some() {
                                supplied
                            } else {
                                let id = random_id();
                                let (tx, rx) = mpsc::channel();
                                pending.lock().unwrap().insert(id.clone(), tx);
                                if let Err(e) = app_auth.emit("ssh-auth-request",json!({"id":id,"connectionId":connection_id,"prompt":prompt,"kind":if is_host_key {"hostKey"}else{"secret"}})) {
                                    auth_log.log(format!("无法发送身份验证弹窗事件: {e}"));
                                    pending.lock().unwrap().remove(&id);
                                } else {
                                    auth_log.log("身份验证弹窗事件已发送，等待用户确认");
                                }
                                let deadline = Instant::now() + Duration::from_secs(180);
                                let reply = loop {
                                    if stop_auth.load(Ordering::Relaxed)
                                        || Instant::now() > deadline
                                    {
                                        break None;
                                    }
                                    match rx.recv_timeout(Duration::from_millis(100)) {
                                        Ok(a) => break a,
                                        Err(mpsc::RecvTimeoutError::Disconnected) => break None,
                                        _ => {}
                                    }
                                };
                                pending.lock().unwrap().remove(&id);
                                auth_log.log(if reply.is_some() {
                                    "身份验证弹窗已回复"
                                } else {
                                    "身份验证弹窗取消或超时"
                                });
                                reply
                            };
                            if is_password {
                                *last_auth.lock().unwrap() = answer.clone();
                            }
                            let _ = serde_json::to_writer(&mut stream, &AuthReply { answer });
                            let _ = stream.write_all(b"\n");
                        }
                        _ => {}
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(_) => break,
            }
        }
    });
    let logpath = directory.join("master.log");
    let master_log = fs::File::create(&logpath).map_err(|e| e.to_string())?;
    let mut command = system_command("ssh");
    command
        .args(["-v", "-M", "-N"])
        .args(&args)
        .arg("-o")
        .arg("ControlMaster=yes")
        .arg(&profile.host)
        .env("SSH_ASKPASS", &bridge)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("SYNCUI_BRIDGE_CONFIG", &config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(master_log)
        .process_group(0);
    let master = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            stop.store(true, Ordering::Relaxed);
            let _ = fs::remove_dir_all(directory);
            return Err(e.to_string());
        }
    };
    let conn = Arc::new(Connection {
        profile: profile.clone(),
        info: Mutex::new(ConnectionInfo {
            id: profile.id.clone(),
            user: String::new(),
            home: String::new(),
            arch: String::new(),
            host_key: String::new(),
            rsync_ready: false,
            agent_ready: false,
            message: warning,
        }),
        directory,
        bridge,
        config,
        control,
        ssh_args: args,
        bytes,
        stop,
        master: Mutex::new(master),
    });
    let deadline = Instant::now() + Duration::from_secs(190);
    loop {
        if conn
            .master
            .lock()
            .unwrap()
            .try_wait()
            .map_err(|e| e.to_string())?
            .is_some()
        {
            let raw = fs::read_to_string(&logpath).unwrap_or_default();
            log.detail(&raw);
            log.log(format!(
                "SSH 连接失败；收到的身份验证请求数={}",
                auth_requests.load(Ordering::Relaxed)
            ));
            return Err(format!(
                "SSH 连接失败: {}",
                raw.lines()
                    .filter(|s| !s.starts_with("debug") && !s.starts_with("OpenSSH"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if conn.control.exists() {
            break;
        }
        if Instant::now() > deadline {
            return Err("SSH 认证超时".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    log.log(format!(
        "SSH 认证完成：elapsed={:.2}ms",
        started.elapsed().as_secs_f64() * 1000.
    ));
    let probe = conn.exec(
        "uname -s; uname -m; id -un; printf '%s\\n' \"$HOME\"; rsync --version 2>/dev/null || true",
        vec![],
        &AtomicBool::new(false),
    )?;
    let probe = String::from_utf8(probe).map_err(|_| "远端环境输出不是 UTF-8")?;
    let lines: Vec<_> = probe.lines().collect();
    if lines.len() < 4 || lines[0] != "Linux" {
        return Err("首版只支持 Linux 远端".into());
    }
    let host_key = fs::read_to_string(&logpath)
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.split_once("Server host key: ").map(|(_, s)| s.to_owned()))
        .ok_or("未能确认服务器主机密钥")?;
    {
        let mut info = conn.info.lock().unwrap();
        info.user = lines[2].into();
        info.home = lines[3].into();
        info.arch = lines[1].into();
        info.host_key = host_key;
        info.rsync_ready = rsync_version_ok(&probe);
    }
    let hello = conn.exec(
        &format!("{} --version", quote(&conn.agent_path())),
        vec![],
        &AtomicBool::new(false),
    );
    conn.info.lock().unwrap().agent_ready = hello
        .map(|s| String::from_utf8_lossy(&s).contains(&format!("protocol={}", protocol::VERSION)))
        .unwrap_or(false);
    if profile.remember_password {
        if let Some(p) = last_password.lock().unwrap().as_ref() {
            if let Err(e) = crate::credentials::access(&credential_key(&profile), Some(p), false) {
                conn.info.lock().unwrap().message = e
            }
        }
    }
    *last_password.lock().unwrap() = None;
    *secret.lock().unwrap() = None;
    if !conn.info.lock().unwrap().rsync_ready {
        conn.info.lock().unwrap().message.push_str(" 远端缺少 rsync >= 3.2.0；Ubuntu/Debian 请运行 sudo apt install rsync，Fedora 请运行 sudo dnf install rsync。");
    }
    log.log(format!(
        "远端环境/Agent 检测完成：elapsed={:.2}ms",
        started.elapsed().as_secs_f64() * 1000.
    ));
    state
        .connections
        .lock()
        .unwrap()
        .insert(profile.id.clone(), conn.clone());
    // Passive monitoring only: do not send requests or reconnect in the background.
    let connection = Arc::downgrade(&conn);
    let monitor_app = app.clone();
    let monitor_log = log.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(500));
        let Some(c) = connection.upgrade() else {
            break;
        };
        if let Err(reason) = c.ensure_alive() {
            let state = monitor_app.state::<SshState>();
            let removed = {
                let mut connections = state.connections.lock().unwrap();
                if connections
                    .get(&c.profile.id)
                    .is_some_and(|current| Arc::ptr_eq(current, &c))
                {
                    connections.remove(&c.profile.id);
                    true
                } else {
                    false
                }
            };
            if removed {
                monitor_app
                    .state::<crate::coordinator::Coordinator>()
                    .sessions
                    .lock()
                    .unwrap()
                    .clear();
                monitor_log.log(&reason);
                let _ = monitor_app.emit(
                    "ssh-connection-state",
                    json!({
                        "connectionId": c.profile.id, "connected": false, "reason": reason,
                    }),
                );
            }
            break;
        }
    });
    Ok(conn)
}
pub fn rsync_version_ok(text: &str) -> bool {
    text.lines()
        .find(|l| l.starts_with("rsync  version "))
        .and_then(|l| l.split_whitespace().nth(2))
        .map(|v| {
            let mut x = v.split('.').filter_map(|p| p.parse::<u32>().ok());
            (x.next().unwrap_or(0), x.next().unwrap_or(0)) >= (3, 2)
        })
        .unwrap_or(false)
}
pub fn deploy(app: &AppHandle, c: &Connection) -> Result<ConnectionInfo, String> {
    use sha2::{Digest, Sha256};
    let arch = c.info.lock().unwrap().arch.clone();
    let target = match arch.as_str() {
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => return Err(format!("不支持的远端架构: {arch}")),
    };
    let bytes = resource_bytes(app, &format!("agents/{target}/syncui-agent"))?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let home = c.info.lock().unwrap().home.clone();
    let base = format!("{home}/.local/share/syncui/agent");
    let version = format!("0.2.0-{}", &hash[..12]);
    let dir = format!("{base}/{version}");
    let tmp = format!("{dir}/.upload-{}", random_id());
    let never = AtomicBool::new(false);
    let upload = format!(
        "umask 077; mkdir -p {} && cat > {} && chmod 700 {} && sha256sum -- {}",
        quote(&dir),
        quote(&tmp),
        quote(&tmp),
        quote(&tmp)
    );
    let digest = c.exec(&upload, bytes, &never)?;
    if !String::from_utf8_lossy(&digest).starts_with(&hash) {
        return Err("Agent 上传校验失败，未切换版本".into());
    }
    let version_out = c.exec(&format!("{} --version", quote(&tmp)), vec![], &never)?;
    if !String::from_utf8_lossy(&version_out).contains(&format!("protocol={}", protocol::VERSION)) {
        return Err("Agent 自检失败，未切换版本".into());
    }
    let link = format!("{base}/.current-{}", random_id());
    c.exec(
        &format!(
            "mv -f -- {} {} && ln -s -- {} {} && mv -Tf -- {} {}",
            quote(&tmp),
            quote(&format!("{dir}/syncui-agent")),
            quote(&version),
            quote(&link),
            quote(&link),
            quote(&format!("{base}/current"))
        ),
        vec![],
        &never,
    )?;
    c.info.lock().unwrap().agent_ready = true;
    Ok(c.info.lock().unwrap().clone())
}
#[derive(Clone)]
pub struct SshEndpoint {
    pub connection: Arc<Connection>,
    pub root: String,
}
impl Endpoint for SshEndpoint {
    fn scan(
        &self,
        ignore: &[String],
        cancel: &AtomicBool,
        progress: &(dyn Fn(usize) + Sync),
    ) -> Result<Manifest, String> {
        let mut files = Inventory::new();
        let mut duplicate = false;
        let data = self.connection.request(
            &self.root,
            Operation::Scan {
                ignore: ignore.to_vec(),
            },
            cancel,
            &mut |b| {
                for (p, s) in b {
                    if endpoint::validate_rel(&p).is_err() || files.insert(p, s).is_some() {
                        duplicate = true
                    }
                }
                progress(files.len())
            },
        )?;
        if duplicate {
            return Err("Agent 文件清单包含重复或无效路径".into());
        }
        let mut m: Manifest = serde_json::from_value(data).map_err(|e| e.to_string())?;
        m.files = files;
        Ok(m)
    }
    fn stats(
        &self,
        paths: &[String],
        c: &AtomicBool,
    ) -> Result<BTreeMap<String, Option<Stamp>>, String> {
        serde_json::from_value(self.connection.request(
            &self.root,
            Operation::Stats {
                paths: paths.to_vec(),
            },
            c,
            &mut |_| {},
        )?)
        .map_err(|e| e.to_string())
    }
    fn hashes(
        &self,
        files: &Inventory,
        c: &AtomicBool,
    ) -> Result<BTreeMap<String, String>, String> {
        serde_json::from_value(self.connection.request(
            &self.root,
            Operation::Hashes {
                files: files.clone(),
            },
            c,
            &mut |_| {},
        )?)
        .map_err(|e| e.to_string())
    }
    fn read(&self, p: &str) -> Result<TextFile, String> {
        serde_json::from_value(self.connection.request(
            &self.root,
            Operation::Read { path: p.into() },
            &AtomicBool::new(false),
            &mut |_| {},
        )?)
        .map_err(|e| e.to_string())
    }
    fn write(&self, p: &str, t: &str, e: &Option<Stamp>) -> Result<Stamp, String> {
        serde_json::from_value(self.connection.request(
            &self.root,
            Operation::Write {
                path: p.into(),
                content: t.into(),
                expected: e.clone(),
            },
            &AtomicBool::new(false),
            &mut |_| {},
        )?)
        .map_err(|e| e.to_string())
    }
    fn delete(&self, p: &str, e: &Stamp) -> Result<(), String> {
        self.delete_cancelled(p, e, &AtomicBool::new(false))
    }
    fn delete_cancelled(&self, p: &str, e: &Stamp, cancel: &AtomicBool) -> Result<(), String> {
        self.connection
            .request(
                &self.root,
                Operation::Delete {
                    path: p.into(),
                    expected: e.clone(),
                },
                cancel,
                &mut |_| {},
            )
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_and_shell_arguments() {
        assert!(rsync_version_ok(
            "rsync  version 3.2.7  protocol version 31"
        ));
        assert!(!rsync_version_ok(
            "rsync  version 3.1.3  protocol version 31"
        ));
        assert_eq!(quote("a'b\n$HOME"), "'a'\\''b\n$HOME'");
    }
    #[test]
    fn credentials_are_bound_to_server_identity_settings() {
        let p = SshProfile {
            id: "id".into(),
            name: "name".into(),
            host: "host1".into(),
            user: "u".into(),
            port: None,
            identity_file: "".into(),
            auth: "auto".into(),
            remember_password: true,
        };
        let mut q = p.clone();
        q.host = "host2".into();
        assert_ne!(credential_key(&p), credential_key(&q));
    }
}
