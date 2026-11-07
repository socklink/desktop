# SockLink Desktop

A modern, high-performance desktop client for the **Android ↔ Desktop SockLink** ecosystem, built with **Tauri v2**, **React 19**, **TypeScript**, and **TanStack Router**. 

SockLink connects your computer and Android device seamlessly over the local network to enable instant sharing, remote system integration, and universal workspace synchronization.

---

## Key Features

- **Local P2P Discovery & Pairing**: Automatically discovers and securely pairs with Android devices over local Wi-Fi using UDP broadcasting (port `8200`) and TCP listeners (port `8080`).
- **Universal Clipboard**: Automatically synchronizes clipboard text and copied files between your desktop and Android device in real time.
- **Seamless File Transfers**: Send and receive files directly to/from the system's `Downloads` folder, featuring a visual transfer progress ring in the system tray.
- **Media & Volume Control**: Check currently playing media and control playback or synchronize master volume levels from your mobile device.
- **Secure Terminal Access**: Run secure remote terminal commands directly from paired mobile devices.
- **Autostart & System Tray Integration**: Run silently in the background with a native tray menu, or configure the app to run minimized at startup.

---

## Technology Stack

The desktop app is designed with a lightweight, multi-process architecture:

- **Frontend**: [React 19](https://react.dev/), [Vite](https://vite.dev/), [TanStack Router](https://tanstack.com/router) (for routing and state management), and vanilla CSS with glassmorphism aesthetics.
- **Backend**: [Rust](https://www.rust-lang.org/) for secure, native OS integration, custom networking modules, system tray control, and file processing.
- **Tauri Framework**: Powered by the official [Tauri Framework](https://tauri.app/) (v2) to package the web frontend into a lightweight desktop binary.

---

## Project Structure

```text
desktop/
├── src/                      # React Frontend Source
│   ├── assets/               # Static assets & SVG icons
│   ├── components/           # Reusable React components
│   │   ├── ui/               # Atomic elements (Buttons, Cards, Toggles)
│   │   └── layout/           # Global templates (Modals, Shells)
│   ├── hooks/                # Customs hooks encapsulating Tauri IPC/state logic
│   ├── routes/               # Page routes mapped by TanStack Router
│   ├── utils/                # Helper utilities and type models
│   └── index.css             # Main styling system, design tokens & glassmorphism theme
├── src-tauri/                # Tauri Rust Backend Source
│   ├── src/
│   │   ├── app.rs            # Service setup, background thread spawners & event loop
│   │   ├── commands/         # IPC handlers called by the React frontend
│   │   ├── network/          # TCP/UDP networking protocols and pairing session logic
│   │   ├── system/           # OS/hardware bindings (clipboard polling, terminal, etc.)
│   │   └── main.rs           # Cargo entry point
│   ├── Cargo.toml            # Rust backend dependencies
│   └── tauri.conf.json       # App manifests, bundle configurations, and window options
└── package.json              # Frontend dependency definition
```

---

## Getting Started

### Prerequisites

1. **Rust Toolchain**: Ensure you have `rustup`, `rustc`, and `cargo` installed. Follow the [Tauri Prerequisites Guide](https://tauri.app/start/prerequisites/) for your platform.
2. **Node.js**: Node 18+ is recommended.
3. **Linux System Dependencies** (if building on Linux):
   - `scrot`
   - `xdotool`
   - `wmctrl`
   - `gnome-screenshot` or `flameshot`

### Development Setup

1. **Install Frontend Dependencies**:
   ```bash
   npm install
   ```

2. **Run in Development Mode**:
   Spawns the Vite development server alongside the compiled Tauri desktop app window, complete with Hot Module Replacement (HMR) and cargo live-reloading:
   ```bash
   npm run tauri dev
   ```

3. **Rust Backend Validation**:
   Validate code correctness or run rust-specific tests under `src-tauri`:
   ```bash
   cd src-tauri
   cargo check
   cargo test
   ```

### Production Build

Build the production-ready installer and optimized executable bundle:
```bash
npm run build
```

---

## Learn More

To dive deeper into the Tauri framework, configure custom window hooks, or understand IPC events:
- Visit the official [Tauri Website](https://tauri.app/).
- Read the [Tauri v2 Documentation](https://v2.tauri.app/).
