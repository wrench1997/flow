# Flow

**A Windows BitTorrent and HTTP(S) download manager built with Rust, egui, and librqbit.**

English · [简体中文](README.zh-CN.md)

<img src="assets/flow-icon.png" alt="Flow icon" width="112" />

One executable starts both the desktop interface and embedded download engine. No Python installation or separate backend service is required.

## Long-running memory control (0.4.17)

- Hidden or busy windows retain only the latest state refresh while operation responses remain ordered.
- Slow-disk resume persistence retains the in-flight and latest pending bitmaps, including final progress on pause/exit.
- Each BT task remembers at most 4096 candidate peers and queues at most 4096 discovery or upload requests. Active/retrying peers are preserved; full upload queues apply backpressure, cancelled on pause.
- Task deletion releases completed file lists, loading state and peer sampling caches.
- Run `powershell -File scripts/test-memory.ps1 -RepeatCount 10` for repeated memory regressions covering stalled consumers, peer floods, slow disk and pause/resume resource release.
- Version 0.4.17 supports manual download/install. The original update signing key is unavailable on this release host, so this release has no in-app update manifest. Signature verification remains enabled; fully exit Flow before upgrading manually.

## Installation behavior

- Fix Windows canonical path handling when creating shortcuts, including Chinese names and spaces.
- Use the selected installation folder directly without appending another Flow folder. Retry in the same directory while retaining user data.

## Packaging and updates

- Windows installer: detects a previous registered installation and reuses its directory for an in-place upgrade, preserving settings and downloads. Choose a per-user installation directory, create shortcuts and register an uninstall entry. Installation includes a small, independently compiled `Flow-Uninstall.exe` without the download engine or player; use it, the Start menu's Uninstall Flow shortcut, or Windows Installed apps. Exit Flow from the tray before uninstalling. Uninstall retains downloads, runtime and personal data. Rerun the installer to add these entries or replace an older full-size uninstaller; in-app updates replace the main program only; portable copies can be removed manually.
- Portable ZIP: extract the folder and run `Flow.exe`, with no installation required.
- Automatic update checks on startup, manual checks, download progress and confirmation before replacing the executable. Installation stops Flow and restarts it afterward, preserving user data and an old-executable backup.
- Configurable HTTPS GitHub mirror prefix, with fallback to GitHub. Signed update manifests and SHA-256 verification are enforced even through a mirror.

## Downloads and updates

| File | Use |
| --- | --- |
| `Flow-Setup-<release>-x64.exe` | Installer with directory selection, shortcuts and uninstall entry |
| `Flow-<release>-windows-x64-portable.zip` | Extract and run; keep the extracted folder writable |
| `Flow.exe` | Replace an existing copy after fully exiting Flow |

Use **Update** in the toolbar or **Settings → Software update / Mirror**. By default, Flow checks once each launch; downloads and installation require confirmation. Updates use the same in-place executable replacement for installed and portable copies. Existing downloads, `data/` and `runtime/` remain in place. Backups are saved under `data/updates/`.

Mirror setting: enter an HTTPS prefix provided by your proxy service. The client requests `PREFIX/https://github.com/…`; the service must support GitHub Releases assets and redirects. A configured mirror is tried first, then official GitHub on failure. Leave it blank for direct GitHub access. No public mirror is bundled or guaranteed available. Signature verification cannot be disabled; a forged manifest, a modified executable or a downgrade is rejected. The app currently targets Windows x64.

Copies without the built-in updater need a manual upgrade first. Installation does not automatically migrate data from a separate portable folder: exit Flow and copy `data/` and optional `runtime/` to the chosen installation directory if retaining that setup. Download paths remain unchanged; keep the original download folders.

## Source recovery

### Network settings and diagnostics

