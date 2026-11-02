# PhoneBridge — Phase 1 Plan (control an Android phone from a Mac over an embedded tailnet)

## Context
You want a Phone Link-style Mac app: a single self-contained Tauri v2 `.app` that mirrors and controls your Android phone with scrcpy. It joins your tailnet itself through userspace Tailscale, so it works when the phone is on cellular. It needs no Homebrew, no system adb or scrcpy, and no Tailscale.app.

Your decisions so far:
- **Name:** PhoneBridge, bundle id `io.cjscode.phonebridge`, tailnet hostname `phonebridge-mac`. ("Phone Link" is a Microsoft trademark.)
- **Architecture:** Apple Silicon only (`aarch64-apple-darwin`). Minimum macOS is 13 Ventura, because Go 1.27, which libtailscale requires, needs macOS 13.
- **Build:** on your Mac with only Xcode Command Line Tools and Rust. No npm, and no Go installed by you.
- **Tailscale:** libtailscale. Its `build.rs` downloads a pinned, SHA-256-checked Go toolchain into `target/` at build time only.
- **Repo:** a new repo you create. Tell me its name (for example `cjscode/phonebridge`) and I'll attach it and work on `claude/keen-hypatia-5httn8`.

---

## 1. Verified facts (checked against sources on 2026-11-02, not from memory)

### Tauri v2 (tauri 2.12.5, tauri-cli 2.12.4, plugin-store 2.5.0, plugin-opener 2.7.2, plugin-single-instance 2.5.5)
- `bundle.externalBin: ["binaries/adb", "binaries/scrcpy"]` is resolved relative to `src-tauri/`. Each file on disk must carry the target-triple suffix: `binaries/adb-aarch64-apple-darwin`. Get the triple with `rustc --print host-tuple`. A universal build would need `-universal-apple-darwin`.
- The bundler source (`tauri-bundler/src/bundle/macos/app.rs`) copies externalBin into **`Contents/MacOS/`** with the suffix stripped. It **signs them before the app**, inside-out, with the configured `signingIdentity`. `"-"` means ad-hoc. Resources go to `Contents/Resources/`.
- So: adb and scrcpy become **externalBin** (signed executables in `MacOS/`). `scrcpy-server` is an APK (plain data) and goes in **resources**. Executables are never placed in Resources.
- I won't use tauri-plugin-shell. The backend spawns `current_exe().parent()/adb` and `/scrcpy` with `tokio::process`. That gives full control of env vars, process groups, signals, and stdout parsing, and needs no shell capability.
- No npm: `cargo install tauri-cli --version 2.12.4 --locked`, then `cargo tauri build`. The frontend is static files loaded through `frontendDist`, and `app.withGlobalTauri: true` exposes `window.__TAURI__`, so no JS bundler is needed.

### scrcpy 5.0.1 (released 2026-10-08, latest)
- macOS assets: `scrcpy-macos-aarch64-v5.0.1.tar.gz`, SHA-256 `33611e51977a8289e2e124b220f727895181ef2eb9060e0987a4dc2db9cf0d6b`. I downloaded it and verified the hash.
- Tarball contents: `scrcpy` (arm64, statically linked, minimum macOS 11, system frameworks only), `scrcpy-server` (APK), `adb`, `scrcpy.png`, `disconnected.png`, `LICENSE`.
- platform-tools: `platform-tools_r37.0.1-darwin.zip`, SHA-256 `ee39ad5967e95c2a07f04dbcbde96b1a0c916ba376096db5d2f498b7727a5d1d`. This is the current "latest" build and matches scrcpy's own pinned hash. Its `adb` is a universal binary and is **byte-identical** to the adb in the scrcpy tarball. As you asked, I'll take adb from platform-tools.
- Environment variables (from `cli.c`): `ADB`, `SCRCPY_SERVER_PATH`, `SCRCPY_ICON_DIR` (holds scrcpy.png and disconnected.png), `ANDROID_SERIAL`.
- `--new-display[=WxH][/dpi]` requires Android 10+. The display is destroyed on exit. Related flags: `-x/--flex-display` (resizes the display with the window; incompatible with `--window-width/height` and `--crop`), `--no-vd-destroy-content`, `--no-vd-system-decorations`, `--keep-active`.
- `--start-app=<pkg>`. A `+` prefix force-stops the app first. A `?` prefix looks the app up by name, which is slow, so I'll always pass package names.
- `--list-apps` output, from the server's `LogUtils.buildAppListMessage`: each line is ` * ` (system app) or ` - `, then the name padded to 30 columns, then a space and the package. Names of 30 characters or more wrap, and the package goes on the next indented line. The parser handles both cases. There are no icons, so the grid uses letter tiles in the MVP.
- Other flags used: `-s`, `--max-size`, `--video-bit-rate`, `--max-fps`, `--audio-bit-rate`, `--no-audio`, `--window-title`, `--video-codec`, `--force-adb-forward`.

