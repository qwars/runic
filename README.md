# Runic

A lightweight engine for rendering HTML/CSS/JS widgets on Linux desktops. A modern alternative to Conky with a web stack: write widgets using familiar HTML, CSS, and JavaScript, while Runic handles system integration through a transparent IPC bridge.

Built with **Rust + GTK3 + WebKitGTK + Layer Shell** (Wayland).

---

## Features

- **True web rendering** — HTML5, CSS3, modern JavaScript via WebKitGTK
- **Layer Shell integration** — widgets sit below all windows (or on other layers)
- **Transparent background** — widgets blend seamlessly into the desktop
- **Powerful IPC bridge** — 8 built-in actions: file read/write, command execution, streaming output, file monitoring, and stream control
- **Flexible positioning** — custom size, coordinates, fullscreen mode
- **Quiet mode** — no log spam by default, debug output enabled with `--debug` flag
- **Async streaming** — long-running command output flows to widgets in real-time
- **File monitoring via inotify** — track file changes without polling (zero CPU load while idle)
- **Hot-reload** — automatic widget reload when `.html`, `.css`, or `.js` files change (in `--debug` mode)
- **Sleep wake detector** — automatic widget reload after system wake-up

---

## 🚀 Performance (v1.3.0)

The project is optimized for minimal resource consumption:

| Metric                 | Value                 |
|------------------------|-----------------------|
| Peak memory at startup | ~30-50 MB             |
| Thread count           | 8-12 (fixed pool)     |
| Idle CPU               | <1%                   |
| IPC response time      | <1 ms                 |
| Zombie processes       | None (Process Groups) |

**ThreadPool**: a fixed pool of 8 worker threads handles all IPC requests. Eliminated ~800 MB memory consumption and kernel scheduler overhead.

**Process Groups**: all `sh -c` commands are launched in a separate process group. On termination, the entire group receives `SIGTERM`/`SIGKILL` — child processes (e.g., `tail -f`, `ping`) no longer become orphans.

**Directory debouncing**: 500 ms time-based debounce prevents inotify notification storms when scripts trigger events in monitored directories.

**Non-blocking channels**: `try_send` instead of `send_blocking` eliminates potential deadlocks when GTK main loop hangs.

**Monotonic clocks**: `Instant` instead of `SystemTime` for the sleep detector — correct behavior during system clock adjustments and NTP synchronization.

---

## 🛡️ Reliability & Security (v1.3.0)

The project undergoes continuous security and stability audits:

- **Resource Profiling**: Built-in tracking of memory (VmRSS) and CPU time (`getrusage`) for every IPC request, logged when thresholds are exceeded.
- **Thread Tracking (`ThreadTracker`)**: Comprehensive monitoring of active and completed background threads, including peak memory, lifespan, and task count, preventing silent resource exhaustion.
- **Colored Debug Output**: ANSI-colored console logs for instant visual parsing of Errors (🔴), Warnings (🟡), Success (🟢), Debug info (🔵), and Profiling data (🟣).
- **Atomic process management**: Eliminated TOCTOU (Time-of-Check to Time-of-Use) race conditions when launching streaming commands. Repeated `stream` calls with the same argument are now guaranteed to be blocked.
- **Safe resource cleanup**: Eliminated double-kill scenarios and panics during `SIGTERM`/`SIGINT` signal handling. All mutexes are protected against poisoning.
- **Graceful Shutdown**: All background threads receive a stop signal and terminate cleanly, leaving no zombie processes or memory leaks.
- **Code Quality**: The codebase fully complies with strict `cargo clippy -- -D warnings` checks and includes 15 comprehensive unit tests.

---

## Requirements

- **OS**: Linux with Wayland compositor (Sway, Hyprland, Wayfire, etc.)
- **System libraries**: GTK 3.24+, WebKitGTK 4.1, gtk-layer-shell
- **Rust**: 1.70+ (for building)

---

## Installation

### 1. System dependencies

**Debian 13 / Ubuntu:**

```bash
sudo apt install -y build-essential pkg-config libgtk-3-dev libwebkit2gtk-4.1-dev libgtk-layer-shell-dev
```

**Arch Linux:**

```bash
sudo pacman -S gtk3 webkit2gtk gtk-layer-shell
```

**Fedora:**

```bash
sudo dnf install gtk3-devel webkit2gtk4.1-devel gtk-layer-shell-devel
```

### 2. Build

```bash
git clone https://github.com/qwars/runic.git
cd runic
cargo build --release
```

