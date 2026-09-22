<div align="center">

<h1>Discord Quest Helper</h1>

<h2>Local safe build changes</h2>

<p>This local fork is intentionally hardened for review and personal use:</p>

<ul>
  <li>Local Discord profile scanning and DPAPI/Keychain/Secret Service token extraction are removed.</li>
  <li>Manual Discord-token login is disabled.</li>
  <li>The UI exposes CDP login only; the running Discord client must be selected explicitly.</li>
  <li>The remote-JavaScript fallback for SuperProperties is disabled.</li>
  <li>Stealth relaunch, temporary random executable copies, PE identity rewriting, and Mark-of-the-Web removal are removed.</li>
  <li>Windows CDP desktop-shortcut creation is disabled; the safe build does not execute PowerShell.</li>
</ul>

<p>The application still handles a live Discord session in memory and automates Quest requests. It is not a zero-trust application and may violate Discord's Terms of Service.</p>

<p align="center">
  <img src="src-tauri/icons/icon.png" alt="Discord Quest Helper logo" width="150">
</p>

<p><strong>🎮 Automate your Discord Quests with one click</strong></p>

<p>Complete Discord video, stream, and game quests automatically while you focus on what matters.</p>

<p>⭐ <strong>If you find this helpful, please give it a star!</strong> ⭐</p>

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue.svg)](https://github.com/Masterain98/discord-quest-helper/releases)
[![Tauri](https://img.shields.io/badge/tauri-2-blue.svg)](https://tauri.app/)
[![Vue](https://img.shields.io/badge/vue-3.5-green.svg)](https://vuejs.org/)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub Release](https://img.shields.io/github/v/release/Masterain98/discord-quest-helper?label=latest%20release&color=41b883)](https://github.com/Masterain98/discord-quest-helper/releases/latest)

<br>

<img src="public/certificated-ai-sloop-tiny.png" alt="Certificated AI Slop" width="480">

</div>

## 🚀 Quick Start

> [!WARNING]
> **This tool is for educational purposes only.** Using this tool may violate Discord's Terms of Service. The authors are not responsible for any consequences resulting from the use of this software. Use at your own risk.

### Download & Run

Download the latest build from [GitHub Releases](https://github.com/Masterain98/discord-quest-helper/releases/latest).

| Platform | Release file | Instructions |
| --- | --- | --- |
| Windows x64 Installer | `discord-quest-helper-Windows-x64-<version>-setup.msi` | Open the MSI installer. |
| Windows x64 Portable | `discord-quest-helper-Windows-x64-<version>-portable.zip` | Extract the ZIP and run `discord-quest-helper.exe`. |
| macOS Apple Silicon Installer | `discord-quest-helper-MacOS-arm64-<version>.dmg` | Open the DMG and drag the app to Applications. If macOS blocks it, run the quarantine-removal command below. |
| Linux x86_64 Installer | `discord-quest-helper-Linux-x86_64-<version>.deb` | Install the Debian package with the command below. |
| Linux x86_64 Portable | `discord-quest-helper-Linux-x86_64-<version>.AppImage` | Make the AppImage executable and run it with the commands below. |

On macOS, remove the quarantine attribute if needed:

```bash
xattr -cr "/Applications/Discord Quest Helper.app"
```

On Linux, install the Debian package like this:

```bash
sudo apt install ./discord-quest-helper-Linux-x86_64-<version>.deb
```

Or run the portable AppImage like this:

```bash
chmod +x discord-quest-helper-Linux-x86_64-<version>.AppImage
./discord-quest-helper-Linux-x86_64-<version>.AppImage
```

> [!NOTE]
> Release binaries are built and published by GitHub Actions from the repository source. Linux release packages target x86_64; macOS releases currently target Apple Silicon.

### Sign in

This safe build supports **CDP login only**. There is no Auto Detect Token, no local Discord profile scanning, and no manual token input.

1. Start the Discord desktop client or Vesktop with CDP enabled. The app can launch or restart the selected client for you from **Settings → Discord Client Integration**.
2. In the app, choose the client and click **Log in with Discord Client**.
3. The backend captures the live Discord session over CDP, validates it, and keeps it in backend memory for the current app session. The raw token is never returned to the frontend and is not persisted to disk.

> [!TIP]
> Vesktop is supported through CDP only; it is not scanned as a local token source. You can select a detected installation or add a custom/portable `vesktop.exe` in Settings.

### Complete Quests

- **Video/Stream:** Click **Start Quest** on an incomplete quest.
- **Game:** Open **Game Simulator**, select a game, then create and run a simulated game.

## ✨ Features

- 🔐 **CDP-Only Login** — Connect through the live Discord client or Vesktop over the Chrome DevTools Protocol; no local profile scanning and no manual token entry.
- 🖥️ **Discord & Vesktop Support** — Select the desktop client or installation used for CDP login, including custom paths.
- 🐧 **Linux Desktop Support** — Available as an x86_64 AppImage or Debian package.
- 🎮 **Zero-Download Game Simulation** — Complete game quests without downloading or installing the actual game.
- 📺 **Video & Stream Automation** — Start once and let quest progress update in the background.
- 🔍 **Advanced Quest Filters** — Filter by reward type, completion status, and more.
- 🌏 **Multi-language** — English, Simplified Chinese, Traditional Chinese, Japanese, Korean, Russian, Spanish, German, French, Indonesian, Polish, Portuguese, Thai, Turkish, and Vietnamese.

## 📸 Screenshots

| Login | Home |
|:-----:|:----:|
| ![Login](https://github.com/user-attachments/assets/a67369e9-7bd5-46ca-afdc-f16e54f64824) | ![Home](https://github.com/user-attachments/assets/bde65569-c4e0-4d0e-971a-a28ab9f38468) |

| Game Simulator |
|:--------------:|
| ![Game Simulator](https://github.com/user-attachments/assets/d1f9d481-39f6-4bef-8b4a-9bf90c9ad4e3) |

| Quest Progress | Settings |
|:--------------:|:--------:|
| ![Quest Progress](https://github.com/user-attachments/assets/7a1d6c82-63a6-4595-a060-cefae05676e5) | ![Settings](https://github.com/user-attachments/assets/1261a2f5-8b99-4c55-ab7e-18e7b9617e8e) |

## 🏗️ Architecture

```text
Discord Quest Helper
├─ Vue 3 + Vite frontend
│  ├─ Views: Home, Game Simulator, Settings, Debug
│  ├─ Pinia stores and composables for auth, quests, settings, and UI state
│  └─ src/api/tauri.ts — typed Tauri IPC client
│
├─ Tauri 2 Rust application
│  ├─ Discord API and Gateway integration
│  ├─ CDP client and quest execution for video, stream, activity, and game quests
│  ├─ Official Discord and Vesktop providers for discovery, launch, and process supervision
│  ├─ Platform capability detection
│  ├─ Game simulation and manual CDP game sessions
│  └─ Runtime identity auditing and platform runtime bridge management
│
├─ Workspace crates
│  ├─ discord-cdp-launch-core — cross-platform client discovery and launch core
│  ├─ src-cdp-launcher — optional Discord/Vesktop CDP launcher sidecar
│  └─ src-runner — minimal game-process runner sidecar
│
└─ Discord services
   ├─ REST API — quests, accounts, rewards, and profile data
   ├─ Gateway — account and activity events
   └─ Discord/Vesktop CDP targets — browser automation and session capture
```

The frontend communicates with the Rust backend through Tauri IPC. The backend owns Discord networking, CDP sessions, quest execution, process cleanup, and platform-specific integration.

Explore the codebase with [![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/Masterain98/discord-quest-helper)

## 🔒 Security

- **Tokens are kept in memory by the helper** — The app does not intentionally persist your Discord token to disk.
- **CDP-only login in this fork** — The safe build does not scan local Discord profiles or read platform credential stores.
- **HTTPS for Discord API requests** — Network requests use secure HTTPS connections.
- **Sanitized diagnostics** — Logs and debug exports redact sensitive tokens and account data where applicable.

## 🤝 Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for:

- Development setup
- Project structure
- Code conventions
- Pull request guidelines

## 📄 License

MIT License — see the [LICENSE](LICENSE) file.


## 🙏 Credits

**Inspiration & Resources**
- [markterence/discord-quest-completer](https://github.com/markterence/discord-quest-completer)
- [power0matin/discord-quest-auto-completer](https://github.com/power0matin/discord-quest-auto-completer)
- [taisrisk/Discord-Quest-Helper](https://github.com/taisrisk/Discord-Quest-Helper)
- [aamiaa/CompleteDiscordQuest.md](https://gist.github.com/aamiaa/204cd9d42013ded9faf646fae7f89fbb)
- [docs.discord.food](https://docs.discord.food/)

**Technologies**
- [Tauri](https://tauri.app/) • [Vue.js](https://vuejs.org/) • [Pinia](https://pinia.vuejs.org/) • [vue-i18n](https://vue-i18n.intlify.dev/) • [shadcn-vue](https://www.shadcn-vue.com/) • [TailwindCSS](https://tailwindcss.com/) • [Lucide Icons](https://lucide.dev/)
