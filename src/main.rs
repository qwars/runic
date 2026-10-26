use async_channel::Sender;
use clap::{CommandFactory, Parser};
use gtk4::gio;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use javascriptcore6::Value as JsValue;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use webkit6::prelude::*;
use webkit6::{UserContentInjectedFrames, UserContentManager, UserScript, UserScriptInjectionTime, WebView};

const WORKER_THREADS: usize = 8;
const TASK_QUEUE_SIZE: usize = 1024;
const IPC_CHANNEL_SIZE: usize = 1024;
const TAIL_READ_LIMIT: u64 = 64 * 1024;
const HOT_RELOAD_DEBOUNCE_MS: u64 = 300;
const MAX_TAIL_LINES: usize = 1000;
const KILL_GRACE_ITERATIONS: usize = 10;
const KILL_GRACE_INTERVAL_MS: u64 = 10;

#[derive(Parser)]
#[command(name = "runic", about = "HTML desktop widget")]
struct Arguments {
	#[arg(short = 'w', long)]
	width: Option<i32>,
	#[arg(short = 'H', long)]
	height: Option<i32>,
	#[arg(short = 'x', long, default_value_t = 0)]
	x_offset: i32,
	#[arg(short = 'y', long, default_value_t = 0)]
	y_offset: i32,
	#[arg(short = 'd', long)]
	debug: bool,
	#[arg(value_name = "FILE")]
	html_file_path: Option<String>,
}

#[derive(Clone)]
struct InterfaceConfiguration {
	width: Option<i32>,
	height: Option<i32>,
	x_offset: i32,
	y_offset: i32,
}

#[derive(Deserialize)]
struct IncomingMessage {
	action: String,
	payload: serde_json::Value,
}

#[derive(Serialize)]
struct Response<'a> {
	action: &'a str,
	status: &'a str,
	message: &'a str,
	data: Option<&'a str>,
}

enum WidgetEvent {
	Stop(String),
	Start(String),
}

type Task = Box<dyn FnOnce() + Send + 'static>;

struct ThreadPool {
	sender: Sender<Task>,
}

impl ThreadPool {
	fn spawn(size: usize, queue_capacity: usize) -> Arc<Self> {
		let (sender, receiver) = async_channel::bounded::<Task>(queue_capacity);
		for _ in 0..size {
			let worker: async_channel::Receiver<Task> = receiver.clone();
			thread::spawn(move || {
				while let Ok(task) = worker.recv_blocking() {
					task();
				}
			});
		}
		drop(receiver);
		Arc::new(Self { sender })
	}

	fn submit(&self, task: impl FnOnce() + Send + 'static) -> bool {
		self.sender.try_send(Box::new(task)).is_ok()
	}

	fn pending_count(&self) -> usize {
		self.sender.len()
	}
}

struct ProcessGuard {
	child: Option<std::process::Child>,
	group_id: i32,
}

impl From<std::process::Child> for ProcessGuard {
	fn from(child: std::process::Child) -> Self {
		let group_id = child.id() as i32;
		Self {
			child: Some(child),
			group_id,
		}
	}
}

impl ProcessGuard {
	fn terminate(&mut self) {
		let Some(mut child) = self.child.take() else { return };
		unsafe {
			libc::killpg(self.group_id, libc::SIGTERM);
		}
		for _ in 0..KILL_GRACE_ITERATIONS {
			match child.try_wait() {
				Ok(Some(_)) | Err(_) => return,
				Ok(None) => thread::sleep(Duration::from_millis(KILL_GRACE_INTERVAL_MS)),
			}
		}
		unsafe {
			libc::killpg(self.group_id, libc::SIGKILL);
		}
		let _ = child.wait();
	}
}

impl Drop for ProcessGuard {
	fn drop(&mut self) {
		self.terminate();
	}
}

enum WatchTarget {
	File {
		offset: u64,
		inode: u64,
		device: u64,
		handle: Option<fs::File>,
	},
	Directory {
		mtime: u64,
	},
}

struct Watcher {
	target: WatchTarget,
	cancellable: gio::Cancellable,
}