### libtailscale (commit `ac7ed43`, 2026-10-30; BSD-3; go.mod needs Go ≥ 1.27.1 and tailscale.com v1.104.1)
- C API: `tailscale_new`, `set_dir`, `set_hostname`, `set_authkey`, `set_logfd`, `start`, `up`, `close`, `getips`, `dial(sd,"tcp","ip:port",&fd)`, `listen`/`accept`, `loopback` (LocalAPI over HTTP plus SOCKS5, with creds), `status_json` (the ipnstate.Status JSON, newer API), `errmsg`.
- **Login URL:** there is no dedicated call, but tsnet runs `StartLoginInteractive` when the state is `NeedsLogin` (checked in tsnet.go v1.104.1). The URL then appears as **`AuthURL`** in `tailscale_status_json`. `BackendState` gives NeedsLogin, Starting, Running, and so on, and `Peer{}` lists each peer's HostName, OS, TailscaleIPs, Online, CurAddr, and Relay.
- **Logout:** `POST /localapi/v0/logout` through `tailscale_loopback`, with the `Sec-Tailscale: localapi` header and basic-auth creds. This is the same method the official Swift TailscaleKit uses.
- `dial` returns a socketpair fd, which can be wrapped as a tokio `UnixStream`. The 10-30 commit marks handed-out fds close-on-exec. **This matters here** because we spawn adb and scrcpy, and the tailnet fds must not leak into them. So the pin must be at or after `ac7ed43`.
- Go toolchain to auto-fetch: `go1.27.3.darwin-arm64.tar.gz`, SHA-256 `dd3908cf20959d798c1e06ebe5b72213f0505ce01373b40ce3110975a6cd99c2`.

### Tailscale options compared → **libtailscale + our own small `-sys` crate**
| Option | Verdict |
|---|---|
| **Own `libtailscale-sys` (vendored C/Go sources at ac7ed43, auto-fetched Go)** | **Chosen.** Mature tsnet with everything we need. We control the pin. |
| messense `libtailscale` 0.2.2 crate | Its submodule is pinned to a Feb-2026 commit that lacks `status_json` and the close-on-exec fix. Its build.rs needs Go on PATH. |
| tailscale-rs 0.6.1 (official, pure Rust) | No Go needed, but it's WIP: no list-peers API, no logout, no peer OS field, NAT traversal still WIP. A good candidate to revisit later. |
| Go tsnet helper sidecar | This is the fallback if c-archive linking fails (for example Go runtime vs. Tauri signal or thread conflicts). The cost is an extra process and IPC. |

