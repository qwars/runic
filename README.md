# Runic

A lightweight engine for rendering HTML/CSS/JS widgets on Linux desktops. A modern alternative to Conky with a web stack: write widgets using familiar HTML, CSS, and JavaScript, while Runic handles system integration through a transparent IPC bridge.

Built with Rust + GTK4 + WebKitGTK 6.0 + JavaScriptCore6 + Layer Shell (Wayland).

## Features
- **True web rendering** — HTML5, CSS3, modern JavaScript via WebKitGTK
- **Layer Shell integration** — widgets sit below all windows (or on other layers)
- **Transparent background** — widgets blend seamlessly into the desktop
- **Powerful IPC bridge** — 8 built-in actions: file read/write, command execution, streaming output, file monitoring, and stream control
- **Flexible positioning** — custom size, coordinates, fullscreen mode
- **Async streaming** — long-running command output flows to widgets in real-time
- **File monitoring via gio::FileMonitor** — track file changes without polling (zero CPU load, event-driven)
- **Hot-reload** — automatic widget reload when `.html`, `.css`, or `.js` files change
- **Event-driven Sleep Detector (DBus)** — instant reaction to system suspend/resume via `systemd-logind` signals, no polling overhead

## 🚀 Performance (v2.0.0)
The project is optimized for minimal resource consumption:
| Metric | Value |
|---|---|
| Peak memory at startup | ~30-50 MB |
| Thread count | 8-12 (fixed pool) |
| Idle CPU | <0.1% (event-driven DBus instead of polling) |
| IPC response time | <1 ms |
| Zombie processes | None (Process Groups + ProcessGuard) |

**Architectural Optimizations:**
- **Event-driven DBus (`PrepareForSleep`):** Instant widget sleep/wake without background timers.
- **gio::FileMonitor:** Hot-reload runs in the GLib context, eliminating extra threads for `inotify`.
- **ProcessGuard with adaptive kill:** Process groups with `SIGTERM` + adaptive wait + `SIGKILL` — zero zombie processes.

## Requirements

- **OS**: Linux with Wayland compositor (Sway, Hyprland, Wayfire, etc.)
- **System libraries**: GTK 4.14+, WebKitGTK 6.0, gtk4-layer-shell
- **Rust**: 1.80+ (for building, edition 2024)

## Installation

### 1. System dependencies

**Debian 13 / Ubuntu:**
```bash
sudo apt install -y build-essential pkg-config libgtk-4-dev libwebkitgtk-6.0-dev libgtk4-layer-shell-dev
```

**Arch Linux:**
```bash
sudo pacman -S gtk4 webkitgtk-6.0 gtk4-layer-shell
```

**Fedora:**
```bash
sudo dnf install gtk4-devel webkitgtk6.0-devel gtk4-layer-shell-devel
```


### 2. Build
```bash
git clone https://github.com/qwars/runic.git
cd runic

cargo build --release
```
The binary will be at `target/release/runic`.

For verbose debug output (hot-reload, IPC tracing, profiling):
```bash
cargo run -- examples/test.html/index.html --debug
```

## Usage

### Basic launch

```bash
# Fullscreen widget (default)
./target/release/runic examples/test.html/index.html

# Fixed size 400x300 in top-left corner
./target/release/runic examples/test.html/index.html -w 400 -H 300

# With offset from edges
./target/release/runic examples/test.html/index.html -w 400 -H 300 -x 50 -y 100

# Debug mode (verbose terminal output with logs and profiling)
./target/release/runic examples/test.html/index.html --debug
```

### Command-line arguments
| Argument         | Description                                                         | Default    |
|------------------|---------------------------------------------------------------------|------------|
| html_path        | Path to the widget HTML file                                        | (required) |
| -w, --width <N>  | Window width in pixels                                              | fullscreen |
| -H, --height <N> | Window height in pixels                                             | fullscreen |
| -x, --x <N>      | Offset from left edge                                               | 0          |
| -y, --y <N>      | Offset from top edge                                                | 0          |
| -d, --debug      | Enable verbose debug output (logs, IPC trace, profiling)            | false      |

**Note**: `-x` and `-y` coordinates only work when window size is specified (`-w` and `-H`).

### Autostart in Sway

Add to `~/.config/sway/config`:

