//! Milestone CLI: proves each layer on its own, outside the app.
//!
//!   cargo run -p pb-core --example probe -- login                  (browser login)
//!   cargo run -p pb-core --example probe -- --setup-key KEY login  (or a setup key)
//!   cargo run -p pb-core --example probe -- status
//!   cargo run -p pb-core --example probe -- peers
//!   cargo run -p pb-core --example probe -- dial 100.x.y.z:5555
//!   cargo run -p pb-core --example probe -- forward 100.x.y.z
//!   cargo run -p pb-core --example probe -- adb-test 100.x.y.z
//!   cargo run -p pb-core --example probe -- scrcpy 100.x.y.z [--force-adb-forward]
//!
//! Options (before the command): --state DIR (default: a "probe-state" folder
//! in the build cache, separate from the app's), --name NAME (NetBird device
//! name, default phonebridge-probe), --setup-key KEY, --management-url URL
//! (default NetBird Cloud). `login` saves the identity; later commands reuse it.
//!
//! adb-test / scrcpy use the binaries placed by `cargo xtask fetch`.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pb_core::adb::Adb;
use pb_core::forwarder::{Dialer, Forwarder, bind_loopback};
use pb_core::overlay::{DEFAULT_MANAGEMENT_URL, NetManager, NetSnapshot, NetState};
use pb_core::tools::BundledTools;

fn usage() -> ExitCode {
    eprintln!(
        "usage: probe [--state DIR] [--name NAME] [--setup-key KEY] [--management-url URL] [--relogin] \
         <login|status|peers|dial IP:PORT|forward IP|adb-test IP|scrcpy IP [--force-adb-forward]>"
    );
    ExitCode::from(2)
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn host_triple() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        _ => "unknown",
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,netbird=warn".into()),
        )
        .with_target(false)
        .init();

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut opt = |name: &str| -> Option<String> {
        let i = args.iter().position(|a| a == name)?;
        args.remove(i);
        (i < args.len()).then(|| args.remove(i))
    };
    let state = opt("--state")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace().join("target/phonebridge-cache/probe-state"));
    let name = opt("--name").unwrap_or_else(|| "phonebridge-probe".into());
    let setup_key = opt("--setup-key");
    let mgmt = opt("--management-url").unwrap_or_else(|| DEFAULT_MANAGEMENT_URL.into());
    let mut flag = |name: &str| -> bool {
        match args.iter().position(|a| a == name) {
            Some(i) => {
                args.remove(i);
                true
            }
            None => false,
        }
    };
    let force_forward = flag("--force-adb-forward");
    let relogin = flag("--relogin");
    let Some(cmd) = args.first().cloned() else {
        return usage();
    };
    let arg = args.get(1).cloned();

    println!(
        "NetBird state dir: {}\nmanagement server: {mgmt}",
        state.display()
    );
    let tn = NetManager::new(&state, &name, &mgmt);
    let reuse_login = cmd == "login" && setup_key.is_none() && !relogin && tn.has_saved_login();
    if reuse_login {
        println!("Already logged in; reusing the saved login (pass --relogin to sign in again).");
    }
    let started = if cmd == "login" && !reuse_login {
        match setup_key {
            Some(k) => tn.login_with_setup_key(k).await,
            None => tn.login_browser().await.map(|flow| {
                println!("Open this URL to log in to NetBird:\n  {}", flow.url);
                if let Some(code) = &flow.user_code {
                    println!("and enter the code: {code}");
                }
            }),
        }
    } else {
        tn.start().await
    };
    if let Err(e) = started {
        eprintln!("failed to start NetBird: {e}");
        return ExitCode::FAILURE;
    }

    let result = match cmd.as_str() {
        "status" => cmd_status(&tn).await,
        "login" => wait_running(&tn, Duration::from_secs(600))
            .await
            .map(|s| print_snapshot(&s)),
        "peers" => wait_running(&tn, Duration::from_secs(60))
            .await
            .map(|s| print_snapshot(&s)),
        "dial" => match arg.and_then(|a| a.parse::<SocketAddr>().ok()) {
            Some(addr) => cmd_dial(&tn, addr).await,
            None => return usage(),
        },
        "forward" => match arg.and_then(|a| a.parse::<IpAddr>().ok()) {
            Some(ip) => cmd_forward(&tn, ip).await,
            None => return usage(),
        },
        "adb-test" => match arg.and_then(|a| a.parse::<IpAddr>().ok()) {
            Some(ip) => cmd_adb_test(&tn, ip).await,
            None => return usage(),
        },
        "scrcpy" => match arg.and_then(|a| a.parse::<IpAddr>().ok()) {
            Some(ip) => cmd_scrcpy(&tn, ip, force_forward).await,
            None => return usage(),
        },
        _ => return usage(),
    };
    tn.shutdown().await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\nFAILED: {e}");
            ExitCode::FAILURE
        }
    }
}

