//! Milestone CLI: proves each layer on its own, outside the app.
//!
//!   cargo run -p pb-core --example probe -- status
//!   cargo run -p pb-core --example probe -- login
//!   cargo run -p pb-core --example probe -- peers
//!   cargo run -p pb-core --example probe -- dial 100.x.y.z:5555
//!   cargo run -p pb-core --example probe -- forward 100.x.y.z
//!   cargo run -p pb-core --example probe -- adb-test 100.x.y.z
//!   cargo run -p pb-core --example probe -- scrcpy 100.x.y.z [--force-adb-forward]
//!
//! Options (before the command): --state DIR (default: a "probe-state" folder
//! in the build cache, separate from the app's), --hostname NAME
//! (default phonebridge-probe), --auth-key tskey-...
//!
//! adb-test / scrcpy use the binaries placed by `cargo xtask fetch`.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pb_core::adb::Adb;
use pb_core::forwarder::{Dialer, Forwarder, bind_loopback};
use pb_core::tailnet::{TailnetManager, TailnetSnapshot, TailnetState};
use pb_core::tools::BundledTools;

fn usage() -> ExitCode {
    eprintln!(
        "usage: probe [--state DIR] [--hostname NAME] [--auth-key KEY] \
         <status|login|peers|dial IP:PORT|forward IP|adb-test IP|scrcpy IP [--force-adb-forward]>"
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
                .unwrap_or_else(|_| "info,tsnet=warn".into()),
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
    let hostname = opt("--hostname").unwrap_or_else(|| "phonebridge-probe".into());
    let auth_key = opt("--auth-key");
    let force_forward = if let Some(i) = args.iter().position(|a| a == "--force-adb-forward") {
        args.remove(i);
        true
    } else {
        false
    };
    let Some(cmd) = args.first().cloned() else {
        return usage();
    };
    let arg = args.get(1).cloned();

    println!("tailnet state dir: {}", state.display());
    let tn = TailnetManager::new(&state, &hostname);
    let started = match auth_key {
        Some(k) => tn.login_with_auth_key(k).await,
        None => tn.start().await,
    };
    if let Err(e) = started {
        eprintln!("failed to start tailnet node: {e}");
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

fn print_snapshot(s: &TailnetSnapshot) {
    println!("state: {:?}", s.state);
    if let Some(n) = &s.self_name {
        println!("this node: {n} {:?}", s.self_ips);
    }
    if let Some(t) = &s.tailnet {
        println!("tailnet: {t}");
    }
    for h in &s.health {
        println!("health: {h}");
    }
    if !s.peers.is_empty() {
        println!(
            "\n{:<28} {:<9} {:<8} {:<17} link",
            "peer", "os", "online", "ipv4"
        );
        for p in &s.peers {
            println!(
                "{:<28} {:<9} {:<8} {:<17} {:?}",
                p.display_name(),
                p.os,
                p.online,
                p.ipv4().map(|i| i.to_string()).unwrap_or_default(),
                p.link
            );
        }
    }
}

/// Start the node and report the first meaningful state (login URL or running).
async fn cmd_status(tn: &Arc<TailnetManager>) -> R {
    let mut rx = tn.subscribe();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let s = rx.borrow_and_update().clone();
        match &s.state {
            TailnetState::NeedsLogin {
                auth_url: Some(url),
            } => {
                println!("NeedsLogin. Open this URL to add the node to your tailnet:\n  {url}");
                return Ok(());
            }
            TailnetState::Running => {
                print_snapshot(&s);
                return Ok(());
            }
            TailnetState::Error { message } => return Err(message.clone()),
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

async fn wait_running(
    tn: &Arc<TailnetManager>,
    limit: Duration,
) -> Result<TailnetSnapshot, String> {
    let mut rx = tn.subscribe();
    let deadline = Instant::now() + limit;
    let mut shown_url = None::<String>;
    loop {
        let s = rx.borrow_and_update().clone();
        match &s.state {
            TailnetState::Running => return Ok(s),
            TailnetState::NeedsLogin {
                auth_url: Some(url),
            } if shown_url.as_deref() != Some(url) => {
                println!("Waiting for Tailscale login… open:\n  {url}");
                shown_url = Some(url.clone());
            }
            TailnetState::NeedsMachineAuth => {
                println!("Waiting for an admin to approve this node…")
            }
            _ => {}
        }
        if Instant::now() > deadline {
            return Err(format!(
                "tailnet not running after {}s (state {:?})",
                limit.as_secs(),
                s.state
            ));
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), rx.changed()).await;
    }
}

async fn cmd_dial(tn: &Arc<TailnetManager>, addr: SocketAddr) -> R {
    wait_running(tn, Duration::from_secs(60)).await?;
    let dialer = tn.dialer();
    let t = Instant::now();
    match dialer.dial(addr).await {
        Ok(_stream) => {
            println!(
                "OK: TCP connection to {addr} through the tailnet in {:?}",
                t.elapsed()
            );
            Ok(())
        }
        Err(e) => Err(format!(
            "dial {addr}: {e}{}",
            if e == pb_core::tailnet::DialError::Refused {
                "  (phone reachable, but adb isn't listening: run `adb tcpip 5555` over USB first)"
            } else {
                ""
            }
        )),
    }
}

async fn start_forwarder(tn: &Arc<TailnetManager>, ip: IpAddr) -> Result<Arc<Forwarder>, String> {
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

async fn cmd_forward(tn: &Arc<TailnetManager>, ip: IpAddr) -> R {
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

async fn cmd_adb_test(tn: &Arc<TailnetManager>, ip: IpAddr) -> R {
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

async fn cmd_scrcpy(tn: &Arc<TailnetManager>, ip: IpAddr, force_forward: bool) -> R {
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
