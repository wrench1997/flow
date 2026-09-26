# Flow

**A Windows BitTorrent and HTTP(S) download manager built with Rust, egui, and librqbit.**

English · [简体中文](README.zh-CN.md)

<img src="assets/flow-icon.png" alt="Flow icon" width="112" />

One executable starts both the desktop interface and embedded download engine. No Python installation or separate backend service is required.

## What is new in v0.4.2

- Peer records and manual IP blacklist management persist locally. Reference scores use observed sustained/intermittent/idle transfer (90/60/10); new peers remain unscored. Blocking and unblocking require restarting Flow to update the native incoming/outgoing connection filter. Shared IPs affect all BT tasks; there is no automatic malicious-client classification. Records include the latest observed task, client, cumulative session transfers, errors and policy reason.

- Remove the composite Tracker score from the UI. Peer rows and reconnect-cache candidates prioritize sustained received data: three positive 10-second samples, then intermittent transfer, new observations, and idle peers. Recent receive rates and cumulative uploads/downloads are visible. This does not override engine bandwidth scheduling or automatically ban clients. Connection diagnostics now include failed/disconnected peers, instead of only live connections.

- Long links scroll inside a bounded input area; the import dialog is constrained to the available window height.

- Drag one or more `.torrent` files into Flow to queue import dialogs. Local torrents are parsed before adding; select the files to download and see their combined size. Empty selections cannot start.
- Settings includes a `.torrent` association button. Double-clicking opens the same import dialog, including when Flow is already running. If Windows has an explicit default, select Flow in Open with → Always.
- Magnets can be added paused, then configured in the Files tab once metadata arrives. Shared pieces can write some data to adjacent unselected files.

## What is new in v0.4.1

- First playback automatically downloads, verifies and installs the pinned mpv runtime. Download progress and retry are built into Flow; playback resumes after setup. No manual PowerShell script is needed. Existing installations are reused.

## What is new in v0.4.0

- Five built-in Tracker subscriptions: XIU2, ngosang, newTrackon, animeTrackerList, and OpenTracker. Existing installations receive the three additions once; disabled/custom sources are preserved. Sources refresh concurrently with a 12-second budget per source, mirror fallback, deduplication and last-good cache retention.
- Completed tasks are excluded from the Paused category; Paused means an unfinished download that has been stopped.

- Drag with the left mouse button in the task list to select intersecting rows with a translucent blue marquee; Ctrl-drag adds to the selection. Ctrl-click task names to toggle multiple selections. Right-click removal, the toolbar and Delete share a batch confirmation dialog, keep files by default and report per-task failures.

- Magnet metadata resolution reuses cached peers and adds subscribed sources before resolving tracker-less magnets, with two bounded attempts and stage diagnostics. Explicit tracker sets are preserved. Startup resolves at most two pending magnets concurrently.
- Cache metadata-discovery candidates, replacing them every 30 seconds with peers that actually transferred data, ranked by bytes received; retain up to 64 peers for seven days and exclude private torrents. Diagnostics separate metadata acquisition, no connection attempts, failed connections and connected-but-idle peers without inventing piece-availability information.
- Tracker scores are labeled health scores, with reported seed counts weighted at only 5%. Per-tracker transfer attribution is unavailable, so these are not download-speed rankings.

- While Flow is running (including in the tray), copying a magnet or a sharing URL containing `magnet:?` opens a confirmation dialog. Clipboard detection is enabled by default and can be disabled in Settings. Existing clipboard contents are ignored at startup. An independent Windows clipboard-change monitor recognizes each new copy, including copying the same link again; no sharing webpage is fetched and no download starts without confirmation.

- Stopped torrents with a complete persisted piece bitmap restore as saved completed tasks without opening their payload files or creating download sessions. File lists and local playback remain available; Verify explicitly reloads the task. Saved completion is not a fresh disk integrity check.

- Peer queries use existing task handles instead of waiting for the session lock during file opening. A separate health check distinguishes failed refreshes from an unresponsive engine. **Exit application** in the main toolbar stops background downloads and exits.