```
exec /home/user/runic/target/release/runic /home/user/runic/widgets/clock.html -w 300 -H 150 -x 50 -y 50
```

## Widget API

Runic provides a JavaScript object `window.ipc` for system interaction.

### Sending a message

```javascript
window.ipc.postMessage(
  JSON.stringify({
    action: "exec",
    payload: { command: "uname -a" },
  })
);
```

### Receiving responses

Define a global handler:

```javascript
window.onRunicResponse = function (response) {
  // response.action  — action name
  // response.status  — "success" | "error" | "stream_data" | "watch_data"
  // response.message — text message
  // response.data    — payload (string or null)
  console.log(response);
};
```

### Available actions

#### `read` — read a file

```json
{ "action": "read", "payload": { "path": "/etc/os-release" } }
```

Response: `data` contains file contents.

#### `write` — append to a file

```json
{ "action": "write", "payload": { "path": "/tmp/log.txt", "data": "line\n" } }
```

**Note**: Parent directories are created automatically. File is opened in `append` mode by default (can be overridden with `"append": false` in payload to truncate).

#### `exec` — execute a command

```json
{ "action": "exec", "payload": { "command": "df -h" } }
```

Response: `data` contains `stdout` on success or `STDERR:\n...` on error. Simple commands (no pipes, redirects, etc.) run directly; complex commands (pipes, redirects, shell variables) run via `sh -c`. All commands run in an isolated Process Group.

#### `stream` — streaming execution

```json
{ "action": "stream", "payload": { "target": "ping -c 10 8.8.8.8" } }
```

Response: Arrives multiple times, one line at a time, with status `stream_data`. Perfect for `top`, `ping`, `tail -f`, and other long-running commands. Re-running the same command is blocked until the previous one completes.

#### `unstream` — stop a streaming command

```json
{ "action": "unstream", "payload": { "target": "ping -c 10 8.8.8.8" } }
```

Response: Stops a previously started `stream` command. The entire Process Group receives `SIGKILL`, and the stream is removed from the active list.

#### `watch` — monitor a file via gio::FileMonitor

```json
{ "action": "watch", "payload": { "path": "/var/log/syslog", "tail": 10 } }
```

Tracks file changes using `gio::FileMonitor` (event-driven, zero CPU load). On each file change, new lines are sent to the widget with status `watch_data`.

**Parameters**: `path` (required), `tail` (optional, number of last lines to send on startup).

**Important**: Re-running `watch` for the same file will return an error. Use `unwatch` to stop.

**Note**: Reading system logs requires membership in the `adm` group.

#### `unwatch` — stop file monitoring

```json
{ "action": "unwatch", "payload": { "path": "/var/log/syslog" } }
```

Response: Stops monitoring a file previously started via `watch`. The `gio::FileMonitor` terminates.

#### `stats` — runtime statistics

```json
{ "action": "stats" }
```

Response: Returns current counts of active streams, watchers, executions, and the task queue length.

### Debug mode
The `--debug` flag enables verbose debug output via `env_logger`:
- All IPC messages printed to the terminal with structured logging.
- `console.log` from JS redirected to stdout.
- Hot-reload: automatic widget reload when `.html`, `.css`, `.js` files change (300ms debounce protection).
- STDOUT/stderr from streamed commands captured and logged.

*Note: Without `--debug`, the program runs silently — no console output (except critical startup errors).*

### Example debug output:

```
2026-10-26 12:00:00.123 DEBUG [runic] ipc {"action":"exec","payload":{"command":"uname -a"}}
2026-10-26 12:00:00.125 DEBUG [runic] read 'examples/test.html/index.html' => 1024 bytes
2026-10-26 12:00:00.130 DEBUG [runic] exec 'uname -a' => code: 0, output: 'Linux hostname 6.1.0...'
```

**Important**: In normal mode (without `--debug`), the program runs quietly — no console output (except critical startup errors). This allows using Runic in production without log pollution.

## License

MIT

## Acknowledgments

Inspired by [Conky](https://github.com/brndnmtthws/conky) and built with excellent Rust bindings:

- [gtk4-rs](https://github.com/gtk-rs/gtk4-rs)
- [webkit6-rs](https://crates.io/crates/webkit6)
- [gtk4-layer-shell-rs](https://github.com/Smithay/gtk4-layer-shell-rs)