struct ApplicationState {
	streams: Arc<Mutex<HashMap<String, ProcessGuard>>>,
	watchers: Arc<Mutex<HashMap<String, Watcher>>>,
	executions: Arc<Mutex<HashSet<u32>>>,
	reload_cancellation: Mutex<Option<gio::Cancellable>>,
	shutdown: Arc<AtomicBool>,
	pool: Arc<ThreadPool>,
}

impl ApplicationState {
	fn new(pool: Arc<ThreadPool>) -> Arc<Self> {
		Arc::new(Self {
			streams: Arc::new(Mutex::default()),
			watchers: Arc::new(Mutex::default()),
			executions: Arc::new(Mutex::default()),
			reload_cancellation: Mutex::default(),
			shutdown: Arc::new(AtomicBool::new(false)),
			pool,
		})
	}
}

fn modification_timestamp(path: &Path) -> u64 {
	fs::metadata(path)
		.ok()
		.and_then(|m| m.modified().ok())
		.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

fn payload_string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
	value.get(key)?.as_str()
}

fn payload_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
	value.get(key)?.as_u64()
}

fn payload_bool(value: &serde_json::Value, key: &str) -> bool {
	value.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn requires_shell(input: &str) -> bool {
	input.chars().any(|c| {
		matches!(
			c,
			'|' | '<'
				| '>' | '$' | '`'
				| '(' | ')' | ';'
				| '&' | '*' | '?'
				| '[' | ']' | '{'
				| '}' | '"' | '\''
				| '\\' | '#' | '!'
		)
	})
}

fn compose_command(input: &str, capture_stdout: bool, stderr: Stdio) -> Option<Command> {
	let mut command = if requires_shell(input) {
		let mut c = Command::new("sh");
		c.args(["-c", input]);
		c
	} else {
		let parts = shlex::split(input)?;
		if parts.is_empty() {
			return None;
		}
		let mut c = Command::new(&parts[0]);
		c.args(&parts[1..]);
		c
	};
	if capture_stdout {
		command.stdout(Stdio::piped());
	}
	command.stderr(stderr);
	unsafe {
		command.pre_exec(|| {
			libc::setpgid(0, 0);
			Ok(())
		});
	}
	Some(command)
}

fn build_response(action: &str, status: &str, message: &str, data: Option<&str>) -> String {
	match serde_json::to_string(&Response {
		action,
		status,
		message,
		data,
	}) {
		Ok(json) => format!("if (window.onRunicResponse) window.onRunicResponse({json});"),
		Err(_) => String::new(),
	}
}

fn respond(sender: &Sender<String>, action: &str, status: &str, message: &str, data: Option<&str>) {
	let _ = sender.try_send(build_response(action, status, message, data));
}

fn update_file_state(
	state: &ApplicationState,
	path: &str,
	handle: Option<fs::File>,
	offset: u64,
	inode: u64,
	device: u64,
) {
	let mut watchers = state.watchers.lock();
	if let Some(Watcher {
		target: WatchTarget::File {
			offset: o,
			inode: i,
			device: d,
			handle: h,
		},
		..
	}) = watchers.get_mut(path)
	{
		*o = offset;
		*i = inode;
		*d = device;
		*h = handle;
	}
}

fn emit_file_deltas(path: &str, sender: &Sender<String>, state: &ApplicationState) {
	let (mut handle, mut offset, mut inode, mut device) = {
		let mut watchers = state.watchers.lock();
		let Some(watcher) = watchers.get_mut(path) else { return };
		let WatchTarget::File {
			offset,
			inode,
			device,
			handle,
		} = &mut watcher.target
		else {
			return;
		};
		(handle.take(), *offset, *inode, *device)
	};

	let Ok(metadata) = fs::metadata(path) else {
		update_file_state(state, path, handle, offset, inode, device);
		return;
	};
	let length = metadata.len();
	let (new_inode, new_device) = (metadata.ino(), metadata.dev());
	if new_inode != inode || new_device != device || length < offset {
		offset = 0;
		inode = new_inode;
		device = new_device;
		handle = None;
	}
	if length == offset {
		update_file_state(state, path, handle, offset, inode, device);
		return;
	}
	let Some(mut file) = handle.or_else(|| fs::File::open(path).ok()) else {
		return;
	};
	let _ = file.seek(SeekFrom::Start(offset));
	for line in BufReader::new(&file).lines().map_while(Result::ok) {
		if !line.is_empty() {
			respond(sender, "watch", "watch_data", path, Some(&line));
		}
	}
	update_file_state(state, path, Some(file), length, inode, device);
}

fn watch_directory(
	path: String,
	buffer: PathBuf,
	cancellation: gio::Cancellable,
	state: Arc<ApplicationState>,
	sender: Sender<String>,
) {
	let Ok(monitor) = gio::File::for_path(&path).monitor_directory(gio::FileMonitorFlags::NONE, Some(&cancellation))
	else {
		return;
	};
	monitor.connect_changed(move |_, _, _, _| {
		let current = modification_timestamp(&buffer);
		let changed = {
			let mut watchers = state.watchers.lock();
			match watchers.get_mut(&path).map(|w| &mut w.target) {
				Some(WatchTarget::Directory { mtime }) if *mtime != current => {
					*mtime = current;
					true
				}
				_ => false,
			}
		};
		if changed {
			respond(&sender, "watch", "watch_data", &path, Some("changed"));
		}
	});
}

fn watch_file(
	path: String,
	buffer: PathBuf,
	cancellation: gio::Cancellable,
	state: Arc<ApplicationState>,
	sender: Sender<String>,
) {
	let Some(parent) = buffer.parent().map(Path::to_path_buf) else {
		return;
	};
	let target_name = match buffer.file_name().and_then(|n| n.to_str()).map(String::from) {
		Some(n) => n,
		None => return,
	};
	let Ok(monitor) = gio::File::for_path(&parent).monitor_directory(gio::FileMonitorFlags::NONE, Some(&cancellation))
	else {
		return;
	};
	monitor.connect_changed(move |_, changed_file, _, event| {
		if !matches!(
			event,
			gio::FileMonitorEvent::Changed | gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::Created
		) {
			return;
		}
		let matches_target = changed_file
			.path()
			.and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
			.map(|name| name == target_name)
			.unwrap_or(false);
		if matches_target {
			emit_file_deltas(&path, &sender, &state);
		}
	});
}

fn halt_everything(state: &ApplicationState, cancel_reload: bool) {
	state.shutdown.store(true, Ordering::Relaxed);

	let pids: Vec<u32> = state.executions.lock().drain().collect();
	for pid in pids {
		unsafe {
			libc::killpg(pid as i32, libc::SIGTERM);
		}
	}

	let guards: Vec<ProcessGuard> = state.streams.lock().drain().map(|(_, g)| g).collect();
	for mut guard in guards {
		guard.terminate();
	}

	let watchers: Vec<Watcher> = state.watchers.lock().drain().map(|(_, w)| w).collect();
	for watcher in watchers {
		watcher.cancellable.cancel();
	}

	if cancel_reload && let Some(c) = state.reload_cancellation.lock().take() {
		c.cancel();
	}
}

fn dispatch(state: Arc<ApplicationState>, request: IncomingMessage, sender: Sender<String>) {
	let action = request.action.clone();
	let pool = state.pool.clone();
	let state_clone = state.clone();
	let sender_clone = sender.clone();
	if !pool.submit(move || handle_request(state_clone, request, sender_clone)) {
		respond(&sender, &action, "error", "queue full", None);
	}
}

fn handle_request(state: Arc<ApplicationState>, request: IncomingMessage, sender: Sender<String>) {
	match request.action.as_str() {
		"read" => handle_read(&request, &sender),
		"write" => handle_write(&request, &sender),
		"exec" => handle_exec(&state, &request, &sender),
		"stream" => handle_stream(&state, &request, &sender),
		"unstream" => handle_unstream(&state, &request, &sender),
		"watch" => handle_watch(state, &request, &sender),
		"unwatch" => handle_unwatch(&state, &request, &sender),
		"stats" => handle_stats(&state, &sender),
		_ => respond(&sender, &request.action, "error", "unknown action", None),
	}
}

fn handle_read(request: &IncomingMessage, sender: &Sender<String>) {
	let Some(path) = payload_string(&request.payload, "path") else {
		return respond(sender, "read", "error", "path", None);
	};
	match fs::read_to_string(path) {
		Ok(content) => {
			log::debug!("read '{}' => {} bytes", path, content.len());
			respond(sender, "read", "success", "OK", Some(&content));
		}
		Err(e) => {
			let msg = e.to_string();
			respond(sender, "read", "error", &msg, None);
		}
	}
}

fn handle_write(request: &IncomingMessage, sender: &Sender<String>) {
	let (Some(path), Some(data)) = (
		payload_string(&request.payload, "path"),
		payload_string(&request.payload, "data"),
	) else {
		return respond(sender, "write", "error", "path/data", None);
	};
	let append = payload_bool(&request.payload, "append");
	let p = Path::new(path);
	if let Some(parent) = p.parent()
		&& let Err(e) = fs::create_dir_all(parent)
	{
		let msg = format!("create_dir_all: {e}");
		return respond(sender, "write", "error", &msg, None);
	}
	let result = OpenOptions::new()
		.create(true)
		.write(true)
		.append(append)
		.truncate(!append)
		.open(p)
		.and_then(|mut f| f.write_all(data.as_bytes()).and_then(|_| f.flush()));
	match result {
		Ok(_) => respond(sender, "write", "success", "OK", None),
		Err(e) => {
			let msg = e.to_string();
			respond(sender, "write", "error", &msg, None);
		}
	}
}

fn handle_exec(state: &ApplicationState, request: &IncomingMessage, sender: &Sender<String>) {
	let Some(cmd) = payload_string(&request.payload, "command").map(str::to_string) else {
		return respond(sender, "exec", "error", "command", None);
	};
	let Some(mut c) = compose_command(&cmd, true, Stdio::piped()) else {
		return respond(sender, "exec", "error", "bad command", None);
	};
	match c.spawn() {
		Ok(child) => {
			let pid = child.id();
			state.executions.lock().insert(pid);
			let executions_ref = state.executions.clone();
			let sender_clone = sender.clone();
			thread::spawn(move || {
				let output = child.wait_with_output();
				executions_ref.lock().remove(&pid);
				match output {
					Ok(o) => {
						let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
						let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
						let data = if o.status.success() {
							stdout
						} else {
							format!("STDERR:\n{stderr}")
						};
						respond(&sender_clone, "exec", "success", &cmd, Some(&data));
					}
					Err(e) => {
						let msg = e.to_string();
						respond(&sender_clone, "exec", "error", &msg, None);
					}
				}
			});
		}
		Err(e) => {
			let msg = e.to_string();
			respond(sender, "exec", "error", &msg, None);
		}
	}
}

fn spawn_stderr_logger(stderr: std::process::ChildStderr, label: String) {
	thread::spawn(move || {
		for line in BufReader::new(stderr).lines().map_while(Result::ok) {
			log::warn!("[{label}] {line}");
		}
	});
}

fn handle_stream(state: &ApplicationState, request: &IncomingMessage, sender: &Sender<String>) {
	let Some(target) = payload_string(&request.payload, "target").map(str::to_string) else {
		return respond(sender, "stream", "error", "target", None);
	};
	if target.trim().is_empty() {
		return respond(sender, "stream", "error", "empty target", None);
	}

	let stderr_mode = if log::log_enabled!(log::Level::Debug) {
		Stdio::piped()
	} else {
		Stdio::null()
	};
	let Some(mut cmd) = compose_command(&target, true, stderr_mode) else {
		return respond(sender, "stream", "error", "bad target", None);
	};

	let mut streams = state.streams.lock();
	if streams.contains_key(&target) {
		drop(streams);
		return respond(sender, "stream", "success", "already running", None);
	}
	match cmd.spawn() {
		Ok(mut child) => {
			let pid = child.id();
			let Some(stdout) = child.stdout.take() else {
				let _ = child.kill();
				drop(streams);
				return respond(sender, "stream", "error", "stdout", None);
			};
			let stderr = child.stderr.take();
			streams.insert(target.clone(), child.into());
			drop(streams);
			respond(sender, "stream", "success", "started", Some(&pid.to_string()));

			if let Some(stderr) = stderr {
				spawn_stderr_logger(stderr, target.clone());
			}

			let shutdown = state.shutdown.clone();
			let streams_ref = state.streams.clone();
			let sender_clone = sender.clone();
			let target_clone = target.clone();
			thread::spawn(move || {
				for line in BufReader::new(stdout).lines() {
					if shutdown.load(Ordering::Relaxed) {
						break;
					}
					let Ok(content) = line else { break };
					if sender_clone
						.try_send(build_response("stream", "stream_data", &target_clone, Some(&content)))
						.is_err()
					{
						break;
					}
				}
				streams_ref.lock().remove(&target_clone);
			});
		}
		Err(e) => {
			drop(streams);
			let msg = e.to_string();
			respond(sender, "stream", "error", &msg, Some(&target));
		}
	}
}

fn handle_unstream(state: &ApplicationState, request: &IncomingMessage, sender: &Sender<String>) {
	let Some(target) = payload_string(&request.payload, "target") else {
		return respond(sender, "unstream", "error", "target", None);
	};
	if let Some(guard) = state.streams.lock().remove(target) {
		respond(sender, "unstream", "success", "stopped", None);
		drop(guard);
	} else {
		respond(sender, "unstream", "error", "not found", None);
	}
}

fn handle_watch(state: Arc<ApplicationState>, request: &IncomingMessage, sender: &Sender<String>) {
	let Some(path) = payload_string(&request.payload, "path").map(str::to_string) else {
		return respond(sender, "watch", "error", "path", None);
	};
	let buffer = PathBuf::from(&path);
	let tail_count = payload_u64(&request.payload, "tail");
	let cancellation = gio::Cancellable::new();
	{
		let mut watchers = state.watchers.lock();
		if watchers.contains_key(&path) {
			return respond(sender, "watch", "success", "already watched", None);
		}
		let target = if buffer.is_dir() {
			WatchTarget::Directory {
				mtime: modification_timestamp(&buffer),
			}
		} else {
			let meta = fs::metadata(&buffer).ok();
			WatchTarget::File {
				offset: meta.as_ref().map(|m| m.len()).unwrap_or(0),
				inode: meta.as_ref().map(|m| m.ino()).unwrap_or(0),
				device: meta.as_ref().map(|m| m.dev()).unwrap_or(0),
				handle: None,
			}
		};
		watchers.insert(
			path.clone(),
			Watcher {
				target,
				cancellable: cancellation.clone(),
			},
		);
	}
	if let Some(count) = tail_count
		&& !buffer.is_dir()
		&& buffer.exists()
	{
		let path_clone = path.clone();
		let sender_clone = sender.clone();
		thread::spawn(move || {
			let Ok(mut file) = fs::File::open(&path_clone) else {
				return;
			};
			let Ok(meta) = file.metadata() else { return };
			let length = meta.len();
			if length == 0 {
				return;
			}
			let to_read = length.min(TAIL_READ_LIMIT);
			let _ = file.seek(SeekFrom::End(-(to_read as i64)));
			let mut lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
			if to_read < length && !lines.is_empty() {
				lines.remove(0);
			}
			let n = usize::try_from(count).unwrap_or(MAX_TAIL_LINES).min(MAX_TAIL_LINES);
			for line in lines.iter().skip(lines.len().saturating_sub(n)) {
				respond(&sender_clone, "watch", "watch_data", &path_clone, Some(line));
			}
		});
	}
	if buffer.is_dir() {
		watch_directory(
			path.clone(),
			buffer.clone(),
			cancellation,
			state.clone(),
			sender.clone(),
		);
	} else {
		watch_file(
			path.clone(),
			buffer.clone(),
			cancellation,
			state.clone(),
			sender.clone(),
		);
	}
	respond(sender, "watch", "success", "watching", None);
}

fn handle_unwatch(state: &ApplicationState, request: &IncomingMessage, sender: &Sender<String>) {
	let Some(path) = payload_string(&request.payload, "path") else {
		return respond(sender, "unwatch", "error", "path", None);
	};
	if let Some(watcher) = state.watchers.lock().remove(path) {
		watcher.cancellable.cancel();
		respond(sender, "unwatch", "success", "stopped", None);
	} else {
		respond(sender, "unwatch", "error", "not watched", None);
	}
}

fn handle_stats(state: &ApplicationState, sender: &Sender<String>) {
	let data = format!(
		"streams={} watchers={} execs={} queue={}",
		state.streams.lock().len(),
		state.watchers.lock().len(),
		state.executions.lock().len(),
		state.pool.pending_count()
	);
	respond(sender, "stats", "success", "OK", Some(&data));
}

fn build_interface(
	configuration: &InterfaceConfiguration,
	url: &str,
	state: &Arc<ApplicationState>,
	sender: &Sender<String>,
	main_loop: &glib::MainLoop,
) -> (gtk4::Window, WebView) {
	let window = gtk4::Window::new();
	window.set_default_size(800, 600);
	let css = gtk4::CssProvider::new();
	css.load_from_string("window{background-color:rgba(0,0,0,0);}");
	if let Some(display) = gtk4::gdk::Display::default() {
		gtk4::style_context_add_provider_for_display(&display, &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
	}

	window.init_layer_shell();
	window.set_layer(Layer::Background);
	window.set_keyboard_mode(KeyboardMode::None);
	window.set_namespace(Some("runic"));
	match (configuration.width, configuration.height) {
		(Some(w), Some(h)) => {
			window.set_default_size(w, h);
			window.set_anchor(Edge::Top, true);
			window.set_anchor(Edge::Left, true);
			window.set_anchor(Edge::Right, false);
			window.set_anchor(Edge::Bottom, false);
			window.set_margin(Edge::Top, configuration.y_offset);
			window.set_margin(Edge::Left, configuration.x_offset);
		}
		_ => {
			window.set_anchor(Edge::Top, true);
			window.set_anchor(Edge::Left, true);
			window.set_anchor(Edge::Right, true);
			window.set_anchor(Edge::Bottom, true);
		}
	}
	window.set_exclusive_zone(0);

	let webview = WebView::new();
	if let Some(settings) = WebViewExt::settings(&webview) {
		settings.set_enable_webgl(false);
		settings.set_enable_webaudio(false);
		settings.set_enable_media_stream(false);
		settings.set_enable_mediasource(false);
		settings.set_enable_encrypted_media(false);
		settings.set_enable_smooth_scrolling(false);
	}

	let manager: UserContentManager = webview.user_content_manager().expect("user_content_manager");
	if !manager.register_script_message_handler("ipc", None) {
		log::error!("ipc registration failed");
	}

	let state_clone = state.clone();
	let sender_clone = sender.clone();
	manager.connect_script_message_received(Some("ipc"), move |_, message: &JsValue| {
		let raw_gstring = message.to_str();
		let raw = raw_gstring.as_str();
		log::debug!("ipc {raw}");
		if let Ok(req) = serde_json::from_str::<IncomingMessage>(raw) {
			dispatch(state_clone.clone(), req, sender_clone.clone());
		}
	});

	manager.add_script(&UserScript::new(
        r#"window.ipc={postMessage:function(m){try{if(window.webkit&&window.webkit.messageHandlers&&window.webkit.messageHandlers.ipc){window.webkit.messageHandlers.ipc.postMessage(m);}else{console.error('[runic] ipc unavailable');}}catch(e){console.error('[runic] ipc error',e);}}};"#,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));

	webview.load_uri(url);
	window.set_child(Some(&webview));

	let state_on_close = state.clone();
	let main_loop_for_close = main_loop.clone();
	window.connect_close_request(move |_| {
		halt_everything(&state_on_close, true);
		main_loop_for_close.quit();
		glib::Propagation::Stop
	});

	(window, webview)
}

fn main() {
	let arguments = Arguments::parse();
	env_logger::Builder::new()
		.filter_level(if arguments.debug {
			log::LevelFilter::Debug
		} else {
			log::LevelFilter::Info
		})
		.format_timestamp_millis()
		.init();

	let Some(html_path) = arguments.html_file_path else {
		let _ = Arguments::command().print_help();
		std::process::exit(0);
	};
	let absolute_path = match fs::canonicalize(&html_path) {
		Ok(p) => p,
		Err(e) => {
			log::error!("file '{html_path}': {e}");
			std::process::exit(1);
		}
	};
	let file_url = format!("file://{}", absolute_path.display());

	gtk4::init().expect("GTK initialization failed");
	let main_loop = glib::MainLoop::new(None, false);

	let pool = ThreadPool::spawn(WORKER_THREADS, TASK_QUEUE_SIZE);
	let state = ApplicationState::new(pool.clone());

	let (ipc_sender, ipc_receiver) = async_channel::bounded::<String>(IPC_CHANNEL_SIZE);
	let (event_sender, event_receiver) = async_channel::unbounded::<WidgetEvent>();
	let (shutdown_sender, shutdown_receiver) = async_channel::unbounded::<()>();

	let configuration = InterfaceConfiguration {
		width: arguments.width,
		height: arguments.height,
		x_offset: arguments.x_offset,
		y_offset: arguments.y_offset,
	};

	let window_slot: Rc<RefCell<Option<gtk4::Window>>> = Rc::new(RefCell::new(None));
	let webview_slot: Rc<RefCell<Option<WebView>>> = Rc::new(RefCell::new(None));

	let (window, webview) = build_interface(&configuration, &file_url, &state, &ipc_sender, &main_loop);
	window.present();
	*window_slot.borrow_mut() = Some(window);
	*webview_slot.borrow_mut() = Some(webview);

	let ipc_receiver_clone = ipc_receiver.clone();
	let webview_for_js = webview_slot.clone();
	glib::MainContext::default().spawn_local(async move {
		while let Ok(code) = ipc_receiver_clone.recv().await {
			if let Some(wv) = webview_for_js.borrow().clone() {
				wv.evaluate_javascript(&code, None::<&str>, None::<&str>, None::<&gio::Cancellable>, |_| {});
			}
		}
	});

	let window_for_events = window_slot.clone();
	let webview_for_events = webview_slot.clone();
	let state_for_events = state.clone();
	let sender_for_events = ipc_sender.clone();
	let url_for_events = file_url.clone();
	let configuration_for_events = configuration.clone();
	let main_loop_for_events = main_loop.clone();
	let event_receiver_clone = event_receiver.clone();
	glib::MainContext::default().spawn_local(async move {
		while let Ok(event) = event_receiver_clone.recv().await {
			match event {
				WidgetEvent::Stop(reason) => {
					log::info!("stop: {reason}");
					halt_everything(&state_for_events, false);
					state_for_events.shutdown.store(true, Ordering::Relaxed);
					*webview_for_events.borrow_mut() = None;
					if let Some(w) = window_for_events.borrow_mut().take() {
						w.destroy();
					}
				}
				WidgetEvent::Start(reason) => {
					log::info!("start: {reason}");
					halt_everything(&state_for_events, false);
					state_for_events.shutdown.store(false, Ordering::Relaxed);
					*webview_for_events.borrow_mut() = None;
					if let Some(w) = window_for_events.borrow_mut().take() {
						w.destroy();
					}
					let (new_window, new_webview) = build_interface(
						&configuration_for_events,
						&url_for_events,
						&state_for_events,
						&sender_for_events,
						&main_loop_for_events,
					);
					new_window.present();
					*window_for_events.borrow_mut() = Some(new_window);
					*webview_for_events.borrow_mut() = Some(new_webview);
				}
			}
		}
	});

	let state_for_shutdown = state.clone();
	let shutdown_receiver_clone = shutdown_receiver.clone();
	let main_loop_for_shutdown = main_loop.clone();
	glib::MainContext::default().spawn_local(async move {
		while shutdown_receiver_clone.recv().await.is_ok() {
			halt_everything(&state_for_shutdown, true);
			main_loop_for_shutdown.quit();
		}
	});

	let mut _monitor_keep_alive: Option<gio::FileMonitor> = None;
	if let Some(directory) = absolute_path.parent().map(Path::to_path_buf) {
		let (reload_sender, reload_receiver) = async_channel::bounded::<()>(1);
		let event_sender_clone = event_sender.clone();
		glib::MainContext::default().spawn_local(async move {
			while reload_receiver.recv().await.is_ok() {
				log::info!("hot reload");
				let _ = event_sender_clone.send(WidgetEvent::Stop("hot-reload".into())).await;
				glib::timeout_future(Duration::from_millis(200)).await;
				let _ = event_sender_clone.send(WidgetEvent::Start("hot-reload".into())).await;
			}
		});

		let cancellation = gio::Cancellable::new();
		*state.reload_cancellation.lock() = Some(cancellation.clone());
		if let Ok(monitor) =
			gio::File::for_path(&directory).monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, Some(&cancellation))
		{
			_monitor_keep_alive = Some(monitor.clone());
			let last_change = RefCell::new(Instant::now());
			let reload_sender_clone = reload_sender.clone();
			monitor.connect_changed(move |_, file, _, event| {
				log::debug!("FS_EVENT: {:?} | PATH: {:?}", event, file.path());
				if !matches!(
					event,
					gio::FileMonitorEvent::Changed
						| gio::FileMonitorEvent::ChangesDoneHint
						| gio::FileMonitorEvent::Created
						| gio::FileMonitorEvent::Deleted
						| gio::FileMonitorEvent::Moved
				) {
					return;
				}
				let Some(file_path) = file.path() else { return };
				let Some(name) = file_path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
					return;
				};
				let relevant = (name.ends_with(".html")
					|| name.ends_with(".css")
					|| name.ends_with(".js")
					|| name.ends_with(".mjs"))
					&& !name.ends_with('~')
					&& !name.contains(".swp")
					&& !name.contains(".tmp");
				if !relevant {
					return;
				}
				let now = Instant::now();
				let mut last = last_change.borrow_mut();
				if now.duration_since(*last) > Duration::from_millis(HOT_RELOAD_DEBOUNCE_MS) {
					*last = now;
					let _ = reload_sender_clone.try_send(());
				}
			});
		}
	}

	let shutdown_sender_clone = shutdown_sender.clone();
	thread::spawn(move || {
		let mut signals =
			signal_hook::iterator::Signals::new([libc::SIGTERM, libc::SIGINT]).expect("signal registration");
		for _ in signals.forever() {
			let _ = shutdown_sender_clone.send_blocking(());
		}
	});

	let dbus_sender = event_sender.clone();
	thread::spawn(move || {
		use zbus::Connection;
		use zbus::proxy;

		#[proxy(
			interface = "org.freedesktop.login1.Manager",
			default_service = "org.freedesktop.login1",
			default_path = "/org/freedesktop/login1"
		)]
		trait Login1Manager {
			#[zbus(signal)]
			async fn prepare_for_sleep(entering: bool);
		}

		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.expect("tokio runtime");
		runtime.block_on(async {
			let Ok(connection) = Connection::system().await else {
				log::debug!("dbus connection failed");
				return;
			};
			let Ok(proxy) = Login1ManagerProxy::new(&connection).await else {
				log::debug!("dbus proxy failed");
				return;
			};
			let Ok(mut stream) = proxy.receive_prepare_for_sleep().await else {
				log::debug!("dbus signal subscription failed");
				return;
			};
			log::debug!("PrepareForSleep subscribed");
			use futures_lite::StreamExt;
			while let Some(signal) = stream.next().await {
				let Ok(args) = signal.args() else { continue };
				let event = if args.entering {
					WidgetEvent::Stop("suspend".into())
				} else {
					WidgetEvent::Start("resume".into())
				};
				let _ = dbus_sender.send(event).await;
			}
		});
	});

	main_loop.run();
}