### Licensing
- **scrcpy:** Apache-2.0. The binary statically includes FFmpeg 9.0.2 (LGPL, built without GPL options), libusb 1.0.30 (LGPL-2.1), SDL3 (zlib), and dav1d (BSD). We redistribute it unmodified, so we ship its LICENSE plus a notice pointing to scrcpy's source and `app/deps` build scripts. Not a concern for personal use.
- **adb:** the source is AOSP Apache-2.0, and `NOTICE.txt` ships in the zip, which we'll bundle. However, the dl.google.com download falls under the Android SDK License Agreement, which restricts redistributing the SDK "except as required by third-party licenses". scrcpy and Homebrew both redistribute adb anyway. **Personal use: fine. Public distribution: a gray area.** The clean alternative is building adb from AOSP.
- **libtailscale / tailscale.com / Go:** BSD-3, plus mostly BSD/MIT/Apache Go module dependencies. build.rs writes the module list and copies their LICENSE files into `resources/licenses/`. Don't use the Tailscale name or logo in branding.

### Apple Silicon only vs. universal
Apple Silicon only means one libtailscale build, a smaller app (roughly 45 MB less), half the build time, and no x86 slice that you can't test. A universal build would lipo the two official scrcpy builds, build libtailscale twice, and use `--target universal-apple-darwin`. The fetch code stays triple-aware so universal can be added later.

---

## 2. Architecture

```
scrcpy (per window) ──adb client──► private adb server (ANDROID_ADB_SERVER_PORT=<ours>)
                                         │  one adb transport (TCP)
                                         ▼
                         127.0.0.1:<fwd port>  (Rust forwarder, tokio)
                                         │  tailscale_dial("tcp","100.x.y.z:5555") → socketpair
                                         ▼
                         libtailscale/tsnet (in-process, userspace netstack, WireGuard/DERP)
                                         ▼
                                  phone adbd :5555 (after `adb tcpip 5555`)
```

**Does one forwarder carry everything?** Per `app/src/adb/adb_tunnel.c`, scrcpy first tries `adb reverse` and falls back to `adb forward`.
- With **reverse**, the device opens a stream that adbd sends to the host adb server over the transport. The adb server then connects to `127.0.0.1:27183`.
- With **forward**, scrcpy connects to the local adb server, which multiplexes the stream over the transport.

Either way, video, audio, control, and the server push all ride the **single adb transport TCP connection**, which is the forwarder. The only thing that would bypass it is scrcpy's `--tcpip` auto mode, which dials the phone's IP directly, and we never use that.

In Phase 2 I'll prove this rather than trust it:
- run scrcpy with `-V debug` and log whether reverse succeeded
- check `adb reverse --list`
- check with `lsof -nP -p` that adb and scrcpy hold only 127.0.0.1 sockets
- check that the forwarder's byte counters account for the stream
- repeat with `--force-adb-forward`

The app keeps `--force-adb-forward` as a setting in case reverse misbehaves on a particular phone.

**Isolation from system tools:**
- Every adb and scrcpy child gets `ADB=<bundle>/MacOS/adb`, `SCRCPY_SERVER_PATH=<Resources>/scrcpy-server`, `SCRCPY_ICON_DIR`, and a **private `ANDROID_ADB_SERVER_PORT`**, so we never touch or kill a Homebrew adb server on 5037.
- PATH is set to `/usr/bin:/bin`.

**Forwarder port:** picked once from the 20000–60000 range and saved, so the adb serial `127.0.0.1:<port>` stays stable. It avoids 5554–5585, which adb scans as emulator ports.

**Phone state detection:**
- `tailscale_dial` refused (RST) → **"Phone rebooted — plug in USB once"**
- Peer offline in status, or dial timeout → **"Phone not reachable on tailnet"**
- BackendState not Running → **"Tailnet offline / logged out"**
- adb `unauthorized` → **"Accept the prompt on your phone"**

---

## 3. File structure