type R = Result<(), String>;

fn print_snapshot(s: &NetSnapshot) {
    println!("state: {:?}", s.state);
    if let Some(n) = &s.self_name {
        println!("this node: {n} {:?}", s.self_ips);
    }
    if let Some(t) = &s.network {
        println!("management: {t}");
    }
    for h in &s.health {
        println!("health: {h}");
    }
    println!(
        "network map received: {}",
        if s.synced { "yes" } else { "no" }
    );
    if s.synced && s.peers.is_empty() {
        println!(
            "no peers visible: is the phone in the same NetBird account, and do access policies allow this device to reach it?"
        );
    }
    if !s.peers.is_empty() {
        println!("\n{:<28} {:<10} {:<17} link", "peer", "connected", "ipv4");
        for p in &s.peers {
            println!(
                "{:<28} {:<10} {:<17} {:?}",
                p.display_name(),
                p.online,
                p.ipv4().map(|i| i.to_string()).unwrap_or_default(),
                p.link
            );
        }
    }
}

/// Start the node and report the first meaningful state (login URL or running).
async fn cmd_status(tn: &Arc<NetManager>) -> R {
    let mut rx = tn.subscribe();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let s = rx.borrow_and_update().clone();
        match &s.state {
            NetState::NeedsLogin { reason, .. } => {
                println!(
                    "Not logged in{}. Run `probe login` (browser) or `probe --setup-key KEY login`.",
                    reason
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                );
                return Ok(());
            }
            NetState::Running => {
                drop(rx);
                print_snapshot(&wait_synced(tn, s).await);
                return Ok(());
            }
            NetState::Error { message, .. } => return Err(message.clone()),
            _ => {}
        }
        if Instant::now() > deadline {
            return Err(format!(
                "no login URL or Running state after 45s (last: {:?})",
                s.state
            ));
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), rx.changed()).await;
    }
}

/// After Running, wait (up to 20 s) for the first network map so the peer list is complete.
async fn wait_synced(tn: &Arc<NetManager>, mut s: NetSnapshot) -> NetSnapshot {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !s.synced && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        s = tn.refresh().await;
    }
    if !s.synced {
        println!("note: no network map from NetBird after 20 s; the peer list may be incomplete");
    }
    s
}

async fn wait_running(tn: &Arc<NetManager>, limit: Duration) -> Result<NetSnapshot, String> {
    let mut rx = tn.subscribe();
    let deadline = Instant::now() + limit;
    let mut shown_url = None::<String>;
    loop {
        let s = rx.borrow_and_update().clone();
        match &s.state {
            NetState::Running => return Ok(wait_synced(tn, s).await),
            NetState::NeedsLogin {
                auth_url: Some(url),
                ..
            } if shown_url.as_deref() != Some(url) => {
                println!("Waiting for NetBird login… open:\n  {url}");
                shown_url = Some(url.clone());
            }
            NetState::NeedsLogin {
                auth_url: None,
                reason,
                ..
            } => {
                return Err(format!(
                    "not logged in to NetBird{}; run `probe login` first",
                    reason
                        .as_deref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                ));
            }
            _ => {}
        }
        if Instant::now() > deadline {
            return Err(format!(
                "NetBird not running after {}s (state {:?})",
                limit.as_secs(),
                s.state
            ));
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), rx.changed()).await;
    }
}

