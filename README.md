# PhoneBridge

Control your Android phone from your Mac, from anywhere. Open any phone app in its own Mac window, mirror the whole screen, and pin favourites to the menu bar. It works when the phone is on cellular, because PhoneBridge joins your **NetBird** or **Tailscale** network by itself. You choose which one, and can switch at any time.

- One self-contained `.app`. No Homebrew, no separately installed adb/scrcpy, and no NetBird or Tailscale app on the Mac.
- Mirroring and control are done by [scrcpy](https://github.com/Genymobile/scrcpy) 5.0.1, bundled. Each app runs on its own virtual display (Android 10+).
- Both networks' own clients are compiled into the app:
  - NetBird's [`client/embed`](https://github.com/netbirdio/netbird/tree/main/client/embed) v0.82.0. Works with NetBird Cloud or a self-hosted NetBird server.
  - Tailscale's [`tsnet`](https://tailscale.com/kb/1244/tsnet) v1.104.1. Works with Tailscale or a self-hosted [Headscale](https://headscale.net) server.

  Both run in userspace: no VPN, no system extension, no system DNS or route changes, no admin rights.

---

## One-time setup

### 1. Phone

1. **Developer options:** Settings → About phone → tap **Build number** 7 times.
2. **USB debugging:** Settings → System → Developer options → turn on **USB debugging**.
3. **NetBird or Tailscale:** install the app for the network you'll use, log in to the same network you'll use on the Mac, and keep it connected. In Android settings, set that app's battery usage to **Unrestricted** so it stays reachable in your pocket. Android runs one VPN app at a time, so the phone uses one network at a time.

### 2. Build the app on your Mac (once per update)

You need the Xcode Command Line Tools and Rust. Nothing else: Go is downloaded automatically into the build folder, used only to compile the NetBird and Tailscale clients, and never installed or shipped.

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

`dist` downloads and SHA-256-checks scrcpy and adb, builds the NetBird/Tailscale library and the app, then signs and checks the bundle. The result is:

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

### 4. Pick a network and log in

The login screen has a **NetBird | Tailscale** switch (also in Settings → Network). Pick the one your phone uses. On first launch PhoneBridge opens that network's login page in your browser and shows **"Waiting for … login…"**. Sign in with the same account as the phone. The Mac appears as **phonebridge-mac** in the NetBird dashboard or the Tailscale admin console.

Or paste a key instead:
- **NetBird setup key** (dashboard → Setup Keys). Peers added with a setup key never have to sign in again. Peers added by browser login follow your account's *peer login expiration* setting; when it expires, PhoneBridge shows "Your NetBird login expired. Sign in again."
- **Tailscale auth key** (admin console → Settings → Keys, `tskey-auth-…`). Tailscale device keys expire (180 days by default); to avoid signing in again, choose **Disable key expiry** for `phonebridge-mac` in the admin console.

**Self-hosted servers:** open "Self-hosted NetBird server" or "Headscale server" on the login screen and enter the URL (for example `https://netbird.example.com` or `https://headscale.example.com`). Clear the field to return to NetBird Cloud (`https://api.netbird.io:443`) or Tailscale (`https://controlplane.tailscale.com`).

Each network's login is saved separately in `~/Library/Application Support/io.cjscode.phonebridge/netbird` or `…/tailscale`. Switching networks keeps both, so you can switch back without signing in again. The phone is remembered separately for each network too, because it has a different address on each: the first time you use a network, run "Set up via USB" once.

> If your network uses access controls (NetBird policies other than the default "All", or a Tailscale ACL / grants policy), allow `phonebridge-mac` to reach the phone on **TCP 5555**. Devices you're not allowed to reach don't appear in the phone picker.

### 5. Plug in once and click "Set up via USB"

Connect the phone with a data cable, unlock it, tap **Allow** on "Allow USB debugging?" (tick "Always allow"), and click **Detect phone**. PhoneBridge reads the phone's IP addresses and pre-selects its NetBird or Tailscale device. Click **Enable and connect**, then unplug. From now on it connects over the network and reconnects automatically.

---

## After the phone reboots

Android turns wireless debugging (adb on port 5555) off on every reboot. PhoneBridge notices that the phone answers but port 5555 is closed, and shows **"Phone rebooted: plug in USB once"**. Plug it in, click **Re-enable via USB** (in the banner or the menu), and unplug. No need to pick the phone again.

## Using it

- **Apps:** click any app to open it in its own window. Several windows can be open at once. Star an app to pin it to **Favorites** (also in the menu bar).
- **Full mirror:** shows the whole phone screen, with audio (Android 11+).
- **Quality:** *Wi-Fi* (1920 px, 8 Mbit/s, 60 fps) or *Cellular* (1024 px, 1.5 Mbit/s, 30 fps). The menu bar item can switch between them.
- **Menu bar:** connection status, Favorites, Full Mirror, Reconnect, quality, Quit. Closing the main window keeps PhoneBridge running in the menu bar. Quit closes all phone windows, disconnects adb, and stops the network client.
- **Settings (gear icon):** the network switch, network info and **Log out**, phone actions, window options, and **Diagnostics**, which shows the exact adb/scrcpy paths in use, versions, and bytes forwarded.

| Status | Meaning | What to do |
|---|---|---|
| Connected (direct / relayed via …) | Working. "Relayed" means the network couldn't make a direct path (NetBird relay or Tailscale DERP), so expect more lag. | Use the Cellular preset if it stutters, or try the other network. |
| Reconnecting… | The link dropped; retrying with backoff (1 s → 30 s). | Wait, or click Retry now. |
| Phone rebooted: plug in USB once | Phone is reachable but adb isn't listening. | Re-enable via USB. |
| Phone unreachable | Phone offline, its NetBird/Tailscale app off, no signal, or asleep. | Open the app on the phone. |
| Waiting for NetBird/Tailscale / logged out | This Mac's device on that network isn't up. | Check internet, or sign in again. |
| Allow USB debugging on the phone | adb is waiting for your approval. | Unlock the phone and tap Allow. |

## How it works

```
scrcpy window ─┐                         ┌─ 127.0.0.1:<port>  forwarder (Rust)
scrcpy window ─┼─ private adb server ────┤        │ one adb transport (TCP)
full mirror  ──┘  (own port, bundled)    │        ▼
                                         └─ NetBird or Tailscale client (in-process, userspace WireGuard)
                                                  ▼
                                    phone (overlay IP):5555 adbd
```

adb can't use a userspace network, so PhoneBridge listens on `127.0.0.1:<port>`. For each connection it dials the phone through its own NetBird peer or Tailscale node and pipes bytes both ways; adb then runs `adb connect 127.0.0.1:<port>`. adb multiplexes everything over that one connection, including scrcpy's video, audio and control sockets (whether scrcpy uses `adb reverse` or `adb forward`). So the forwarder carries all traffic, and Diagnostics shows the byte counts.

Every adb/scrcpy process gets `ADB`, `SCRCPY_SERVER_PATH` and `SCRCPY_ICON_DIR` pointing inside the app bundle, `PATH=/usr/bin:/bin`, and a **private adb server port**. So PhoneBridge never uses, or interferes with, a Homebrew adb.

The embedded NetBird peer uses a random WireGuard port (so it can run next to an installed NetBird agent), connects to peers eagerly (so their status is meaningful), and ignores network routes and exit nodes (it only needs to reach the phone). The embedded Tailscale node is a normal `tsnet` node (its own machine in your tailnet, no exit node, no subnet routes) with log uploads to Tailscale turned off. NetBird and Tailscale share one Go runtime, so they're built into one library (`crates/overlay-sys`); only the selected one runs.

## Security and privacy notes

- `adb tcpip 5555` makes the phone listen on all its networks, including Wi-Fi. Connections still need the RSA key you approved ("Allow USB debugging?"), but restrict TCP 5555 to `phonebridge-mac` with a NetBird access policy or Tailscale ACL if you can. Avoid untrusted Wi-Fi while it's enabled (a reboot turns it off).
- Traffic between the Mac and the phone is end-to-end encrypted by WireGuard (both networks). When no direct path exists it goes through a NetBird relay or Tailscale DERP server, still encrypted end to end.
- Login tokens are only used to register the Mac; afterwards the app keeps just the device's keys (in the folders above, readable only by you).

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

The `probe` tool tests each layer on its own (network, then adb, then scrcpy; `--network netbird|tailscale`); see [docs/TESTING.md](docs/TESTING.md). Design notes and research are in [docs/PLAN.md](docs/PLAN.md).

Layout: `crates/fetch` (pinned downloads), `crates/overlay-sys` (Go wrapper around NetBird's and Tailscale's embedded clients, FFI and build), `crates/pb-core` (network manager, forwarder, adb, scrcpy, connection manager), `src-tauri` (app), `ui` (plain HTML/CSS/JS, no build step), `xtask`. Third-party versions and SHA-256s are in [deps.toml](deps.toml); Go modules are pinned by `crates/overlay-sys/go/go.sum`.

## Licences

PhoneBridge's own code is MIT. Bundled components keep their licences (scrcpy Apache-2.0; adb Apache-2.0 with the platform-tools NOTICE; NetBird client and Tailscale BSD-3-Clause; Go modules as listed). See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md); full texts ship in `PhoneBridge.app/Contents/Resources/licenses`. PhoneBridge isn't affiliated with NetBird, Tailscale, Google, Genymobile or Microsoft.
