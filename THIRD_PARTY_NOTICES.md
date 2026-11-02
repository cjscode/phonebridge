# Third-party software in PhoneBridge

Full licence texts are bundled in `PhoneBridge.app/Contents/Resources/licenses/`.

| Component | Version | Licence | Where it is | Source |
|---|---|---|---|---|
| scrcpy (client binary, unmodified official macOS build) | 5.0.1 | Apache-2.0 | `Contents/MacOS/scrcpy` | https://github.com/Genymobile/scrcpy |
| scrcpy-server | 5.0.1 | Apache-2.0 | `Contents/Resources/scrcpy-server` | same |
| adb (Android SDK platform-tools, unmodified) | 37.0.1 | Apache-2.0 + notices in `platform-tools-NOTICE.txt` | `Contents/MacOS/adb` | https://android.googlesource.com/platform/packages/modules/adb |
| NetBird client (`client/embed` and the client/shared packages it uses) | v0.82.0 | BSD-3-Clause | linked into `Contents/MacOS/phonebridge` | https://github.com/netbirdio/netbird |
| Go modules used by the NetBird client | see `licenses/go/MODULES.txt` | BSD / MIT / Apache-2.0 / ISC, three MPL-2.0 (per module) | linked into `phonebridge` | listed in `MODULES.txt` |
| Go standard library and runtime | 1.27.3 | BSD-3-Clause | linked into `phonebridge` | https://go.dev |
| Tauri and Rust crates | see `Cargo.lock` | MIT / Apache-2.0 (per crate) | `phonebridge` | https://crates.io |

## Libraries inside the scrcpy binary

The official scrcpy macOS build statically includes:

- FFmpeg 9.0.2 (LGPL-2.1-or-later; built without GPL components)
- SDL 3.4.18 (zlib)
- dav1d 1.5.4 (BSD-2-Clause)
- libusb 1.0.30 (LGPL-2.1-or-later)

PhoneBridge redistributes that binary unmodified. The exact sources and build scripts are in the scrcpy repository at tag `v5.0.1` (`app/deps/` and `release/build_macos.sh`), which lets you rebuild and relink it as the LGPL requires.

## NetBird and copyleft notes

- NetBird's repository is BSD-3-Clause except its server directories (`management/`, `signal/`, `relay/`, `combined/`), which are AGPL-3.0. PhoneBridge links only client code: none of the 157 NetBird packages in the macOS build comes from those directories (checked with `go list -deps`).
- Three Go modules are MPL-2.0: `github.com/hashicorp/errwrap`, `github.com/hashicorp/go-multierror` and `github.com/hashicorp/go-version`. They are used unmodified; their source is available at the versions listed in `MODULES.txt` (and in `crates/netbird-sys/go/go.sum`).

## Notes

- adb is downloaded from Google's platform-tools package. That download is covered by the Android SDK License Agreement, which limits redistribution of the SDK except where third-party (open-source) licences require otherwise. adb itself is Apache-2.0 open source. Redistributing it in a personal build is common practice (scrcpy and Homebrew do the same), but check this before distributing PhoneBridge publicly. The cleanest alternative is building adb from AOSP source.
- "NetBird" is a trademark of NetBird GmbH. "Android" is a trademark of Google LLC. PhoneBridge is not affiliated with or endorsed by them.