```
phonebridge/
  Cargo.toml                 workspace: crates/*, src-tauri, xtask
  rust-toolchain.toml        stable; target aarch64-apple-darwin
  .cargo/config.toml         alias: xtask = "run -p xtask --"
  deps.toml                  single source of pins: scrcpy, platform-tools, go, libtailscale commit + SHA-256s
  README.md  THIRD_PARTY_NOTICES.md  docs/{ARCHITECTURE.md, TESTING.md}
  crates/
    fetch/                   download → SHA-256 verify → extract (ureq, sha2, flate2, tar, zip); used by build.rs + xtask
    libtailscale-sys/
      vendor/libtailscale/   tailscale.go/.c/.h, fd_*.go, go.mod, go.sum, LICENSE (pinned ac7ed43)
      build.rs               fetch Go into target/.toolchains → `go build -buildmode=c-archive -trimpath`
                             (CC=xcrun clang, MACOSX_DEPLOYMENT_TARGET=13.0, GOTOOLCHAIN=local,
                             GOMODCACHE/GOCACHE in target/) → link static=tailscale + CoreFoundation,
                             Security, resolv. Override: PHONEBRIDGE_GO or PHONEBRIDGE_LIBTAILSCALE_A
      src/lib.rs             extern "C" decls for tailscale.h
    pb-core/                 no Tauri dependency; unit-testable on Linux
      src/tailnet/{node.rs, status.rs, localapi.rs, mod.rs}   safe wrapper, Status serde, logout, manager + events
      src/forwarder.rs       127.0.0.1 listener; Dialer trait; spawn_blocking dial → UnixStream ↔ TcpStream
                             copy_bidirectional; TCP_NODELAY; per-conn byte counters; error classification
      src/adb.rs             runs bundled adb with private env and timeouts; devices/track-devices parse;
                             tcpip; connect/disconnect; tun0 IP lookup; kill-server
      src/scrcpy.rs          arg builder (preset × mode), spawn, list-apps parser, stderr ring buffer
      src/connection.rs      state machine + reconnect with backoff (1s→2→4…30s cap, reset on success)
      src/presets.rs  src/procs.rs (process groups, PID file, stale cleanup)  src/error.rs
      examples/probe.rs      Phase-2 milestone CLI: login | peers | dial IP:5555 | forward | adb-test
      tests/                 fixtures (list-apps, devices, status JSON), fake-adb script, forwarder tests
  src-tauri/
    build.rs                 crates/fetch → binaries/ + resources/, chmod +x, then tauri_build::build()
    tauri.conf.json          identifier io.cjscode.phonebridge; externalBin [adb, scrcpy];
                             resources {scrcpy-server, scrcpy-icons/, licenses/}; macOS signingIdentity "-",
                             hardenedRuntime, minimumSystemVersion 13.0; bundle targets app, dmg
    capabilities/default.json   core + store + opener only
    binaries/ resources/     (gitignored, generated)
    src/{main.rs, lib.rs, commands.rs, state.rs, settings.rs, tray.rs, paths.rs, events.rs}
  ui/                        plain HTML/CSS/ES-module JS, `// @ts-check` + JSDoc types (no compiler needed)
    index.html styles.css js/{main.js, api.js, store.js}
    js/views/{login.js, setup.js, home.js, settings.js}  js/components/{status-banner.js, app-grid.js, toast.js}
  xtask/                     `fetch`, `verify-bundle` (codesign --verify --deep --strict, lipo -info, +x, paths),
                             `dist` (= fetch + cargo tauri build + verify-bundle)
```

**Frontend choice:** vanilla JS (not TS or Svelte). Both alternatives need a compiler or npm, which you ruled out. The app has four small views, and `// @ts-check` gives editor type checking for free. It also matches your JS/HTML/CSS background.

## 4. Backend behavior (key points)
- **Tailnet manager:**
  - Creates the node with state dir `~/Library/Application Support/io.cjscode.phonebridge/tailscale` and hostname `phonebridge-mac`. Logs go through `set_logfd` into `tracing`, written to `~/Library/Logs/io.cjscode.phonebridge/`.
  - Calls `start()` and polls `status_json` every second (every 5 s once Running).
  - Emits `tailnet://status` events: LoggedOut, NeedsLogin{url}, Connecting, Online{self_ip, peers}, Error.
  - On NeedsLogin it opens AuthURL with tauri-plugin-opener.
  - Auth key: close the node, create a new one with `set_authkey` and `TSNET_FORCE_LOGIN=1`.
  - Log out: LocalAPI logout, then `close()`, then restart, which yields a fresh login URL.
