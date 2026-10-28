//! SSH askpass and rsync remote-shell bridge. Secrets travel over a private Unix socket.
use std::{
    io::{self, BufRead, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use sync_core::bridge::{AuthReply, BridgeConfig, Message};
fn send(c: &BridgeConfig, msg: &Message) -> io::Result<UnixStream> {
    let mut s = UnixStream::connect(&c.socket)?;
    serde_json::to_writer(&mut s, msg)?;
    s.write_all(b"\n")?;
    Ok(s)
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn main() {
    if let Err(e) = run() {
        eprintln!("syncui bridge: {e}");
        std::process::exit(1)
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config_path = std::path::PathBuf::from(std::env::var("SYNCUI_BRIDGE_CONFIG")?);
    let config: BridgeConfig = serde_json::from_slice(&std::fs::read(&config_path)?)?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("rsync-rsh") {
        let s = send(
            &config,
            &Message::Auth {
                token: config.token.clone(),
                prompt: args.join(" "),
            },
        )?;
        s.set_read_timeout(Some(Duration::from_secs(180)))?;
        let mut line = String::new();
        io::BufReader::new(s).take(65536).read_line(&mut line)?;
        let r: AuthReply = serde_json::from_str(&line)?;
        if let Some(a) = r.answer {
            println!("{a}");
            return Ok(());
        }
        return Err("authentication cancelled".into());
    }
    if args.get(1).map(String::as_str) != Some("syncui-peer")
        || args.get(2).map(String::as_str) != Some("rsync")
    {
        return Err("invalid rsync remote command".into());
    }
    let remote = args[2..]
        .iter()
        .map(|s| quote(s))
        .collect::<Vec<_>>()
        .join(" ");
    let mut child = sync_core::bridge::multiplex_command(
        &config.host,
        &config.ssh_args,
        &std::env::current_exe()?,
        &config_path,
    )
    .arg(remote)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .spawn()?;
    let count = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let n = count.clone();
    let ended = stop.clone();
    let conf = config.clone();
    let report = std::thread::spawn(move || {
        let mut sent = 0;
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let total = n.load(Ordering::Relaxed);
            if total > sent {
                let sent_message = send(
                    &conf,
                    &Message::Bytes {
                        token: conf.token.clone(),
                        count: total - sent,
                    },
                );
                if let Ok(mut socket) = sent_message {
                    let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut ack = [0; 3];
                    let _ = socket.read_exact(&mut ack);
                }
                sent = total;
            }
            if ended.load(Ordering::Relaxed) {
                break;
            }
        }
    });
    let mut input = child.stdin.take().unwrap();
    let n = count.clone();
    std::thread::spawn(move || {
        let mut src = io::stdin();
        if let Err(e) = relay(&mut src, &mut input, &n) {
            eprintln!("syncui bridge: rsync -> ssh: {e}");
        }
    });
    let mut output = child.stdout.take().unwrap();
    let mut dst = io::stdout().lock();
    relay(&mut output, &mut dst, &count).map_err(|e| format!("ssh -> rsync: {e}"))?;
    let status = child.wait()?;
    stop.store(true, Ordering::Relaxed);
    let _ = report.join();
    if !status.success() {
        return Err(format!("ssh exited: {status}").into());
    }
    Ok(())
}

fn relay<R: Read + AsRawFd, W: Write + AsRawFd>(
    src: &mut R,
    dst: &mut W,
    count: &AtomicU64,
) -> io::Result<()> {
    let mut buf = [0; 65536];
    loop {
        let len = retry_io(src.as_raw_fd(), libc::POLLIN, || src.read(&mut buf))?;
        if len == 0 {
            return Ok(());
        }
        let mut offset = 0;
        while offset < len {
            let written = retry_io(dst.as_raw_fd(), libc::POLLOUT, || {
                dst.write(&buf[offset..len])
            })?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            offset += written;
            count.fetch_add(written as u64, Ordering::Relaxed);
        }
        retry_io(dst.as_raw_fd(), libc::POLLOUT, || dst.flush())?;
    }
}

fn retry_io<T>(
    fd: libc::c_int,
    events: libc::c_short,
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    loop {
        match operation() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // rsync can pass a nonblocking stdout. Wait instead of dropping data or spinning.
                let mut pollfd = libc::pollfd {
                    fd,
                    events,
                    revents: 0,
                };
                while unsafe { libc::poll(&mut pollfd, 1, -1) } < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonblocking_relay_preserves_bytes_under_backpressure() {
        let (mut source, mut producer) = UnixStream::pair().unwrap();
        let (mut destination, mut consumer) = UnixStream::pair().unwrap();
        source.set_nonblocking(true).unwrap();
        destination.set_nonblocking(true).unwrap();
        let expected: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let payload = expected.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            producer.write_all(&payload).unwrap();
        });
        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let mut received = Vec::new();
            consumer.read_to_end(&mut received).unwrap();
            received
        });
        let count = AtomicU64::new(0);
        relay(&mut source, &mut destination, &count).unwrap();
        drop(destination);
        writer.join().unwrap();
        assert_eq!(reader.join().unwrap(), expected);
        assert_eq!(count.load(Ordering::Relaxed), expected.len() as u64);
    }

    #[test]
    fn relay_reports_closed_destination() {
        let (mut source, mut producer) = UnixStream::pair().unwrap();
        let (mut destination, consumer) = UnixStream::pair().unwrap();
        destination.set_nonblocking(true).unwrap();
        drop(consumer);
        producer.write_all(b"data").unwrap();
        drop(producer);
        let error = relay(&mut source, &mut destination, &AtomicU64::new(0)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