The binary will be at `target/release/runic`.

---

## Usage

### Basic launch

```bash
# Fullscreen widget (default)
./target/release/runic examples/test.html/index.html

# Fixed size 400x300 in top-left corner
./target/release/runic examples/test.html/index.html -w 400 -H 300

# With offset from edges
./target/release/runic examples/test.html/index.html -w 400 -H 300 -x 50 -y 100

# Debug mode (verbose terminal output with colored logs and profiling)
./target/release/runic examples/test.html/index.html --debug
```

### Command-line arguments

| Argument           | Description                                           | Default    |
|--------------------|-------------------------------------------------------|------------|
| `html_path`        | Path to the widget HTML file                          | (required) |
| `-w, --width <N>`  | Window width in pixels                                | fullscreen |
| `-H, --height <N>` | Window height in pixels                               | fullscreen |
| `-x, --x <N>`      | Offset from left edge                                 | 0          |
| `-y, --y <N>`      | Offset from top edge                                  | 0          |
| `-d, --debug`      | Enable verbose output, colored logs, and thread stats | false      |

**Note**: `-x` and `-y` coordinates only work when window size is specified (`-w` and `-H`).

### Autostart in Sway

Add to `~/.config/sway/config`:

```
exec /home/user/runic/target/release/runic /home/user/runic/widgets/clock.html -w 300 -H 150 -x 50 -y 50
```

---

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

**Response**: `data` contains file contents.

#### `write` — append to a file

```json
{ "action": "write", "payload": { "path": "/tmp/log.txt", "data": "line\n" } }
```

**Note**: Parent directories are created automatically. File is opened in `append` mode by default (can be overridden with `"append": false` in payload to truncate).

#### `exec` — execute a command

```json
{ "action": "exec", "payload": { "command": "df -h" } }
```

**Response**: `data` contains `stdout` on success or `STDERR:\n...` on error. Command runs via `sh -c` isolated in a Process Group.

#### `stream` — streaming execution

```json
{ "action": "stream", "payload": { "target": "ping -c 10 8.8.8.8" } }
```

**Response**: Arrives multiple times, one line at a time, with status `stream_data`. Perfect for `top`, `ping`, `tail -f`, and other long-running commands. Re-running the same command is blocked until the previous one completes.

#### `unstream` — stop a streaming command

```json
{ "action": "unstream", "payload": { "target": "ping -c 10 8.8.8.8" } }
```

**Response**: Stops a previously started `stream` command. The entire Process Group receives `SIGKILL`, and the stream is removed from the active list.

#### `watch` — monitor a file via inotify

```json
{ "action": "watch", "payload": { "path": "/var/log/syslog", "tail": 10 } }
```

Tracks file changes using the `inotify` system call (zero CPU load while waiting for events). On each file change, new lines are sent to the widget with status `watch_data`.

- **Parameters**: `path` (required), `tail` (optional, number of last lines to send on startup).
- **Important**: Re-running `watch` for the same file will return an error. Use `unwatch` to stop.
- **Note**: Reading system logs requires membership in the `adm` group.

#### `unwatch` — stop file monitoring

```json
{ "action": "unwatch", "payload": { "path": "/var/log/syslog" } }
```

**Response**: Stops monitoring a file previously started via `watch`. The inotify thread terminates, and the file descriptor is released.

#### `stats` — thread statistics

```json
{ "action": "stats" }
```

**Response**: Returns a summary of active and completed threads — peak memory usage, lifespan, and task count.

---

## Debug mode

The `--debug` flag enables:

- All IPC messages printed to the terminal with colored formatting.
- `console.log` from JS redirected to stdout.
- Size and positioning information.
- **Thread statistics**: A summary of active/completed threads, memory, and CPU usage printed every 30 seconds.
- **Hot-reload**: automatic widget reload when `.html`, `.css`, `.js` files change in the widget directory (300ms debounce protection).
- **Profiling**: performance metrics logged when thresholds are exceeded.

```bash
cargo run -- examples/test.html/index.html --debug
```

---

## License

MIT

---

## Acknowledgments

Inspired by [Conky](https://github.com/brndnmtthws/conky) and built with excellent Rust bindings:

- [gtk-rs](https://github.com/gtk-rs/gtk3-rs)
- [webkit2gtk-rs](https://github.com/nicokosi/webkit2gtk-rs)
- [gtk-layer-shell-rs](https://github.com/Smithay/gtk-layer-shell-rs)