- **Peers:** listed from status. Android peers are sorted first and show online and direct/relayed.
- **USB setup:**
  1. Track USB devices (serials without `:`).
  2. Read the phone's tailnet IP from `adb shell ip -4 addr show tun0` and **pre-select the matching peer**. You confirm or pick another.
  3. Run `adb -s <usb> tcpip 5555` and wait for adbd to restart.
  4. Start the forwarder, then `adb connect 127.0.0.1:<port>`, then verify the state is `device`.
  5. Save the phone (stable node id, IP, name).
- **Reconnect:**
  - Triggered on launch, when `adb track-devices` reports the device gone or offline, when the forwarder's upstream closes, and on tailnet state changes.
  - Before each `adb connect`, a probe dial classifies the failure so the UI shows the right state.
- **Windows:**
  - Each app runs `scrcpy -s 127.0.0.1:<port> --new-display=<WxH/dpi> --start-app=<pkg> --window-title="<App> — PhoneBridge" --keep-active --no-audio` plus preset flags.
  - Full mirror is the same without new-display, with audio on.
  - Each window is its own process group, kept in a registry. When a window exits with an error, the UI gets a toast with the last stderr lines.
- **Presets:**
  - **Wi-Fi:** `--max-size=1920 --video-bit-rate=8M --max-fps=60`; app display `1080x1920/420`.
  - **Cellular (low data):** `--max-size=1024 --video-bit-rate=1500K --max-fps=30 --audio-bit-rate=64K`; app display `720x1280/320`.
  - Optional toggles: H.265, flex display.
- **App list:** from `scrcpy --list-apps`, cached in the store with a manual refresh. Each run pushes about 0.7 MB to the phone.
- **Store** (`settings.json`): phone, forwarder/adb ports, favorites, preset, cached apps, force-adb-forward, window options.
- **Tray:** status line, favorites submenu, Full mirror, Show window, Quit. It updates on status events.
- **Shutdown** (`RunEvent::ExitRequested`):
  1. SIGTERM to scrcpy process groups, then SIGKILL after 2 s. scrcpy cleans its tunnels and the virtual display is destroyed.
  2. `adb disconnect`, then `adb kill-server` on our port.
  3. Stop the forwarder.
  4. `tailscale_close`.
  - On the next launch, stale PIDs from the PID file are killed if their executable lives in our bundle.
- **Errors:** every command returns `Result<T, AppError{code, message, hint}>`, and background failures emit `app://error`. The UI shows a persistent banner for connection state and toasts for one-off failures. Nothing is only logged.

## 5. Risks
1. **Throughput and latency through the userspace path.** The local hops (Go netstack → socketpair → Rust → loopback → adb) cost under 1 ms and reach hundreds of Mb/s, far above scrcpy's 1.5–8 Mb/s. The real risks are the network path:
   - On cellular CGNAT, Tailscale may fall back to a **DERP relay**: higher RTT, capped throughput, and TCP-over-TCP stalls.
   - adb multiplexes video, audio, and control on **one TCP stream**, so packet loss causes head-of-line blocking.
   - Mitigations: the cellular preset, a "Direct / Relayed via <region>" indicator from `Peer.CurAddr/Relay`, and the probe's throughput test (an `adb push` of 20 MB).
2. **Go c-archive inside the Tauri process** (Go runtime threads and signal handlers next to tokio and WebKit). This usually works, and I'll check it early in Milestone 1. Fallback: a tsnet helper sidecar.
3. **adbd over tailnet on cellular:**
   - `adb tcpip` is lost on every phone reboot.
   - Some OEMs or Android versions may drop it when the network changes.
   - The Tailscale Android app must stay connected (turn off battery optimization).
   - Tailnet ACLs must allow `phonebridge-mac → phone:5555`.
   - adbd also listens on the phone's Wi-Fi. It still requires RSA authorization, but I'll recommend an ACL or grant restricting port 5555.
