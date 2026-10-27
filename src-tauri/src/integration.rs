//! Opt-in integration executable. Requires an isolated localhost SSH fixture.
use crate::{
    coordinator::{self, Coordinator, RemoteEndpoint},
    engine::CompareOptions,
    ssh,
};
use std::{fs, path::PathBuf, sync::atomic::AtomicBool};
use tauri::{Listener, Manager};
pub fn run() -> Result<(), String> {
    let root = PathBuf::from(
        std::env::var("SYNCUI_SMOKE_ROOT")
            .map_err(|_| "set SYNCUI_SMOKE_ROOT to the isolated fixture")?,
    );
    assert!(root.starts_with("/tmp/"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    let mut context = tauri::generate_context!();
    context.config_mut().app.windows.clear();
    context.config_mut().identifier = "com.syncui.smoke".into();
    let app = tauri::Builder::default()
        .manage(ssh::SshState::default())
        .manage(Coordinator::default())
        .build(context)
        .map_err(|e| e.to_string())?;
    let auth_app = app.handle().clone();
    app.listen("ssh-auth-request", move |event| {
        let value: serde_json::Value = serde_json::from_str(event.payload()).unwrap();
        let id = value["id"].as_str().unwrap();
        let answer = if value["kind"] == "hostKey" {
            "yes"
        } else {
            "fixture-passphrase"
        };
        if let Some(tx) = auth_app
            .state::<ssh::SshState>()
            .pending
            .lock()
            .unwrap()
            .remove(id)
        {
            tx.send(Some(answer.into())).unwrap();
        }
    });
    let profile = ssh::SshProfile {
        id: "isolated-smoke".into(),
        name: "isolated smoke".into(),
        host: "127.0.0.1".into(),
        user: std::env::var("USER").unwrap(),
        port: Some(std::env::var("SYNCUI_SMOKE_PORT").unwrap().parse().unwrap()),
        identity_file: root.join("client_key").to_string_lossy().into(),
        auth: "key".into(),
        remember_password: false,
    };
    let c = ssh::connect(
        app.handle(),
        &app.state::<ssh::SshState>(),
        profile.clone(),
        None,
    )?;
    assert!(
        c.info
            .lock()
            .unwrap()
            .home
            .starts_with(root.to_str().unwrap()),
        "fixture must override HOME; never deploy to the real home"
    );
    let info = ssh::deploy(app.handle(), &c)?;
    assert!(info.agent_ready && info.rsync_ready);
    println!("PASS: SSH authentication, host-key confirmation, deployment, version checks");
    let l = root.join("local");
    let r = root.join("remote");
    fs::create_dir_all(&l).unwrap();
    fs::create_dir_all(&r).unwrap();
    let special = [
        "中文 ' quote.txt",
        "nested/a\nline.txt",
        "-option",
        "wild*card?.txt",
        "back\\slash.txt",
    ];
    for (i, p) in special.iter().enumerate() {
        let p = l.join(p);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, format!("special content {i}")).unwrap();
    }
    fs::write(l.join("unselected"), "do not transfer").unwrap();
    fs::write(r.join("remote-only"), "download me").unwrap();
    let remote = RemoteEndpoint::Ssh {
        profile_id: info.id.clone(),
        path: r.to_string_lossy().into(),
    };
    let options = CompareOptions {
        mode: "twoway".into(),
        use_hash: false,
        ignore: vec![],
    };
    let cancel = AtomicBool::new(false);
    let compared = coordinator::compare(
        app.handle(),
        &app.state::<Coordinator>(),
        l.to_string_lossy().into(),
        remote.clone(),
        options.clone(),
        &cancel,
    )?;
    let selected: Vec<String> = special
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once("remote-only".into()))
        .collect();
    let result = coordinator::sync(
        app.handle(),
        &app.state::<Coordinator>(),
        compared.comparison_id.as_ref().unwrap(),
        selected,
        "skip".into(),
        4,
        &cancel,
    )?;
    assert_eq!(result.failed, 0, "{:?}", result.errors);
    assert!(!result.needs_recompare, "{:?}", result.errors);
    assert!(result.bytes_copied > 0);
    assert!(result.refreshed.is_some());
    assert!(!r.join("unselected").exists());
    for p in special {
        assert_eq!(fs::read(l.join(p)).unwrap(), fs::read(r.join(p)).unwrap());
    }
    println!("PASS: bidirectional rsync, exact selection, special filenames, byte accounting, single refresh");
    let read = coordinator::read_pair(app.handle(), l.to_str().unwrap(), &remote, "remote-only")?;
    coordinator::write(
        app.handle(),
        &app.state::<Coordinator>(),
        l.to_str().unwrap(),
        &remote,
        "remote-only",
        "remote",
        "edited over SSH",
        &read.remote_version,
    )?;
    assert!(coordinator::write(
        app.handle(),
        &app.state::<Coordinator>(),
        l.to_str().unwrap(),
        &remote,
        "remote-only",
        "remote",
        "stale edit",
        &read.remote_version
    )
    .is_err());
    println!("PASS: remote text read/write and stale version protection");
    fs::remove_file(r.join("-option")).unwrap();
    let compared = coordinator::compare(
        app.handle(),
        &app.state::<Coordinator>(),
        l.to_string_lossy().into(),
        remote.clone(),
        options.clone(),
        &cancel,
    )?;
    assert_eq!(compared.delete_local_count, 1);
    let result = coordinator::sync(
        app.handle(),
        &app.state::<Coordinator>(),
        compared.comparison_id.as_ref().unwrap(),
        vec!["-option".into()],
        "skip".into(),
        4,
        &cancel,
    )?;
    assert_eq!(result.deleted_local, 1);
    assert!(!l.join("-option").exists());
    println!("PASS: three-way deletion and baseline preservation");
    let compared = coordinator::compare(
        app.handle(),
        &app.state::<Coordinator>(),
        l.to_string_lossy().into(),
        remote.clone(),
        options.clone(),
        &cancel,
    )?;
    fs::write(l.join("unselected"), "changed after compare").unwrap();
    let result = coordinator::sync(
        app.handle(),
        &app.state::<Coordinator>(),
        compared.comparison_id.as_ref().unwrap(),
        vec!["unselected".into()],
        "skip".into(),
        4,
        &cancel,
    )?;
    assert_eq!(result.skipped, 1);
    assert!(!r.join("unselected").exists());
    println!("PASS: stale comparison cannot overwrite files");

    // Full upload followed by a small edit to a large file: logical progress must
    // include reused blocks while the bridge counts only actual protocol traffic.
    let mut data = vec![0u8; 16 * 1024 * 1024];
    let mut seed = 123456789u64;
    for b in &mut data {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *b = seed as u8;
    }
    fs::write(l.join("large.bin"), &data).unwrap();
    let compared = coordinator::compare(
        app.handle(),
        &app.state::<Coordinator>(),
        l.to_string_lossy().into(),
        remote.clone(),
        options.clone(),
        &cancel,
    )?;
    let full = coordinator::sync(
        app.handle(),
        &app.state::<Coordinator>(),
        compared.comparison_id.as_ref().unwrap(),
        vec!["large.bin".into()],
        "local".into(),
        4,
        &cancel,
    )?;
    assert!(!full.needs_recompare, "{:?}", full.errors);
    data[8192..9216].fill(42);
    fs::write(l.join("large.bin"), &data).unwrap();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 10;
    filetime::set_file_mtime(
        l.join("large.bin"),
        filetime::FileTime::from_unix_time(stamp, 0),
    )
    .unwrap();
    let compared = coordinator::compare(
        app.handle(),
        &app.state::<Coordinator>(),
        l.to_string_lossy().into(),
        remote.clone(),
        options.clone(),
        &cancel,
    )?;
    let delta = coordinator::sync(
        app.handle(),
        &app.state::<Coordinator>(),
        compared.comparison_id.as_ref().unwrap(),
        vec!["large.bin".into()],
        "local".into(),
        4,
        &cancel,
    )?;
    assert!(!delta.needs_recompare, "{:?}", delta.errors);
    assert_eq!(fs::read(r.join("large.bin")).unwrap(), data);
    assert_eq!(delta.logical_bytes, data.len() as u64);
    assert!(delta.bytes_copied < delta.logical_bytes / 4);
    println!(
        "PASS: delta reuse: full={} protocol bytes, delta={} protocol bytes, logical={} bytes",
        full.bytes_copied, delta.bytes_copied, delta.logical_bytes
    );

    // Exercise agent batching over a real SSH channel, alternating local and
    // remote inventory order; this is a loopback measurement, not a WAN benchmark.
    use sync_core::endpoint::{Endpoint, LocalEndpoint};
    let bench = root.join("benchmark");
    fs::create_dir_all(&bench).unwrap();
    for i in 0..10000 {
        let dir = bench.join(format!("d{:03}", i / 100));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("f{i:05}")), b"fixture").unwrap();
    }
    let local_bench = LocalEndpoint(bench.clone());
    let remote_bench = ssh::SshEndpoint {
        connection: c.clone(),
        root: bench.to_string_lossy().into(),
    };
    for round in 0..3 {
        let start = std::time::Instant::now();
        let (a, b, first) = if round % 2 == 0 {
            let a = local_bench.scan(&[], &cancel, &|_| {})?;
            let first = start.elapsed();
            let b = remote_bench.scan(&[], &cancel, &|_| {})?;
            (a, b, first)
        } else {
            let b = remote_bench.scan(&[], &cancel, &|_| {})?;
            let first = start.elapsed();
            let a = local_bench.scan(&[], &cancel, &|_| {})?;
            (a, b, first)
        };
        assert_eq!(a.files, b.files);
        assert_eq!(a.files.len(), 10000);
        println!(
            "BENCH round={} order={} first_ms={:.2} second_ms={:.2} remote_scan_ms={:.2}",
            round + 1,
            if round % 2 == 0 {
                "local,ssh"
            } else {
                "ssh,local"
            },
            first.as_secs_f64() * 1000.,
            (start.elapsed() - first).as_secs_f64() * 1000.,
            b.elapsed_ms
        );
    }
    println!("PASS: 10000-file local/SSH manifest parity over three rounds");

    // Force a transfer to remain active, then cancel. Only this process's PATH
    // receives the test wrapper; no installed tools or user data are changed.
    use std::os::unix::fs::PermissionsExt;
    let wrapper = root.join("test-bin");
    fs::create_dir_all(&wrapper).unwrap();
    fs::write(wrapper.join("rsync"), "#!/bin/sh\nif [ \"$1\" = --version ]; then exec /usr/bin/rsync \"$@\"; fi\nexec /usr/bin/rsync --bwlimit=256 \"$@\"\n").unwrap();
    fs::set_permissions(wrapper.join("rsync"), fs::Permissions::from_mode(0o700)).unwrap();
    let previous_path = std::env::var("PATH").unwrap();
    std::env::set_var("PATH", format!("{}:{previous_path}", wrapper.display()));
    fs::write(l.join("cancel.bin"), &data).unwrap();
    let compared = coordinator::compare(
        app.handle(),
        &app.state::<Coordinator>(),
        l.to_string_lossy().into(),
        remote.clone(),
        options.clone(),
        &cancel,
    )?;
    let baseline_path = app.state::<Coordinator>().sessions.lock().unwrap()
        [compared.comparison_id.as_ref().unwrap()]
    .baseline_path
    .clone();
    let old_baseline = fs::read(&baseline_path).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(700));
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let result = coordinator::sync(
            app.handle(),
            &app.state::<Coordinator>(),
            compared.comparison_id.as_ref().unwrap(),
            vec!["cancel.bin".into()],
            "local".into(),
            4,
            &cancel,
        )
        .unwrap();
        assert!(result.cancelled && result.needs_recompare);
    });
    std::env::set_var("PATH", previous_path);
    assert!(!r.join("cancel.bin").exists());
    assert_eq!(fs::read(&baseline_path).unwrap(), old_baseline);
    println!(
        "PASS: in-flight rsync cancellation leaves no partial final file and preserves baseline"
    );
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    fs::write(wrapper.join("rsync"), "#!/bin/sh\nif [ \"$1\" = --version ]; then exec /usr/bin/rsync \"$@\"; fi\nulimit -f 1\nexec /usr/bin/rsync \"$@\"\n").unwrap();
    std::env::set_var(
        "PATH",
        format!("{}:{}", wrapper.display(), std::env::var("PATH").unwrap()),
    );
    fs::write(l.join("large.bin"), b"keep old destination").unwrap();
    let pull = CompareOptions {
        mode: "mirror_pull".into(),
        ..options.clone()
    };
    let compared = coordinator::compare(
        app.handle(),
        &app.state::<Coordinator>(),
        l.to_string_lossy().into(),
        remote.clone(),
        pull,
        &cancel,
    )?;
    let failed = coordinator::sync(
        app.handle(),
        &app.state::<Coordinator>(),
        compared.comparison_id.as_ref().unwrap(),
        vec!["large.bin".into(), "unselected".into()],
        "remote".into(),
        4,
        &cancel,
    )?;
    assert!(failed.failed > 0 && failed.needs_recompare);
    assert_eq!(
        fs::read(l.join("large.bin")).unwrap(),
        b"keep old destination"
    );
    assert!(
        l.join("unselected").exists(),
        "failed transfer must stop deletion stage"
    );
    assert_eq!(fs::read(&baseline_path).unwrap(), old_baseline);
    std::env::set_var(
        "PATH",
        std::env::var("PATH").unwrap().split_once(':').unwrap().1,
    );
    println!(
        "PASS: receiver write limit preserves destination/baseline and stops subsequent deletion"
    );
    let mut encrypted_profile = profile.clone();
    encrypted_profile.id = "encrypted-key".into();
    encrypted_profile.identity_file = root.join("encrypted_key").to_string_lossy().into();
    let encrypted = ssh::connect(
        app.handle(),
        &app.state::<ssh::SshState>(),
        encrypted_profile,
        None,
    )?;
    assert!(encrypted.info.lock().unwrap().agent_ready);
    println!("PASS: encrypted private key unlock through askpass UI event bridge");
    if let Ok(port) = std::env::var("SYNCUI_SMOKE_PASSWORD_PORT") {
        let mut password_profile = profile.clone();
        password_profile.id = "password-fixture".into();
        password_profile.port = Some(port.parse().unwrap());
        password_profile.auth = "password".into();
        password_profile.identity_file.clear();
        let connection = ssh::connect(
            app.handle(),
            &app.state::<ssh::SshState>(),
            password_profile,
            Some("incorrect-fixture-password".into()),
        )?;
        assert!(connection.info.lock().unwrap().rsync_ready);
        assert!(!connection.info.lock().unwrap().agent_ready);
        println!(
            "PASS: wrong password recovery and password authentication through OpenSSH askpass"
        );
    }
    // A truncated agent response can never become a valid inventory.
    let agent = c.agent_path();
    let original = fs::read(&agent).unwrap();
    fs::write(&agent, b"#!/bin/sh\nexit 0\n").unwrap();
    let invalid = remote_bench.scan(&[], &AtomicBool::new(false), &|_| {});
    fs::write(&agent, &original).unwrap();
    assert!(invalid.is_err());
    println!("PASS: interrupted agent protocol cannot produce a complete manifest");
    let known = app.path().app_config_dir().unwrap().join("ssh_known_hosts");
    let original_known = fs::read_to_string(&known).unwrap();
    let other = fs::read_to_string(root.join("encrypted_key.pub")).unwrap();
    let replacement = other.split_whitespace().nth(1).unwrap();
    let changed = original_known
        .lines()
        .map(|line| {
            let parts: Vec<_> = line.split_whitespace().collect();
            format!("{} {} {}", parts[0], parts[1], replacement)
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&known, changed).unwrap();
    let mut changed_profile = profile;
    changed_profile.id = "changed-host-key".into();
    let refused = ssh::connect(
        app.handle(),
        &app.state::<ssh::SshState>(),
        changed_profile,
        None,
    );
    fs::write(&known, original_known).unwrap();
    assert!(refused.is_err());
    println!("PASS: changed known host key is blocked without replacement");

    app.state::<ssh::SshState>()
        .connections
        .lock()
        .unwrap()
        .clear();
    println!("ALL SSH SMOKE CHECKS PASSED");
    Ok(())
}

/// Run only inside the disposable D-Bus/keyring session started by the test script.
pub fn keyring() -> Result<(), String> {
    assert_eq!(std::env::var("SYNCUI_ISOLATED_KEYRING").as_deref(), Ok("1"));
    let key = "syncui-isolated-test";
    if std::env::var_os("SYNCUI_KEYRING_UNAVAILABLE").is_some() {
        assert!(crate::credentials::access(key, Some("fixture-only-password"), false).is_err());
        println!(
            "PASS: unavailable Secret Service returns explicit error without plaintext fallback"
        );
        return Ok(());
    }
    crate::credentials::access(key, Some("fixture-only-password"), false)?;
    assert_eq!(
        crate::credentials::access(key, None, false)?.as_deref(),
        Some("fixture-only-password")
    );
    crate::credentials::access(key, None, true)?;
    assert_eq!(crate::credentials::access(key, None, false)?, None);
    println!("PASS: Secret Service save/read/delete in isolated session");
    Ok(())
}