- Settings offers IPv4/IPv6 dual stack and independently selectable TCP/uTP transports. Defaults enable both transports and IPv6 where available, using port 51413. If the fixed port cannot bind, Flow falls back to an automatic port and reports the reason. Network changes require fully exiting and restarting Flow; existing task data and preferences are retained.
- Optional UPnP requests router port mappings. The Diagnostics tab shows the actual listener, enabled transports, connection attempts/errors and mapping-service failures. The local listener check tests loopback only: it does not prove internet reachability, successful router mapping or NAT hole punching.
- Metadata discovery records observed peer addresses, DHT/Tracker/cache channels, BT handshakes, metadata bytes and concrete failures for the current session (up to 256 peers per hash). Metadata bytes are not downloaded file bytes. Peer details also expose transfer-connection errors. A responsive Tracker or a successful socket connection does not prove that anyone can supply the resource.
- Metadata compatibility: peers that advertise `ut_metadata` without a handshake size are probed for the first block. Missing fragments are collected across peers and discovery rounds in memory for the current session; the complete result must match the magnet SHA-1 before use. Hash failures discard the assembled fragments. Peer requests use a two-block window, failed peers retry with 60–300 second backoff plus jitter, and unsuccessful 90-second discovery rounds resume after a 15–120 second delay. Pause, deletion and engine shutdown cancel discovery and backoff.
- The task overview distinguishes no discovered peers, connection/handshake attempts, waiting for metadata, retained partial metadata and verified metadata. It shows retained fragment counts, concrete peer errors and the next discovery countdown. This does not prove that file pieces are available. Fragment caches are memory-only (not retained across application restarts), capped at 32 MiB per metadata item and 64 MiB across collectors, and released after successful resolution.
- If metadata cannot be obtained, importing the matching `.torrent` bypasses that stage, but still requires reachable peers holding the selected pieces. More Trackers cannot recreate missing data. No BEP 55 hole punching, proprietary offline cache or cross-torrent file index is provided.

- After two minutes without connections or download progress, restart the existing task’s discovery stream, with a ten-minute cooldown. Paused/completed tasks and tasks with live peers are left alone; verified pieces are preserved. Discovery uses the task’s existing Trackers and DHT where allowed, not a scrape-only query. Formerly useful disconnected peers remain cached and are labeled as past contributors. Recovery attempts are recorded in the diagnostic log; discovering a usable alternative is not guaranteed.

## Peer management and torrent import

- Peer records and manual IP blacklist management persist locally. Reference scores use observed sustained/intermittent/idle transfer (90/60/10); new peers remain unscored. Blocking and unblocking require restarting Flow to update the native incoming/outgoing connection filter. Shared IPs affect all BT tasks; there is no automatic malicious-client classification. Records include the latest observed task, client, cumulative session transfers, errors and policy reason.

- Remove the composite Tracker score from the UI. Peer rows and reconnect-cache candidates prioritize sustained received data: three positive 10-second samples, then intermittent transfer, new observations, and idle peers. Recent receive rates and cumulative uploads/downloads are visible. This does not override engine bandwidth scheduling or automatically ban clients. Connection diagnostics now include failed/disconnected peers, instead of only live connections.

- Long links scroll inside a bounded input area; the import dialog is constrained to the available window height.

- Drag one or more `.torrent` files into Flow to queue import dialogs. Local torrents are parsed before adding; select the files to download and see their combined size. Empty selections cannot start.
- Settings includes a `.torrent` association button. Double-clicking opens the same import dialog, including when Flow is already running. If Windows has an explicit default, select Flow in Open with → Always.
- Magnets can be added paused, then configured in the Files tab once metadata arrives. Shared pieces can write some data to adjacent unselected files.

## Player setup

- First playback automatically downloads, verifies and installs the pinned mpv runtime. Download progress and retry are built into Flow; playback resumes after setup. No manual PowerShell script is needed. Existing installations are reused.

## Task management and engine startup

- Five built-in Tracker subscriptions: XIU2, ngosang, newTrackon, animeTrackerList, and OpenTracker. Existing installations receive the three additions once; disabled/custom sources are preserved. Sources refresh concurrently with a 12-second budget per source, mirror fallback, deduplication and last-good cache retention.
- Completed tasks are excluded from the Paused category; Paused means an unfinished download that has been stopped.

- Drag with the left mouse button in the task list to select intersecting rows with a translucent blue marquee; Ctrl-drag adds to the selection. Ctrl-click task names to toggle multiple selections. Right-click removal, the toolbar and Delete share a batch confirmation dialog, keep files by default and report per-task failures.

- Magnet metadata resolution reuses cached peers and adds subscribed sources before resolving tracker-less magnets, with continuing discovery rounds, cancellable backoff and stage diagnostics. Explicit tracker sets are preserved. Startup resolves at most two pending magnets concurrently.
- Cache metadata-discovery candidates, replacing them every 30 seconds with peers that actually transferred data, ranked by bytes received; retain up to 64 peers for seven days and exclude private torrents. Diagnostics separate metadata acquisition, no connection attempts, failed connections and connected-but-idle peers without inventing piece-availability information.
- Tracker rows show response history, latency and reported peers. The UI does not present a composite score as evidence of download speed; actual received bytes are shown in peer diagnostics.

- While Flow is running (including in the tray), use **Copy link address** in Chrome or Edge on an HTTP(S) file link to open Flow's new-task confirmation. Magnets and sharing URLs containing `magnet:?` work as before. The clipboard monitor prompts only for recognizable file URLs (for example `.zip`, `.exe`, `.mp4`); ordinary webpage URLs and extensionless download endpoints are ignored. Paste those manually into New task. Existing clipboard contents are ignored at startup, and copying the same link again is recognized. Clipboard monitoring can be disabled in Settings. No browser extension is required; Flow cannot automatically intercept Chrome downloads without one. No download starts without confirmation.

