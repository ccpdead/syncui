use crate::endpoint::{self, Inventory, Stamp};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::Path, sync::atomic::AtomicBool};
pub const VERSION: u32 = 1;
pub const MAX_FRAME: u64 = 40 * 1024 * 1024;
#[derive(Serialize, Deserialize, Debug)]
pub struct Request {
    pub version: u32,
    pub id: String,
    pub root: String,
    pub action: Operation,
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Operation {
    Hello,
    Scan {
        ignore: Vec<String>,
    },
    Stats {
        paths: Vec<String>,
    },
    Hashes {
        files: Inventory,
    },
    Read {
        path: String,
    },
    Write {
        path: String,
        content: String,
        expected: Option<Stamp>,
    },
    Delete {
        path: String,
        expected: Stamp,
    },
    Browse,
}
#[derive(Serialize, Deserialize, Debug)]
pub struct Frame {
    pub version: u32,
    pub id: String,
    pub event: String,
    pub data: Value,
}
pub fn run(
    request: Request,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Frame),
) -> Result<(), String> {
    let id = request.id.clone();
    let mut send = |event: &str, data: Value| {
        emit(Frame {
            version: VERSION,
            id: id.clone(),
            event: event.into(),
            data,
        })
    };
    if request.version != VERSION {
        return Err("Agent 协议版本不兼容，请升级远端工具".into());
    }
    endpoint::check_cancel(cancel)?;
    let root = Path::new(&request.root);
    let data = match request.action {
        Operation::Hello => {
            json!({"version":env!("CARGO_PKG_VERSION"),"protocol":VERSION,"arch":std::env::consts::ARCH,"root":endpoint::canonical_root(root)?.to_str().ok_or("非 UTF-8 路径")?})
        }
        Operation::Scan { ignore } => {
            let mut m = endpoint::scan(root, &ignore, cancel, &mut |b| send("entries", json!(b)))?;
            m.files.clear();
            json!(m)
        }
        Operation::Stats { paths } => {
            let e = endpoint::LocalEndpoint(root.into());
            use endpoint::Endpoint;
            json!(e.stats(&paths, cancel)?)
        }
        Operation::Hashes { files } => {
            let e = endpoint::LocalEndpoint(root.into());
            use endpoint::Endpoint;
            json!(e.hashes(&files, cancel)?)
        }
        Operation::Read { path } => json!(endpoint::read_text(root, &path)?),
        Operation::Write {
            path,
            content,
            expected,
        } => json!(endpoint::write_text(root, &path, &content, &expected)?),
        Operation::Delete { path, expected } => {
            endpoint::delete(root, &path, &expected)?;
            Value::Null
        }
        Operation::Browse => json!(endpoint::browse(root)?),
    };
    send("done", data);
    Ok(())
}
