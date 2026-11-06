# agent-ws desktop

The agent-ws workspaces of every repository in one window and in the menu bar.
See the [README](../README.md#desktop-app) for what it does; this page is for
working on it.

## Run and build

```sh
npm install
npm run tauri dev       # Vite on :1420 and the app, reloading on changes
npm run tauri build     # a release bundle in ../target/release/bundle
```

`AGENT_WS_HOME` works as for the CLI: start the binary with it set to look at
another state folder (handy for a demo or a test).

## Layout

| Path | Role |
| --- | --- |
| `index.html`, `src/main.ts`, `src/styles.css` | the window: plain TypeScript, no framework |
| `src/api.ts` | typed calls to the Rust side |
| `src-tauri/src/commands.rs` | the commands the window calls; each runs an agent-ws library call on a worker thread |
| `src-tauri/src/monitor.rs` | reads every workspace's last `exec` every 3 s: menu-bar count, notifications, `workspaces-changed` events |
| `src-tauri/src/tray.rs` | the menu-bar icon, drawn in code as a template image |
| `app-icon.png` | source of the app icons in `src-tauri/icons` (`npx tauri icon app-icon.png`) |

`src-tauri` is a member of the repository's Cargo workspace and depends on the
library by path, so a change to the library and to the app can go in one
commit. Plain `cargo build` and `cargo test` at the root build the CLI only;
`cargo build -p agent-ws-desktop` builds the app's Rust side.

The `@tauri-apps/api` minor version must match the `tauri` crate in
`../Cargo.lock` (2.11 for now). Cargo picks the newest `tauri` that builds with
the workspace's Rust 1.88, so it moves up only with that minimum.

## Releases

Tag `desktop-vX.Y.Z` (separate from the CLI's `vX.Y.Z`). The *Release desktop*
workflow builds a universal macOS `.dmg` and a Linux AppImage and `.deb`, and
attaches them to a release that does not replace the CLI's as the latest.
Started by hand, it only builds.