async fn cmd_dial(tn: &Arc<NetManager>, addr: SocketAddr) -> R {
    wait_running(tn, Duration::from_secs(60)).await?;
    let dialer = tn.dialer();
    let t = Instant::now();
    match dialer.dial(addr).await {
        Ok(_stream) => {
            println!(
                "OK: TCP connection to {addr} through NetBird in {:?}",
                t.elapsed()
            );
            Ok(())
        }
        Err(e) => Err(format!(
            "dial {addr}: {e}{}",
            if e == pb_core::overlay::DialError::Refused {
                "  (phone reachable, but adb isn't listening: run `adb tcpip 5555` over USB first)"
            } else {
                ""
            }
        )),
    }
}

async fn start_forwarder(tn: &Arc<NetManager>, ip: IpAddr) -> Result<Arc<Forwarder>, String> {
    wait_running(tn, Duration::from_secs(60)).await?;
    let listener = bind_loopback(None).await.map_err(|e| e.to_string())?;
    let fwd = Forwarder::start(listener, SocketAddr::new(ip, 5555), tn.dialer())
        .map_err(|e| e.to_string())?;
    println!(
        "forwarder: {} -> {ip}:5555 (adb serial {})",
        fwd.local_addr(),
        fwd.adb_serial()
    );
    Ok(fwd)
}

async fn cmd_forward(tn: &Arc<NetManager>, ip: IpAddr) -> R {
    let fwd = start_forwarder(tn, ip).await?;
    println!("Ctrl-C to stop. Try:  adb connect {}", fwd.adb_serial());
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tick.tick() => println!("stats: {:?}", fwd.stats()),
        }
    }
    fwd.stop().await;
    Ok(())
}

fn tools() -> Result<BundledTools, String> {
    let t = BundledTools::from_source_tree(&workspace(), host_triple(), 25037);
    t.check()
        .map_err(|e| format!("{e} (run `cargo xtask fetch` first)"))?;
    println!(
        "using adb    {}\nusing scrcpy {}",
        t.adb.display(),
        t.scrcpy.display()
    );
    Ok(t)
}

