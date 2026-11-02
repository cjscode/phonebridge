# Testing PhoneBridge

## Automated (any machine)

```bash
cargo test --workspace
```

This covers:
- `fetch`: pins, SHA-256, and rejecting a wrong hash.
- `pb-core` unit tests: NetBird status parsing (direct / relayed / not connected peers, null lists, management warnings), login-error classification and friendly messages, NetBird manager input checks, dial error classification, adb `devices -l` and phone-IP parsing, the `--list-apps` parser (fixture made with scrcpy's own formatting rules, including wrapped long names), scrcpy argument building, and process registry (stderr tail, process-group kill, crash cleanup that only kills matching executables).
- Forwarder: echo, byte counters, a 100 MB throughput run, refused-dial reporting, retargeting, and stopping.
- `pb-core/tests/connection.rs`: the connection state machine against a fake `adb` and a scripted dialer. It covers connect, phone needs USB (refused), peer not connected plus dial timeout, a peer that looks idle but answers, dial timeout, NetBird drop and recovery, unauthorized then allowed, device drop mid-session, phone stops answering (reboot), user disconnect, and NetBird IP change.

Checked live from the build sandbox against NetBird Cloud: `probe login` returns a real browser-login URL (PKCE), and a fake setup key is rejected by the server and reported as "log in again".

## Mac checkpoints (Phase 2)

Run these on the Mac, in order, and paste the output back. Each one proves a layer before the next is added. `probe` keeps its own NetBird identity (`phonebridge-probe`, state in `target/phonebridge-cache/probe-state`), separate from the app. Remove it from the NetBird dashboard (Peers) when you're done.

### Checkpoint 1: NetBird only

```bash
cargo run -p pb-core --example probe -- login
```

If the probe already has a saved login, `login` just reuses it (add `--relogin` to sign in again). Otherwise, open the printed URL and sign in (or use `cargo run -p pb-core --example probe -- --setup-key YOUR-KEY login`; add `--management-url https://…` for a self-hosted server). Expected: `state: Running`, then a peer table that includes your phone, `connected: true` within a few seconds.

```bash
cargo run -p pb-core --example probe -- peers
```

With the phone plugged in once, run `adb tcpip` through the bundled adb. Use a private server port (as the app does), and stop that server afterwards, so no adb server is left holding the phone's USB connection on the standard port 5037:

```bash
cargo xtask fetch
```

```bash
ANDROID_ADB_SERVER_PORT=25037 src-tauri/binaries/adb-aarch64-apple-darwin tcpip 5555
```

```bash
ANDROID_ADB_SERVER_PORT=25037 src-tauri/binaries/adb-aarch64-apple-darwin kill-server
```

Then dial the phone's adb port through NetBird (use its address from the peer table):

```bash
cargo run -p pb-core --example probe -- dial 100.x.y.z:5555
```

Expected: `OK: TCP connection to 100.x.y.z:5555 through NetBird`. "connection refused" means adb tcpip isn't on; a timeout means the phone isn't reachable.

```bash
cargo run -p pb-core --example probe -- forward 100.x.y.z
```

This prints the forwarder port. Leave it running; the next checkpoint starts its own.

### Checkpoint 2: adb through the forwarder

```bash
cargo run -p pb-core --example probe -- adb-test 100.x.y.z
```

Expected:
- `adb state: Device`
- `shell echo: "ok"`
- the phone model
- a push throughput line
- "forwarder carried ~21000000 B up during push; connections total 1"

Run it once on Wi-Fi and once with the phone on cellular, and note both throughputs.

### Checkpoint 3: scrcpy through the forwarder (tunnel check)

```bash
cargo run -p pb-core --example probe -- scrcpy 100.x.y.z
```

A mirror window opens. After about 6 s the probe prints:
- `adb reverse --list`: expect a `localabstract:scrcpy_xxxxxxxx tcp:27183` entry (or empty with a "fallback to adb forward" line in scrcpy's log)
- the sockets held by scrcpy: expect only `127.0.0.1` addresses, nothing to 100.x or the internet
- forwarder stats

Close the window. The final line shows the bytes the forwarder carried during the session, which should be in the megabytes for video. Then repeat with forward mode:

```bash
cargo run -p pb-core --example probe -- scrcpy 100.x.y.z --force-adb-forward
```

Expected: the same behaviour, and `adb forward --list` shows the tunnel. Together these confirm the single forwarder carries video, audio and control in both tunnel modes.

### Then the app

```bash
cd src-tauri && cargo tauri dev
```

## Disruption checklist

Do each step with the app connected (and one app window open where relevant). "UI" means the banner and the top-right pills.

| # | Do this | Expected |
|---|---|---|
| 1 | `pkill -f 'Contents/MacOS/adb'` (or `pkill adb` in dev) | Windows close with an error toast. UI: Reconnecting… then Connected within ~5 s (adb server restarts). |
| 2 | Phone: airplane mode for 60 s, then off | UI: Reconnecting… then "Can't reach …: … offline/timed out. Retrying in Ns" (backoff 1 → 30 s), then Connected after it's back. |
| 3 | Phone: switch NetBird off, then on | UI: Phone not reachable over NetBird ("isn't connected to NetBird…") → Connected. |
| 4 | Reboot the phone | UI: "pixel restarted… plug in USB once" (port 5555 refused). Plug in → Re-enable via USB → Connected. |
| 5 | Mac: turn Wi-Fi off for 60 s, then on | UI: Phone not reachable / Reconnecting (the NetBird pill shows a management warning on hover) → Connected. |
| 6 | Sleep the Mac 5 min, wake | Reconnects by itself (liveness check within ~15 s). |
| 7 | Open 3 app windows, then Quit from the menu bar | All windows close. `pgrep -fl PhoneBridge.app` prints nothing. `ps aux \| grep [a]db` shows no PhoneBridge adb. |
| 8 | Open 2 windows, then `kill -9` the PhoneBridge process | Windows may stay open. Relaunch PhoneBridge: the log says "cleaned up 2 scrcpy process(es)" and they're gone. |
| 9 | Settings → Log out of NetBird | Windows close. The login view returns. The peer is logged out in the NetBird dashboard; delete it there if it was added with a setup key. |
| 10 | Click the same app twice | Toast: "X is already open in a window". |
| 11 | Unplug and replug USB during "Set up via USB" | Clear error toast ("No phone found on USB" / "hasn't allowed this Mac yet") with a hint. |
| 12 | Settings → Log out, then "Sign in with browser" and close the browser tab without logging in | Login view stays on "Waiting for NetBird login…"; after ~5 minutes it shows "login cancelled or timed out" with a Sign in button. |
| 13 | Log in with a setup key that was revoked | Login view: "NetBird rejected the setup key (wrong, used up, revoked or expired)." |

## Packaging checks (Phase 3)

```bash
cargo xtask dist
```

`verify-bundle` (run by `dist`) checks:
- `phonebridge`, `adb` and `scrcpy` exist in `Contents/MacOS` and are executable
- `scrcpy-server`, icons and licences exist in `Contents/Resources`
- `lipo -archs` for each executable (arm64; adb is a universal binary)
- `codesign -dv` for each, showing an ad-hoc signature with the `runtime` flag
- `codesign --verify --deep --strict` on the app

`spctl` is expected to reject the app until it's notarized.

### Isolation test

1. Check for other copies:

   ```bash
   which -a adb scrcpy
   ```

   If Homebrew has them, hide them for the test:

   ```bash
   brew unlink scrcpy android-platform-tools
   ```

   (Undo with `brew link scrcpy` and `brew link android-platform-tools`.)
2. Quit the NetBird Mac app if it's installed (menu bar → Quit, and `netbird down` if you use the CLI), and check it's gone:

   ```bash
   pgrep -fl -i netbird
   ```

3. Launch `/Applications/PhoneBridge.app` (from Finder, not a terminal).
4. Settings → Diagnostics: the adb and scrcpy paths are inside `PhoneBridge.app/Contents/MacOS`, and "Other installs" is empty or "(not used)".
5. Open an app window, then:

   ```bash
   ps -axo pid,command | grep -E '[P]honeBridge.app'
   ```

   Expected: `phonebridge`, its `adb … fork-server server --reply-fd` (on the private port) and `scrcpy`, all from inside the bundle.

   ```bash
   lsof -nP -a -i -c scrcpy
   ```

   Expected: only 127.0.0.1 connections.
6. In the NetBird dashboard, `phonebridge-mac` is its own peer and is connected, with no NetBird app running on the Mac.
