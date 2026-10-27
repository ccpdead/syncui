//! Bounded UI diagnostics plus a complete, buffered per-run local log.
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static NEXT_RUN: AtomicU64 = AtomicU64::new(0);
type Emit = dyn Fn(&str) + Send + Sync;

#[derive(Clone)]
pub struct Diagnostics(Arc<Inner>);

struct Inner {
    id: String,
    started: Instant,
    file: Mutex<Option<BufWriter<File>>>,
    emit: Option<Box<Emit>>,
}

impl Diagnostics {
    pub fn silent() -> Self {
        Self(Arc::new(Inner {
            id: String::new(),
            started: Instant::now(),
            file: Mutex::new(None),
            emit: None,
        }))
    }

    pub fn new(
        kind: &str,
        directory: Option<&Path>,
        emit: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self::new_for_version(kind, directory, env!("CARGO_PKG_VERSION"), emit)
    }

    pub fn new_for_version(
        kind: &str,
        directory: Option<&Path>,
        version: &str,
        emit: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let id = format!(
            "{kind}-{timestamp}-{}-{}",
            std::process::id(),
            NEXT_RUN.fetch_add(1, Ordering::Relaxed)
        );
        let path = directory.map(|dir| dir.join(format!("{id}.log")));
        let file = path.as_ref().map(|path| {
            fs::create_dir_all(path.parent().unwrap())?;
            File::create(path).map(BufWriter::new)
        });
        let error = file
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .map(ToString::to_string);
        let this = Self(Arc::new(Inner {
            id,
            started: Instant::now(),
            file: Mutex::new(file.and_then(Result::ok)),
            emit: Some(Box::new(emit)),
        }));
        this.log(format!(
            "任务开始：版本={}，系统={}/{}，构建={}",
            version,
            std::env::consts::OS,
            std::env::consts::ARCH,
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        ));
        match (path, error) {
            (_, Some(error)) => this.log(format!("诊断文件创建失败：{error}；继续输出到界面")),
            (Some(path), None) => this.log(format!("完整诊断文件：{}", path.display())),
            _ => this.log("无法定位诊断目录，仅输出到界面"),
        }
        this
    }

    pub fn id(&self) -> &str {
        &self.0.id
    }

    pub fn enabled(&self) -> bool {
        self.0.emit.is_some()
    }

    pub fn log(&self, message: impl AsRef<str>) {
        self.write(message.as_ref(), true);
    }

    /// File-only details avoid flooding the UI during large syncs.
    pub fn detail(&self, message: impl AsRef<str>) {
        self.write(message.as_ref(), false);
    }

    fn write(&self, message: &str, visible: bool) {
        if !self.enabled() {
            return;
        }
        let line = format!(
            "[诊断 {} +{:.1}ms] {message}",
            self.0.id,
            self.0.started.elapsed().as_secs_f64() * 1000.0
        );
        let mut failure = None;
        {
            let mut guard = self.0.file.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(writer) = guard.as_mut() {
                let result = writeln!(writer, "{line}").and_then(|_| {
                    if visible {
                        writer.flush()
                    } else {
                        Ok(())
                    }
                });
                if let Err(error) = result {
                    failure = Some(error.to_string());
                    *guard = None;
                }
            }
        }
        if let Some(error) = failure {
            self.report_failure(&error);
        }
        if visible {
            eprintln!("{line}");
            if let Some(emit) = &self.0.emit {
                emit(&line);
            }
        }
    }

    pub fn flush(&self) {
        let failure = {
            let mut guard = self.0.file.lock().unwrap_or_else(|e| e.into_inner());
            let error = guard.as_mut().and_then(|writer| writer.flush().err());
            if error.is_some() {
                *guard = None;
            }
            error
        };
        if let Some(error) = failure {
            self.report_failure(&error.to_string());
        }
    }

    fn report_failure(&self, error: &str) {
        let message = format!(
            "[诊断 {}] 诊断文件写入失败：{error}；完整文件不可用，界面摘要继续输出",
            self.0.id
        );
        eprintln!("{message}");
        if let Some(emit) = &self.0.emit {
            emit(&message);
        }
    }
}

/// Inspect mount metadata only; never probe or traverse the remote directory.
pub fn mount_description(root: &Path) -> String {
    #[cfg(target_os = "linux")]
    if let Ok(mounts) = fs::read_to_string("/proc/self/mountinfo") {
        let mut best: Option<(usize, String, String)> = None;
        for line in mounts.lines() {
            let Some((left, right)) = line.split_once(" - ") else {
                continue;
            };
            let Some(mount) = left.split_whitespace().nth(4) else {
                continue;
            };
            let mount = mount
                .replace("\\040", " ")
                .replace("\\011", "\t")
                .replace("\\012", "\n")
                .replace("\\134", "\\");
            let fs_type = right.split_whitespace().next().unwrap_or("unknown");
            if root.starts_with(Path::new(&mount))
                && best.as_ref().map_or(true, |b| mount.len() > b.0)
            {
                best = Some((mount.len(), mount, fs_type.to_string()));
            }
        }
        if let Some((_, mount, fs_type)) = best {
            return format!("type={fs_type}，mount={mount:?}（按路径匹配）");
        }
    }
    "挂载类型未采集".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_keeps_details_while_ui_only_receives_summary() {
        let dir = std::env::temp_dir().join(format!(
            "syncui_diagnostics_test_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let captured = lines.clone();
        let diagnostics = Diagnostics::new("test", Some(&dir), move |line| {
            captured.lock().unwrap().push(line.to_string())
        });
        diagnostics.detail("file-only operation details");
        diagnostics.log("visible phase summary");
        diagnostics.flush();
        let path = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("file-only operation details"));
        assert!(content.contains("visible phase summary"));
        let lines = lines.lock().unwrap();
        assert!(!lines
            .iter()
            .any(|l| l.contains("file-only operation details")));
        assert!(lines.iter().any(|l| l.contains("visible phase summary")));
        drop(diagnostics);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unavailable_log_directory_still_reports_to_ui() {
        let file = std::env::temp_dir().join(format!(
            "syncui_diagnostics_blocked_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&file, "not a directory").unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let captured = lines.clone();
        let diagnostics = Diagnostics::new("test", Some(&file), move |line| {
            captured.lock().unwrap().push(line.to_string())
        });
        diagnostics.log("sync can continue");
        let text = lines.lock().unwrap().join("\n");
        assert!(text.contains("诊断文件创建失败"));
        assert!(text.contains("sync can continue"));
        drop(diagnostics);
        fs::remove_file(file).unwrap();
    }
}