- The window opens before engine initialization and displays the current stage, errors, and a retry button. Closing while disconnected exits instead of hiding to the tray.
- Flow restores tasks in the background after opening its local API. The derived engine index is backed up before rebuilding; downloaded files and resume bitmaps are retained.
- Cached torrents with fewer files restore first, before large file collections and unresolved magnets. During restoration, the UI shows known size, the current stage and elapsed time; progress is marked as pending until resume state is available.
- Initialization can be cancelled, and shutdown bounds the wait for active media streams. Diagnostic stages are saved in `data/startup.log`.
- Run `register-defaults.ps1` to register this copy of Flow for magnet links and `.torrent` files for the current Windows user. Windows may still require selecting Flow in Default apps.

## What is new in v0.3.1

- Completed BT tasks stop seeding automatically by default, including existing configurations.
- Opt in with **Continue seeding after download** in Settings; turning it off also stops completed tasks that are currently seeding.
- Completed, stopped tasks show **Completed**. Downloading tasks can still upload available pieces.

## What is new in v0.3.0

- Right-click media tasks to play or stream while downloading; automatically detect common video/audio file types.
- A single video window with mouse controls; open the additional Flow control panel only when needed.
- Subtitle/audio selection, speed controls, and local playback-position history.
- Fix pause/resume state synchronization, including resuming while file verification is running.

## Features

- Magnet links and local `.torrent` files, with native file and folder pickers.
- Compact task list with search, status filters, file progress, peer details, and diagnostic events.
- Pause/resume and verify existing torrent files.
- HTTP/HTTPS direct downloads with validated range resume when supported by the server.
- Torrent file selection, per-task and aggregate speed curves for the last five minutes.
- Remove tasks with a choice to keep files, delete incomplete files, or delete all task files.
- Close to the system tray and continue downloading in the background.
- Right-click to copy magnet links, original sources, names, or save paths, and open download folders.
- Double-click a task name or press **Space** to pause/resume. **Delete** opens a removal confirmation.
- Persistent default directory, global speed limits, and connection limit settings.
- Editable Tracker subscriptions, ordered mirrors, local caching, retry backoff, and periodic health checks.
- Resource-specific Tracker statistics and scoring, DHT discovery with persisted routing state, and engine session persistence.
- Light/dark appearance and an embedded multi-resolution application icon.

## Build and run