async fn adb_connect(adb: &Adb, serial: &str) -> R {
    adb.start_server().await.map_err(|e| e.to_string())?;
    println!("adb: {}", adb.version().await.unwrap_or_default());
    adb.connect(serial).await.map_err(|e| e.to_string())?;
    for _ in 0..20 {
        if let Ok(Some(state)) = adb.get_state(serial).await {
            println!("adb state: {state:?}");
            if state == pb_core::adb::DeviceState::Device {
                return Ok(());
            }
            if state == pb_core::adb::DeviceState::Unauthorized {
                println!("  -> accept \"Allow USB debugging?\" on the phone");
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("device never became ready".into())
}

async fn cmd_adb_test(tn: &Arc<NetManager>, ip: IpAddr) -> R {
    let tools = tools()?;
    let adb = Adb::new(tools);
    let fwd = start_forwarder(tn, ip).await?;
    let serial = fwd.adb_serial();
    let res = async {
        adb_connect(&adb, &serial).await?;
        let t = Instant::now();
        let echo = adb
            .shell(&serial, "echo ok", Duration::from_secs(15))
            .await
            .map_err(|e| e.to_string())?;
        println!("shell echo: {:?} in {:?}", echo.trim(), t.elapsed());
        let model = adb
            .getprop(&serial, "ro.product.model")
            .await
            .unwrap_or_default();
        println!("model: {model}");

        // Throughput: push 20 MB of random data.
        let tmp = std::env::temp_dir().join("phonebridge-probe-20mb.bin");
        let mut data = vec![0u8; 20 * 1024 * 1024];
        let mut x: u32 = 0x1234_5678;
        for b in data.iter_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
        std::fs::write(&tmp, &data).map_err(|e| e.to_string())?;
        let before = fwd.stats();
        let t = Instant::now();
        let out = adb
            .run(
                &[
                    "-s",
                    &serial,
                    "push",
                    &tmp.to_string_lossy(),
                    "/data/local/tmp/phonebridge-probe.bin",
                ],
                Duration::from_secs(600),
            )
            .await
            .map_err(|e| e.to_string())?;
        let secs = t.elapsed().as_secs_f64();
        println!("push: {}", out.text());
        println!(
            "push throughput: {:.2} MB/s ({:.1} Mbit/s)",
            20.97 / secs,
            20.97 * 8.0 / secs
        );
        let _ = adb
            .shell(
                &serial,
                "rm -f /data/local/tmp/phonebridge-probe.bin",
                Duration::from_secs(10),
            )
            .await;
        let _ = std::fs::remove_file(&tmp);
        let after = fwd.stats();
        println!(
            "forwarder carried {} B up during push; connections total {} (active {})",
            after.bytes_up - before.bytes_up,
            after.total_connections,
            after.active
        );
        Ok::<_, String>(())
    }
    .await;
    let _ = adb.disconnect(&serial).await;
    let _ = adb.kill_server().await;
    fwd.stop().await;
    res
}

async fn cmd_scrcpy(tn: &Arc<NetManager>, ip: IpAddr, force_forward: bool) -> R {
    let tools = tools()?;
    let adb = Adb::new(tools.clone());
    let fwd = start_forwarder(tn, ip).await?;
    let serial = fwd.adb_serial();
    let res = async {
        adb_connect(&adb, &serial).await?;
        let scrcpy = pb_core::scrcpy::Scrcpy::new(tools.clone());
        println!("scrcpy: {}", scrcpy.version().await.unwrap_or_default());
        let mut args = vec![format!("--serial={serial}"), "-V".into(), "debug".into(), "--max-size=1024".into()];
        if force_forward {
            args.push("--force-adb-forward".into());
        }
        println!("running scrcpy {}  (close the window to finish)", args.join(" "));
        let before = fwd.stats();
        let mut child = scrcpy
            .command(&args)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| e.to_string())?;
        let pid = child.id().unwrap_or(0);

        // While it runs: show tunnels and which sockets scrcpy/adb hold.
        tokio::time::sleep(Duration::from_secs(6)).await;
        let rev = adb.run(&["-s", &serial, "reverse", "--list"], Duration::from_secs(10)).await.map(|o| o.text());
        let fwdl = adb.run(&["forward", "--list"], Duration::from_secs(10)).await.map(|o| o.text());
        println!("\nadb reverse --list: {:?}\nadb forward --list: {:?}", rev, fwdl);
        if cfg!(target_os = "macos") {
            let lsof = std::process::Command::new("/usr/sbin/lsof")
                .args(["-nP", "-a", "-i", "-p", &pid.to_string()])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            println!("scrcpy sockets (expect only 127.0.0.1):\n{lsof}");
        }
        println!("forwarder now: {:?}", fwd.stats());

        let status = child.wait().await.map_err(|e| e.to_string())?;
        let after = fwd.stats();
        println!(
            "\nscrcpy exited ({status}). Forwarder carried {} B up / {} B down during the session over {} connection(s) total.",
            after.bytes_up - before.bytes_up,
            after.bytes_down - before.bytes_down,
            after.total_connections
        );
        Ok::<_, String>(())
    }
    .await;
    let _ = adb.disconnect(&serial).await;
    let _ = adb.kill_server().await;
    fwd.stop().await;
    res
}