- Stopped torrents with a complete persisted piece bitmap restore as saved completed tasks without opening their payload files or creating download sessions. File lists and local playback remain available; Verify explicitly reloads the task. Saved completion is not a fresh disk integrity check. Completed task names are restored from cached torrent metadata, including magnets without a display-name parameter.

- Peer queries use existing task handles instead of waiting for the session lock during file opening. A separate health check distinguishes failed refreshes from an unresponsive engine. **Exit application** in the main toolbar stops background downloads and exits.

- The window opens before engine initialization and displays the current stage, errors, and a retry button. Closing while disconnected exits instead of hiding to the tray.
- Flow restores tasks in the background after opening its local API. The derived engine index is backed up before rebuilding; downloaded files and resume bitmaps are retained.
- Cached torrents with fewer files restore first, before large file collections and unresolved magnets. During restoration, the UI shows known size, the current stage and elapsed time; progress is marked as pending until resume state is available.
- Initialization can be cancelled, and shutdown bounds the wait for active media streams. Diagnostic stages are saved in `data/startup.log`.
- Run `register-defaults.ps1` to register this copy of Flow for magnet links and `.torrent` files for the current Windows user. Windows may still require selecting Flow in Default apps.

## Completion and seeding

- Completed BT tasks stop seeding automatically by default, including existing configurations.
- Opt in with **Continue seeding after download** in Settings; turning it off also stops completed tasks that are currently seeding.
- Completed, stopped tasks show **Completed**. Downloading tasks can still upload available pieces.

## Playback controls

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
- Resource-specific Tracker statistics, DHT discovery with persisted routing state, and engine session persistence.
- Light/dark appearance and an embedded multi-resolution application icon.

## Build and run

