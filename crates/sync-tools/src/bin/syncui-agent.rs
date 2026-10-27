use std::{
    io::{self, BufRead, Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use sync_core::protocol::{self, Frame, Request};
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!(
            "syncui-agent {} protocol={} arch={}",
            env!("CARGO_PKG_VERSION"),
            protocol::VERSION,
            std::env::consts::ARCH
        );
        return;
    }
    if std::env::args().nth(1).as_deref() != Some("serve") {
        eprintln!("usage: syncui-agent serve | --version");
        std::process::exit(2)
    }
    let mut input = io::BufReader::new(io::stdin());
    let mut line = String::new();
    if input
        .by_ref()
        .take(protocol::MAX_FRAME + 1)
        .read_line(&mut line)
        .is_err()
        || line.len() as u64 > protocol::MAX_FRAME
    {
        eprintln!("invalid request frame");
        std::process::exit(2)
    }
    let request: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("invalid request: {e}");
            std::process::exit(2)
        }
    };
    let id = request.id.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let c = cancel.clone();
    // Client keeps stdin open until the terminal frame. EOF/cancel stops long scans/hashes.
    std::thread::spawn(move || {
        let mut b = [0u8; 1];
        let _ = input.read(&mut b);
        c.store(true, Ordering::Relaxed);
    });
    let mut output = io::BufWriter::new(io::stdout());
    let mut emit = |f: Frame| {
        if serde_json::to_writer(&mut output, &f).is_err()
            || writeln!(&mut output).is_err()
            || output.flush().is_err()
        {
            cancel.store(true, Ordering::Relaxed);
        }
    };
    if let Err(e) = protocol::run(request, &cancel, &mut emit) {
        emit(Frame {
            version: protocol::VERSION,
            id,
            event: "error".into(),
            data: serde_json::json!({"message":e}),
        });
        std::process::exit(1)
    }
}
