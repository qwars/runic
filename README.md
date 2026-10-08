# Runic

Runic is a lightweight engine for rendering HTML/CSS/JS widgets on Linux desktops. A modern alternative to Conky with a web stack: write widgets using familiar HTML, CSS, and JavaScript, while Runic handles system integration through a transparent IPC bridge.

Built with Rust + GTK3 + WebKitGTK + Layer Shell (Wayland).

## Features

- **True web rendering** — HTML5, CSS3, modern JavaScript via WebKitGTK
- **Layer Shell integration** — widgets sit below all windows (or on other layers)
- **Transparent background** — widgets blend seamlessly into the desktop
- **Powerful IPC bridge** — 6 built-in actions: file read/write, command execution, streaming output, file monitoring
- **Flexible positioning** — custom size, coordinates, fullscreen mode
- **Quiet mode** — no log spam by default, debug output enabled with `--debug` flag
- **Async streaming** — long-running command output flows to widgets in real-time
- **File monitoring via inotify** — track file changes without polling (zero CPU load while idle)
- **Hot-reload** — automatic widget reload when `.html`, `.css`, or `.js` files change (in `--debug` mode)

## Requirements

- **OS**: Linux with Wayland compositor (Sway, Hyprland, Wayfire, etc.)
- **System libraries**: GTK 3.24+, WebKitGTK 4.1, gtk-layer-shell
- **Rust**: 1.85+ (for building)

## Installation

### 1. System dependencies

**Debian 13 / Ubuntu:**
```bash
sudo apt install -y \
    build-essential pkg-config \
    libgtk-3-dev libwebkit2gtk-4.1-dev \
    libgtk-layer-shell-dev
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

## Usage

### Basic launch

```bash
# Fullscreen widget (default)
./target/release/runic examples/test.html/index.html

# Fixed size 400×300 in top-left corner
./target/release/runic examples/test.html/index.html -w 400 -H 300

# With offset from edges
./target/release/runic examples/test.html/index.html -w 400 -H 300 -x 50 -y 100

# Debug mode (verbose terminal output + hot-reload)
./target/release/runic examples/test.html/index.html --debug
```

### Command-line arguments

| Argument           | Description                          | Default      |
|--------------------|--------------------------------------|--------------|
| `html_path`        | Path to the widget HTML file         | *(required)* |
| `-w, --width <N>`  | Window width in pixels               | fullscreen   |
| `-H, --height <N>` | Window height in pixels              | fullscreen   |
| `-x, --x <N>`      | Offset from left edge                | `0`          |
| `-y, --y <N>`      | Offset from top edge                 | `0`          |
| `-d, --debug`      | Enable verbose output and hot-reload | `false`      |


*Note: `-x` and `-y` coordinates only work when window size is specified (`-w` and `-H`).*

### Autostart in Sway

Add to `~/.config/sway/config`:
```text
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
*Response*: `data` contains file contents.

#### `write` — write or append to a file
```json
{ "action": "write", "payload": { "path": "/tmp/log.txt", "data": "line\n", "append": true } }
```
*Parameters*: 
- `path` (required)
- `data` (required)
- `append` (optional, default: `false`). If `true`, data is appended; otherwise, the file is truncated and overwritten.
Parent directories are created automatically.

#### `exec` — execute a command
```json
{ "action": "exec", "payload": { "command": "df -h" } }
```
*Response*: `data` contains `stdout` on success or `STDERR:\n...` on error. Command runs via `sh -c`.

#### `stream` — streaming execution
```json
{ "action": "stream", "payload": { "target": "ping -c 10 8.8.8.8" } }
```
*Response*: Arrives multiple times, one line at a time, with status `stream_data`. Perfect for `top`, `ping`, `tail -f`, and other long-running commands. Re-running the same command is blocked until the previous one completes.

#### `watch` — monitor a file via inotify
```json
{ "action": "watch", "payload": { "path": "/var/log/syslog", "tail": 10 } }
```
Tracks file changes using the `inotify` system call (zero CPU load while waiting for events). On each file change, new lines are sent to the widget with status `watch_data`.
*Parameters*:
- `path` (required) — path to the file or directory to monitor
- `tail` (optional) — number of last lines to send on startup (files only)

*Responses*:
- `status: "success"` — monitoring started
- `status: "watch_data"` — new line from the file (in `data`), file path in `message`
- `status: "error"` — error (file already being monitored, no access, etc.)

*Important*:
- Re-running `watch` for the same path will return an error.
- Use the `unwatch` action to stop monitoring.
- Reading system logs (`/var/log/syslog`, `/var/log/auth.log`) requires membership in the `adm` group.
- On log rotation (logrotate), monitoring continues to follow the old file descriptor — a `watch` restart is required.

#### `unwatch` — stop file monitoring
```json
{ "action": "unwatch", "payload": { "path": "/var/log/syslog" } }
```
Stops monitoring a file previously started via `watch`. The inotify thread terminates, the file descriptor is released.

## Debug mode

The `--debug` flag enables:
- All IPC messages printed to terminal
- `console.log` from JS redirected to stdout
- Size and positioning information
- **Hot-reload**: automatic widget reload when `.html`, `.css`, or `.js` files change in the widget directory (300ms debounce protection)

```bash
cargo run -- examples/test.html/index.html --debug
```

## License

MIT

## Acknowledgments

Inspired by [Conky](https://github.com/brndnmtthws/conky) and built with excellent Rust bindings:
- [gtk-rs](https://github.com/gtk-rs/gtk3-rs)
- [webkit2gtk-rs](https://github.com/nicokosi/webkit2gtk-rs)
- [gtk-layer-shell-rs](https://github.com/Smithay/gtk-layer-shell-rs)
