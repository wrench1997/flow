# Flow

**A Windows BitTorrent download manager built with Rust, egui, and librqbit.**

English · [简体中文](README.zh-CN.md)

<img src="assets/flow-icon.png" alt="Flow icon" width="112" />

One executable starts both the desktop interface and embedded download engine. No Python installation or separate backend service is required.

## Features

- Magnet links and local `.torrent` files, with native file and folder pickers.
- Compact task list with search, status filters, file progress, peer details, and diagnostic events.
- Pause/resume, verify existing files, and remove tasks while keeping downloaded files.
- Right-click to copy magnet links, original sources, names, or save paths, and open download folders.
- Double-click a task name or press **Space** to pause/resume. **Delete** opens a removal confirmation.
- Persistent default directory, global speed limits, and connection limit settings.
- Editable Tracker subscriptions, ordered mirrors, local caching, retry backoff, and periodic health checks.
- Resource-specific Tracker statistics and scoring, DHT discovery with persisted routing state, and engine session persistence.
- Light/dark appearance and an embedded multi-resolution application icon.

## Build and run

Supported desktop target: **Windows x64 (MSVC)**. Building requires Rust/Cargo and Visual Studio C++ build tools with the Windows SDK.

```powershell
git clone https://github.com/wrench1997/flow.git
cd flow
.\build-portable.ps1
.\Flow.exe
```

The script creates `Flow.exe` in the project root. Double-click it for normal use. `start.cmd` / `start.ps1` are developer shortcuts that build and launch the app.

```powershell
cargo test --release --locked
cargo fmt --check
```

The repository includes source and application assets only. Executables, downloads, local task state, caches, and temporary files are excluded. The Windows build statically links the C/C++ runtime; users of the built executable do not need Python, Rust, or a separate Visual C++ runtime installation.

## Settings and behavior

Open **Settings** to choose the default download folder and speed limits. `0` means unlimited. Speed limits apply immediately; the connection limit applies after restarting. The default folder affects new tasks only and does not move existing files.

The default upload limit is **512 KiB/s**. Finished tasks continue seeding until paused. Closing the window saves state and stops the engine. Removing a task preserves downloaded files.

To reuse another client's files, add the matching torrent and choose the original save directory. Flow verifies existing data before continuing. Verification progress is distinguished from download progress; another engine's resume file cannot replace verification.

## Tracker resilience

Flow preserves existing torrent Trackers and subscribes to public lists from [XIU2](https://github.com/XIU2/TrackersListCollection) and [ngosang](https://github.com/ngosang/trackerslist). These projects publish address lists; individual Tracker servers are independently operated.

In **Tracker → Subscription settings**, add, edit, disable, or remove sources. Each source supports up to five HTTP/HTTPS mirrors, tried in order. If all mirrors fail or return invalid/empty data, Flow retains the last successful cache.

- Default list refresh: **24 hours**; health checks for unpaused tasks: **30 minutes**, both configurable.
- Failed queries use exponential retry backoff, capped at **6 hours**.
- Up to **200 Trackers per task**, **12 concurrent queries** across tasks, and an **8-second** request timeout.
- HTTP/UDP scrape queries use the current resource hash; scores combine query history, reported seed count, and response time.
- Private torrents are excluded from public Tracker discovery.

**Background checks update candidates and scores without restarting downloads.** New candidates require **Apply candidates**, which currently reloads the task and verifies existing files.

A scrape failure does not prove a Tracker cannot return peers. Reported seed counts are not connected peers, and higher scores do not guarantee faster downloads. DHT offers another discovery path for public torrents, but cannot recover missing content without reachable peers holding it.

## Local data and portability

Runtime data lives in `data/` beside the executable:

| Path | Purpose |
| --- | --- |
| `settings.json` | Download directory and transfer settings |
| `tasks-rust.json` | Task catalog and Tracker history |
| `tracker-sources.json` / `tracker-cache.json` | Subscription settings, cached lists, and retry state |
| `dht.json` | DHT routing state |
| `rqbit/` | Engine sessions and fast-resume state |
| `status.json` / `events-rust.jsonl` | Local status and diagnostics |

To move an installation, copy the executable and `data/` together and keep the referenced download paths available. The local API uses a random loopback port and a fresh Bearer token per launch. A directory lock prevents concurrent engine instances from using the same state directory.

## Project structure

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | Desktop interface and interactions |
| `src/backend.rs` | Embedded backend lifecycle |
| `src/engine.rs` | Download sessions, tasks, persistence, and local API |
| `src/trackers.rs` | Scrape queries and scoring |
| `src/subscriptions.rs` | Sources, mirrors, cache, and retry policy |
| `src/settings.rs` | Transfer settings and validation |
| `assets/`, `build.rs` | Icons and Windows resource embedding |

## Current limitations

Flow is an early desktop implementation. HTTP direct downloads, per-file download selection, speed charts, automatic application updates, and persistent theme selection are not implemented. Applying new Tracker candidates requires verification. Statistics that librqbit does not reliably expose, such as connected complete seeders and distributed availability, are shown as unknown.

Built with [egui](https://github.com/emilk/egui) and [librqbit](https://github.com/ikatson/rqbit).
