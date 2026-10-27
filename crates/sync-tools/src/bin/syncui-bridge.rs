//! SSH askpass and rsync remote-shell bridge. Secrets travel over a private Unix socket.
use std::{
    io::{self, BufRead, Read, Write},
    os::unix::net::UnixStream,
    process::{Command, Stdio},
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
    let config: BridgeConfig =
        serde_json::from_slice(&std::fs::read(std::env::var("SYNCUI_BRIDGE_CONFIG")?)?)?;
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
    let mut child = Command::new("ssh")
        .args(&config.ssh_args)
        .arg(&config.host)
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
        let mut b = [0; 65536];
        loop {
            match src.read(&mut b) {
                Ok(0) | Err(_) => break,
                Ok(k) => {
                    if input.write_all(&b[..k]).is_err() {
                        break;
                    }
                    n.fetch_add(k as u64, Ordering::Relaxed);
                }
            }
        }
    });
    let mut output = child.stdout.take().unwrap();
    let mut dst = io::stdout();
    let mut buf = [0; 65536];
    loop {
        let k = output.read(&mut buf)?;
        if k == 0 {
            break;
        }
        dst.write_all(&buf[..k])?;
        dst.flush()?;
        count.fetch_add(k as u64, Ordering::Relaxed);
    }
    let status = child.wait()?;
    stop.store(true, Ordering::Relaxed);
    let _ = report.join();
    if !status.success() {
        return Err(format!("ssh exited: {status}").into());
    }
    Ok(())
}
