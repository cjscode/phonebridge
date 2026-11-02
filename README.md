# PhoneBridge

Control your Android phone from your Mac, from anywhere. Open any phone app in its own Mac window, mirror the whole screen, and pin favourites to the menu bar. It works when the phone is on cellular, because PhoneBridge joins your **Tailscale** network by itself.

- One self-contained `.app`. No Homebrew, no separately installed adb/scrcpy, and no Tailscale app on the Mac.
- Mirroring and control are done by [scrcpy](https://github.com/Genymobile/scrcpy) 5.0.1, bundled. Each app runs on its own virtual display (Android 10+).
- The Mac side of Tailscale is [libtailscale](https://github.com/tailscale/libtailscale) (tsnet) compiled into the app. It runs in userspace: no VPN, no system extension, no admin rights.

---

## One-time setup

### 1. Phone

1. **Developer options:** Settings → About phone → tap **Build number** 7 times.
2. **USB debugging:** Settings → System → Developer options → turn on **USB debugging**.
3. **Tailscale:** install the Tailscale app, log in to the same tailnet you'll use on the Mac, and keep it connected. In Android settings, set Tailscale's battery usage to **Unrestricted**, so it stays reachable in your pocket.

### 2. Build the app on your Mac (once per update)

You need the Xcode Command Line Tools and Rust. Nothing else: Go is downloaded automatically into the build folder, used only to compile libtailscale, and never installed or shipped.

```bash
xcode-select --install
```

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

```bash
cargo install tauri-cli --version 2.12.4 --locked
```

Then, in this folder:

```bash
cargo xtask doctor
```

```bash
cargo xtask dist
```

`dist` downloads and SHA-256-checks scrcpy and adb, builds libtailscale and the app, then signs and checks the bundle. The result is:

- `target/aarch64-apple-darwin/release/bundle/macos/PhoneBridge.app`
- `target/aarch64-apple-darwin/release/bundle/dmg/PhoneBridge_0.1.0_aarch64.dmg`

Drag `PhoneBridge.app` to Applications. The first build takes a while (Go toolchain and modules); later builds are cached.

### 3. Open it the first time (Gatekeeper)

The app is ad-hoc signed, not notarized.

- **Built on this Mac:** it opens normally (no quarantine flag).
- **Copied from somewhere else** (download, AirDrop, the .dmg from another Mac): macOS blocks it. Double-click it once, then go to **System Settings → Privacy & Security**, scroll down, click **Open Anyway**, and confirm. You only do this once. Alternatively:

  ```bash
  xattr -dr com.apple.quarantine /Applications/PhoneBridge.app
  ```

### 4. Log in to Tailscale

On first launch PhoneBridge opens the Tailscale login page in your browser and shows **"Waiting for Tailscale login…"**. Sign in and approve the device. It appears in your admin console as **phonebridge-mac**. You can paste an auth key instead (Settings → Keys in the Tailscale admin console). The login is saved in `~/Library/Application Support/io.cjscode.phonebridge/tailscale`, so you stay logged in.

> If your tailnet uses custom ACLs or grants, allow `phonebridge-mac` to reach the phone on **TCP 5555**.

### 5. Plug in once and click "Set up via USB"

Connect the phone with a data cable, unlock it, tap **Allow** on "Allow USB debugging?" (tick "Always allow"), and click **Detect phone**. PhoneBridge finds the phone's tailnet address and pre-selects it. Click **Enable and connect**, then unplug. From now on it connects over the tailnet and reconnects automatically.

---

## After the phone reboots

Android turns wireless debugging (adb on port 5555) off on every reboot. PhoneBridge notices that the phone answers but port 5555 is closed, and shows **"Phone rebooted: plug in USB once"**. Plug it in, click **Re-enable via USB** (in the banner or the menu), and unplug. No need to pick the phone again.

## Using it

- **Apps:** click any app to open it in its own window. Several windows can be open at once. Star an app to pin it to **Favorites** (also in the menu bar).
- **Full mirror:** shows the whole phone screen, with audio (Android 11+).
- **Quality:** *Wi-Fi* (1920 px, 8 Mbit/s, 60 fps) or *Cellular* (1024 px, 1.5 Mbit/s, 30 fps). The menu bar item can switch between them.
- **Menu bar:** connection status, Favorites, Full Mirror, Reconnect, quality, Quit. Closing the main window keeps PhoneBridge running in the menu bar. Quit closes all phone windows, disconnects adb, and stops the tailnet node.
- **Settings (gear icon):** Tailscale info and **Log out**, phone actions, window options, and **Diagnostics**, which shows the exact adb/scrcpy paths in use, versions, and bytes forwarded.

| Status | Meaning | What to do |
|---|---|---|
| Connected (direct / relayed via …) | Working. "Relayed" means Tailscale couldn't make a direct path, so expect more lag. | Use the Cellular preset if it stutters. |
| Reconnecting… | The link dropped; retrying with backoff (1 s → 30 s). | Wait, or click Retry now. |
| Phone rebooted: plug in USB once | Phone is on the tailnet but adb isn't listening. | Re-enable via USB. |
| Phone not reachable on tailnet | Phone offline, Tailscale off, no signal, or asleep. | Open Tailscale on the phone. |
| Waiting for tailnet / logged out | This Mac's node isn't online. | Check internet, or log in again. |
| Allow USB debugging on the phone | adb is waiting for your approval. | Unlock the phone and tap Allow. |

## How it works

```
scrcpy window ─┐                         ┌─ 127.0.0.1:<port>  forwarder (Rust)
scrcpy window ─┼─ private adb server ────┤        │ one adb transport (TCP)
full mirror  ──┘  (own port, bundled)    │        ▼
                                         └─ libtailscale (in-process, userspace WireGuard)
                                                  ▼
                                    phone (tailnet IP):5555 adbd
```

adb can't use a userspace network, so PhoneBridge listens on `127.0.0.1:<port>`. For each connection it dials the phone through its own tailnet node and pipes bytes both ways; adb then runs `adb connect 127.0.0.1:<port>`. adb multiplexes everything over that one connection, including scrcpy's video, audio and control sockets (whether scrcpy uses `adb reverse` or `adb forward`). So the forwarder carries all traffic, and Diagnostics shows the byte counts.

Every adb/scrcpy process gets `ADB`, `SCRCPY_SERVER_PATH` and `SCRCPY_ICON_DIR` pointing inside the app bundle, `PATH=/usr/bin:/bin`, and a **private adb server port**. So PhoneBridge never uses, or interferes with, a Homebrew adb.

## Security and privacy notes

- `adb tcpip 5555` makes the phone listen on all its networks, including Wi-Fi. Connections still need the RSA key you approved ("Allow USB debugging?"), but restrict TCP 5555 in your tailnet ACLs to `phonebridge-mac` if you can. Avoid untrusted Wi-Fi while it's enabled (a reboot turns it off).
- Traffic between the Mac and the phone is end-to-end encrypted by WireGuard (Tailscale).
- Like the official Tailscale clients, the embedded node sends diagnostic logs to Tailscale (`log.tailscale.com`).

## Notarizing later

To get rid of the Gatekeeper step for other people:

1. Join the Apple Developer Program ($99/year) and create a **Developer ID Application** certificate.
2. In `src-tauri/tauri.conf.json`, set `bundle.macOS.signingIdentity` to that certificate's name (e.g. `"Developer ID Application: Your Name (TEAMID)"`).
3. Export App Store Connect API credentials (`APPLE_API_ISSUER`, `APPLE_API_KEY`, `APPLE_API_KEY_PATH`), or `APPLE_ID` + `APPLE_PASSWORD` (app-specific) + `APPLE_TEAM_ID`.
4. Run `cargo xtask dist`. Tauri signs adb and scrcpy and the app with hardened runtime and a timestamp, submits to Apple, and staples the ticket. No extra entitlements are expected.

## Development

```bash
cargo test --workspace
```

```bash
cargo xtask fetch
```

```bash
cd src-tauri && cargo tauri dev
```

The `probe` tool tests each layer on its own (tailnet, then adb, then scrcpy); see [docs/TESTING.md](docs/TESTING.md). Design notes and research are in [docs/PLAN.md](docs/PLAN.md).

Layout: `crates/fetch` (pinned downloads), `crates/libtailscale-sys` (FFI and Go build), `crates/pb-core` (tailnet, forwarder, adb, scrcpy, connection manager), `src-tauri` (app), `ui` (plain HTML/CSS/JS, no build step), `xtask`. All third-party versions and SHA-256s are in [deps.toml](deps.toml).

## Licences

PhoneBridge's own code is MIT. Bundled components keep their licences (scrcpy Apache-2.0; adb Apache-2.0 with the platform-tools NOTICE; libtailscale/Tailscale BSD-3-Clause; Go modules as listed). See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md); full texts ship in `PhoneBridge.app/Contents/Resources/licenses`. PhoneBridge isn't affiliated with Tailscale, Google, Genymobile or Microsoft.