**Ready-to-run Windows build:** download the installer, portable ZIP or `Flow.exe` from the [Flow 0.4.17 release](https://github.com/wrench1997/flow/releases/tag/v0.4.17). This release requires a manual upgrade. SHA-256 checksums are included.

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

Flow includes its own playback controls backed by mpv. On first playback, Flow downloads a pinned Windows build linked from [mpv’s installation page](https://mpv.io/installation/), verifies SHA-256, extracts the 7z archive with its built-in Rust decoder, and continues playback. It no longer depends on Windows 10's `tar.exe` supporting 7z. Progress and retry are available inside the app, and extraction failures include the underlying reason. No script or separate installation is required. The runtime stays in `runtime/mpv/` beside Flow; existing installations are reused, and system file associations are unchanged. First setup requires an internet connection and write access beside the executable. `setup-player.ps1` remains an optional manual alternative using the same built-in installer.

- Open **Player** in the toolbar for local video/audio or HTTP(S) media URLs.
- For torrents/magnets, right-click a task and choose **Play / Play while downloading**. A single media file plays directly; multiple media files open a picker sorted by size. MP4, MKV and other common media extensions are recognized automatically after metadata loads. The **Files** tab also has individual Play buttons. This selects that file if needed and resumes the task.
- Playback opens only the video window, with mouse controls on hover. The extra Flow panel is opened manually from the toolbar. Flow controls pause, seek, volume, speed, fullscreen, subtitles, and audio tracks. Playback positions are stored locally in `data/player-history.json`.
- Torrent playback uses authenticated loopback HTTP with byte-range seeking and librqbit's stream-aware piece scheduling. It waits for missing pieces rather than reading unwritten file bytes.
- Closing the control panel leaves playback running; closing the video window stops playback. Stopping playback does not pause downloads. Fully exiting Flow stops both.

This is a first playback integration, not an embedded video canvas. Source availability and download speed determine buffering; dragging to missing data can take time. No DRM, sharing-page extraction, disc menus, or playlist manager is provided. Copy `runtime/mpv/` together with the executable when moving a player-enabled installation.

## Tracker resilience

Flow preserves existing torrent Trackers and subscribes to public lists from [XIU2](https://github.com/XIU2/TrackersListCollection) [ngosang](https://github.com/ngosang/trackerslist), [newTrackon](https://newtrackon.com/), [animeTrackerList](https://github.com/DeSireFire/animeTrackerList), and [OpenTracker](https://github.com/1265578519/OpenTracker). These projects publish address lists; individual Tracker servers are independently operated.

In **Tracker → Subscription settings**, add, edit, disable, or remove sources. Each source supports up to five HTTP/HTTPS mirrors, tried in order within a 12-second refresh budget. Enabled sources refresh concurrently (at most 16), and duplicate Tracker addresses are merged. Missing built-in sources are added once during subscription migration; later removals are respected on restart. If all mirrors fail or return invalid/empty data, Flow retains the last successful cache.

- Default list refresh: **24 hours**; health checks for unpaused tasks: **30 minutes**, both configurable.
- Failed queries use exponential retry backoff, capped at **6 hours**.
- Up to **200 Trackers per task**, **12 concurrent queries** across tasks, and an **8-second** request timeout.
- HTTP/UDP scrape queries use the current resource hash and show reported seed counts, response history and latency.
- Private torrents are excluded from public Tracker discovery.

**Background checks update candidates and query records without restarting downloads.** New candidates require **Apply candidates**, which currently reloads the task and verifies existing files.

A scrape failure does not prove a Tracker cannot return peers. Reported seed counts are not connected peers, and a successful response does not guarantee a usable download source. DHT offers another discovery path for public torrents, but cannot recover missing content without reachable peers holding it.

## Local data and portability

The toolbar and Settings offer **Automatic (system)**, **简体中文**, and **English**. Automatic uses the Windows display language: Chinese systems use Simplified Chinese; other systems use English. Switching is immediate and does not restart downloads. The installer passes its language choice to the installed app; player, updater, tray and uninstaller use the same preference, saved in `data/ui-language.json`. File names, URLs, paths and copied diagnostics are preserved; system dialogs and external error details may retain their original language.

Runtime data lives in `data/` beside the executable:

| Path | Purpose |
| --- | --- |
| `settings.json` | Download directory and transfer settings |
| `ui-language.json` | Automatic, Chinese, or English interface preference |
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
| `tools/installer/` / `tools/uninstaller/` | Independently compiled installer and small native uninstaller |
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

## Additional sources and transfer evidence

Flow reads HTTP(S) WebSeeds from a torrent's `url-list` and magnet `ws` parameters. It requests byte ranges and verifies each complete piece against the torrent's SHA-1 before passing it to the download engine. Multi-file pieces may require boundary bytes from neighboring unselected files; unrelated files are not requested. Incorrect hashes and invalid range responses isolate that source for the current task session; temporary failures use retry backoff. Pause cancels active WebSeed requests.

WebSeed candidates and historical verified bytes are displayed separately from BT peers. A responsive Tracker, a reported seeder count, and an HTTP candidate do not prove that data is currently downloadable. Verified bytes count transferred pieces, including repeat transfers, rather than task completion. A WebSeed must actually host the torrent's exact bytes; Flow does not discover arbitrary same-file mirrors or access BitComet's long-term seeding network.

Automatic rediscovery observes two minutes without connections, or five minutes with connections but no progress, and uses a ten-minute retry cooldown. Paused and finished tasks are excluded. These observations indicate a stall, not malicious behavior by a peer.

Missing-piece coverage is the fraction of selected, not-yet-verified pieces announced by current online BT peers. It excludes disconnected peers and the local WebSeed transport. Unknown peer declarations make this an observed lower bound, not global swarm availability; 100% does not guarantee delivery. Diagnosis distinguishes missing declarations, no needed pieces, choke, and outstanding requests. No online declarations, stopped tasks, and completed tasks display unknown coverage. The read-only engine extension is tracked in `vendor/librqbit/FLOW-PATCH.md`.

## Current limitations

Flow is an early desktop implementation. Persistent theme selection is not implemented. HTTP downloads use one connection per task and do not provide browser login/cookie integration or Content-Disposition filename extraction. Magnet metadata still requires reachable peers; adding Trackers cannot guarantee resolution. Applying new Tracker candidates requires verification. Connected complete seeder counts and global distributed availability remain unknown. WebSeeds require usable byte-range responses, pieces no larger than 32 MiB, and requests completing within 30 seconds; slow or incompatible servers may fail even when ordinary browser downloads work.

Built with [egui](https://github.com/emilk/egui) and [librqbit](https://github.com/ikatson/rqbit).

Tray controls: right-click opens Show main window / Stop downloads and exit; a single left-click does nothing, and a left double-click restores the main window.

Toolbar actions use vector icons with localized hover hints. Tracker subscriptions can be opened directly from the toolbar or Settings, even without a selected task. Settings and utility windows cannot be collapsed.

Exit from the tray requests engine shutdown immediately, independently of pending status requests, then closes the hidden window and saves task state.

Saving Tracker subscriptions now confirms the save and loads candidates in the background without reloading download tasks. The Tracker page shows loading progress, candidate counts and source refresh/cache details, and distinguishes addresses loaded into the engine from candidates awaiting application. Loaded configuration does not prove an active connection or download; peer-to-Tracker attribution is not currently available. Saving settings no longer waits for an ongoing network refresh.