4. **Two adb servers fighting over USB** during setup, if Homebrew adb is running. The app detects a listener on 5037 and warns.
5. **One Dock icon per scrcpy window.** Each window is a separate SDL process. That's acceptable for the MVP, and the window titles help.
6. **Audio:** only one capture at a time, so audio is off for app windows by default.
7. **I can't run macOS here.** On-device and packaging tests depend on you running the checkpoints below on your Mac.

## 6. Phase 2 build order and verification
I write and unit-test everything that's platform-independent in this Linux sandbox:
- parsers
- arg builder
- backoff and state machine against a fake-adb script
- forwarder with a mock dialer: echo, 100 MB throughput, half-close, refused vs. timeout
- libtailscale-sys build on Linux with auto-fetched Go

Then **you** run three checkpoints on your Mac and paste the output:
1. **Tailnet:**
   - `cargo run -p pb-core --example probe -- login` (browser login)
   - `-- peers`
   - `-- dial <phone-ip>:5555`, after running `adb tcpip 5555` once over USB
   - `-- forward`, which prints the port
2. **adb:** `probe -- adb-test` connects through the forwarder, then runs `shell echo ok`, `reverse --list`, and a 20 MB push throughput test.
3. **scrcpy:**
   - `probe -- scrcpy` checks reverse/forward, `lsof`, and byte counters.
   - Then `cargo tauri dev` for the full UI.
   - Then the disruption checklist in `docs/TESTING.md`, with the expected UI state for each step:
     - kill adb
     - airplane mode
     - stop Tailscale on the phone
     - reboot the phone
     - Mac Wi-Fi off
     - sleep and wake
     - quit with three windows open, then confirm with `pgrep` that nothing is left
     - `kill -9` the app, then relaunch and confirm the cleanup

## 7. Phase 3 packaging
- Run `cargo xtask dist`, which runs `cargo tauri build --bundles app,dmg` and then `verify-bundle`:
  - `codesign --verify --deep --strict --verbose=2` on the app
  - `codesign -dv` on each nested binary
  - executable bits set
  - `lipo -info`
- Isolation test:
  - `which -a adb scrcpy`, then `brew unlink scrcpy android-platform-tools` (or rename the binaries)
  - quit Tailscale.app
  - launch PhoneBridge.app
  - the Diagnostics panel shows the resolved binary paths and versions, all inside the bundle
  - `ps -o command` and `lsof` confirm the same
  - the admin console shows `phonebridge-mac` as its own node
- **First open:**
  - An app built locally has no quarantine flag, so it opens normally.
  - If it's downloaded or AirDropped, macOS 15+ blocks it. Fix: System Settings → Privacy & Security → **Open Anyway**, or `xattr -dr com.apple.quarantine /Applications/PhoneBridge.app`.
- **Notarizing later:**
  - Join the Apple Developer Program ($99/yr) and get a "Developer ID Application" certificate.
  - Set `signingIdentity` to it.
  - Export `APPLE_API_ISSUER`, `APPLE_API_KEY`, and `APPLE_API_KEY_PATH` (or `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`).
  - Tauri then signs the nested binaries with hardened runtime and a timestamp, notarizes, and staples. No extra entitlements are expected.
- **README** covers:
  - phone: Developer options, USB debugging, Tailscale app with battery optimization off
  - Mac: install Xcode CLT and rustup, `cargo install tauri-cli`, `cargo xtask dist`, open the app, log in to Tailscale in the browser
  - plug in once and click **Set up via USB**
  - after a reboot: plug in and click **Re-enable** (one click, no peer picking)

## Next step after approval
Attach your new repo, commit this plan as `docs/PLAN.md`, then start Phase 2 with `crates/fetch`, `libtailscale-sys`, and the tailnet layer plus `probe`.