**Ready-to-run Windows build:** download `Flow.exe` from [GitHub Releases](https://github.com/wrench1997/flow/releases/latest). SHA-256 checksums are included with each release.

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

The default upload limit is **512 KiB/s**. By default, completed BitTorrent tasks automatically stop seeding. Enable **Continue seeding after download** in Settings to opt in. This also applies to older configurations; downloading tasks may still upload available pieces. By default, closing the window hides Flow in the system tray and downloads continue. Double-click the tray icon to restore the window; choose **Exit and stop downloads** from its menu to stop the engine. Disable **Background downloading on close** in Settings to exit when closing the window.

Removal defaults to keeping files. You can instead delete incomplete files (including HTTP partial files), or all files owned by the task. File deletion is permanent and does not use the Recycle Bin; unrelated files and directories are retained.

For selective torrent downloads, enable **Pause after adding**, wait for metadata, then select files in the **Files** tab and apply before resuming. Shared torrent pieces may write some bytes to deselected files.

HTTP links must point directly to a file, rather than a sharing webpage. Partial data is stored beside the destination as `.flow-<id>.part`; output names include a short task ID to avoid collisions. Safe resume requires server range support and an ETag or Last-Modified validator. If the server ignores the range request or no validator is available, the download restarts from the beginning. BT and HTTP downloads share the global download limit.

The **Speed curves** tab shows the current task or all tasks. Samples cover up to five minutes in the current app session and are not saved across restarts.

To reuse another client's files, add the matching torrent and choose the original save directory. Flow verifies existing data before continuing. Verification progress is distinguished from download progress; another engine's resume file cannot replace verification.

To upgrade, exit Flow through the tray menu, replace `Flow.exe`, and retain `data/` and your download folders.

## Flow Player

Flow includes its own playback controls backed by mpv. On first playback, Flow downloads a pinned Windows build linked from [mpv’s installation page](https://mpv.io/installation/), verifies SHA-256, extracts it using the Windows 10/11 archive utility, and continues playback. Progress and retry are available inside the app. No script or separate installation is required. The runtime stays in `runtime/mpv/` beside Flow; existing installations are reused, and system file associations are unchanged. First setup requires an internet connection and write access beside the executable. `setup-player.ps1` remains an optional manual alternative.

- Open **Player** in the toolbar for local video/audio or HTTP(S) media URLs.
- For torrents/magnets, right-click a task and choose **Play / Play while downloading**. A single media file plays directly; multiple media files open a picker sorted by size. MP4, MKV and other common media extensions are recognized automatically after metadata loads. The **Files** tab also has individual Play buttons. This selects that file if needed and resumes the task.
- Playback opens only the video window, with mouse controls on hover. The extra Flow panel is opened manually from the toolbar. Flow controls pause, seek, volume, speed, fullscreen, subtitles, and audio tracks. Playback positions are stored locally in `data/player-history.json`.
- Torrent playback uses authenticated loopback HTTP with byte-range seeking and librqbit's stream-aware piece scheduling. It waits for missing pieces rather than reading unwritten file bytes.
- Closing the control panel leaves playback running; closing the video window stops playback. Stopping playback does not pause downloads. Fully exiting Flow stops both.

This is a first playback integration, not an embedded video canvas. Source availability and download speed determine buffering; dragging to missing data can take time. No DRM, sharing-page extraction, disc menus, or playlist manager is provided. Copy `runtime/mpv/` together with the executable when moving a player-enabled installation. The player is included starting with v0.3.0.

## Tracker resilience

Flow preserves existing torrent Trackers and subscribes to public lists from [XIU2](https://github.com/XIU2/TrackersListCollection) [ngosang](https://github.com/ngosang/trackerslist), [newTrackon](https://newtrackon.com/), [animeTrackerList](https://github.com/DeSireFire/animeTrackerList), and [OpenTracker](https://github.com/1265578519/OpenTracker). These projects publish address lists; individual Tracker servers are independently operated.

In **Tracker → Subscription settings**, add, edit, disable, or remove sources. Each source supports up to five HTTP/HTTPS mirrors, tried in order within a 12-second refresh budget. Enabled sources refresh concurrently (at most 16), and duplicate Tracker addresses are merged. The v0.4.0 upgrade adds missing new sources once; removing them afterward is respected on restart. If all mirrors fail or return invalid/empty data, Flow retains the last successful cache.

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
| `<task-id>.http.json` | HTTP transfer and resume state |
| `rqbit/` | Engine sessions and fast-resume state |
| `status.json` / `events-rust.jsonl` | Local status and diagnostics |

To move an installation, copy the executable and `data/` together and keep the referenced download paths available. The local API uses a random loopback port and a fresh Bearer token per launch. A directory lock prevents concurrent engine instances from using the same state directory.

## Project structure

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | Desktop interface and interactions |
| `src/backend.rs` | Embedded backend lifecycle |
| `src/engine.rs` | Download sessions, tasks, persistence, and local API |
| `src/http_download.rs` | HTTP transfers and validated resume |
| `src/file_ops.rs` | Scoped file deletion |
| `src/player.rs` / `src/media.rs` | Player controls and HTTP byte-range support |
| `src/tray.rs` | Windows tray and background lifecycle |
| `src/trackers.rs` | Scrape queries and scoring |
| `src/subscriptions.rs` | Sources, mirrors, cache, and retry policy |
| `src/settings.rs` | Transfer settings and validation |
| `assets/`, `build.rs` | Icons and Windows resource embedding |

## Windows download notice

Flow is currently unsigned. A user reported antivirus removal of a local preview build; the detection name and affected file have not been supplied, so the cause is unresolved. A new release is not proof that the detection is a false positive. Release checksums verify file identity, not safety. If blocked, keep the detection details for investigation instead of disabling protection.

## Current limitations

Flow is an early desktop implementation. Automatic application updates and persistent theme selection are not implemented. HTTP downloads use one connection per task and do not provide browser login/cookie integration or Content-Disposition filename extraction. Magnet metadata still requires reachable peers; adding Trackers cannot guarantee resolution. Applying new Tracker candidates requires verification. Statistics that librqbit does not reliably expose, such as connected complete seeders and distributed availability, are shown as unknown.

Built with [egui](https://github.com/emilk/egui) and [librqbit](https://github.com/ikatson/rqbit).
