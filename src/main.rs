use async_channel::Sender;
use clap::Parser;
use glib::Propagation;
use gtk::gio;
use gtk::prelude::*;
use gtk_layer_shell::{
    Edge, Layer, init_for_window, set_anchor, set_exclusive_zone, set_keyboard_interactivity,
    set_layer, set_margin, set_namespace,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{self, Command, Stdio};
use std::rc::Rc;
#[cfg(feature = "debug")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use webkit2gtk::{
    SettingsExt, UserContentInjectedFrames, UserContentManagerExt, UserScript,
    UserScriptInjectionTime, WebView, WebViewExt,
};

const IPC_CHANNEL_SIZE: usize = 1024;
const THREAD_POOL_SIZE: usize = 8;
const TAIL_BUF_LIMIT: u64 = 64 * 1024;

#[cfg(feature = "debug")]
const HOT_RELOAD_DEBOUNCE_MS: u64 = 300;
#[cfg(feature = "debug")]
const THREAD_STATS_INTERVAL_SECS: u64 = 30;
#[cfg(feature = "debug")]
const STREAM_STATS_BATCH: u64 = 10;

const RESET: &str = "\x1b[0m";
const RED: &str = "\x1b[31m";
#[cfg(feature = "debug")]
const GREEN: &str = "\x1b[32m";
#[cfg(feature = "debug")]
const YELLOW: &str = "\x1b[33m";
#[cfg(feature = "debug")]
const MAGENTA: &str = "\x1b[35m";
#[cfg(feature = "debug")]
const CYAN: &str = "\x1b[36m";
#[cfg(feature = "debug")]
const BOLD: &str = "\x1b[1m";

type Task = Box<dyn FnOnce() + Send + 'static>;

fn needs_shell(cmd: &str) -> bool {
    let s = cmd.trim();
    s.contains('|')
        || s.contains('<')
        || s.contains('>')
        || s.contains('$')
        || s.contains('`')
        || s.contains('(')
        || s.contains(';')
        || s.contains('&')
        || s.contains('*')
        || s.contains('?')
        || s.contains('[')
        || s.contains('{')
        || s.contains('"')
        || s.contains('\'')
        || s.contains('\\')
        || s.contains('#')
}

fn parse_command(cmd: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escape_next = false;

    for ch in cmd.chars() {
        if escape_next {
            current.push(ch);
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if in_double_quote => {
                escape_next = true;
                current.push(ch);
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    args.push(current.clone());
                    current.clear();
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

struct ThreadPool {
    sender: async_channel::Sender<Task>,
    _workers: Vec<thread::JoinHandle<()>>,
}

impl ThreadPool {
    fn new(size: usize) -> Self {
        let (sender, receiver) = async_channel::bounded::<Task>(THREAD_POOL_SIZE * 4);
        let mut workers = Vec::with_capacity(size);
        for _ in 0..size {
            let receiver = receiver.clone();
            workers.push(thread::spawn(move || {
                while let Ok(task) = receiver.recv_blocking() {
                    task();
                }
            }));
        }
        drop(receiver);
        Self {
            sender,
            _workers: workers,
        }
    }
    fn execute<F: FnOnce() + Send + 'static>(&self, f: F) {
        let _ = self.sender.send_blocking(Box::new(f));
    }
}

#[derive(Parser, Debug)]
#[command(name = "runic", about = "HTML-виджет для рабочего стола")]
struct Args {
    #[arg(short = 'w', long, help = "Ширина окна виджета (в пикселях)")]
    width: Option<i32>,
    #[arg(short = 'H', long, help = "Высота окна виджета (в пикселях)")]
    height: Option<i32>,
    #[arg(
        short = 'x',
        long,
        default_value = "0",
        help = "Отступ от левого края экрана"
    )]
    x: i32,
    #[arg(
        short = 'y',
        long,
        default_value = "0",
        help = "Отступ от верхнего края экрана"
    )]
    y: i32,
    #[arg(
        short = 'd',
        long,
        help = "Включить режим отладки (hot-reload, вывод в консоль)"
    )]
    debug: bool,
    #[arg(help = "Путь к HTML-файлу виджета", value_name = "FILE")]
    html_path: Option<String>,
}

#[derive(Clone)]
struct UiConfig {
    width: Option<i32>,
    height: Option<i32>,
    x: i32,
    y: i32,
    #[cfg(feature = "debug")]
    is_debug: bool,
}

#[derive(Deserialize)]
struct JsMessage {
    action: String,
    payload: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
struct RustResponse {
    action: String,
    status: String,
    message: String,
    data: Option<String>,
}

#[derive(Serialize)]
struct CompactResponse<'a> {
    action: &'a str,
    status: &'a str,
    message: &'a str,
    data: Option<&'a str>,
}

enum WatchState {
    File(Option<u64>, Option<fs::File>),
    Dir(Option<u64>),
}

fn send_new_lines(
    path: &str,
    sender: &Sender<String>,
    state: &AppState,
) -> bool {
    let path_buf = Path::new(path);
    let meta = match fs::metadata(path_buf) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let file_len = meta.len();

    let (current_pos, file_opt) = {
        let mut positions = state.watch_positions.lock().unwrap();
        if let Some(WatchState::File(pos, file_opt)) = positions.get_mut(path) {
            (*pos, file_opt.take())
        } else {
            (None, None)
        }
    };

    let start_pos = match current_pos {
        Some(pos) if file_len >= pos => pos,
        _ => 0,
    };

    if file_len == 0 {
        if let Some(f) = file_opt {
            let mut positions = state.watch_positions.lock().unwrap();
            positions.insert(path.to_string(), WatchState::File(Some(0), Some(f)));
        }
        return true;
    }

    let mut file = match file_opt.or_else(|| fs::File::open(path_buf).ok()) {
        Some(f) => f,
        None => return false,
    };

    if file.seek(SeekFrom::Start(start_pos)).is_ok() {
        let reader = BufReader::new(&file);
        for l in reader.lines().map_while(Result::ok) {
            if !l.is_empty() {
                let js = build_js_callback(&CompactResponse {
                    action: "watch",
                    status: "watch_data",
                    message: path,
                    data: Some(&l),
                });
                let _ = sender.try_send(js);
            }
        }
    }

    let mut positions = state.watch_positions.lock().unwrap();
    if let Some(WatchState::File(pos_opt, _)) = positions.get_mut(path) {
        *pos_opt = Some(file_len);
    }
    if let Some(WatchState::File(_, cached)) = positions.get_mut(path) {
        *cached = Some(file);
    }

    true
}

enum WidgetEvent {
    Stop(String),
    Start(String),
}

#[cfg(feature = "debug")]
#[derive(Debug, Clone)]
struct ThreadStats {
    id: u64,
    thread_type: String,
    name: String,
    created_at: Instant,
    finished_at: Option<Instant>,
    peak_memory_kb: u64,
    cpu_time_ms: u128,
    tasks_completed: u64,
}

#[cfg(feature = "debug")]
struct ThreadTracker {
    threads: Mutex<HashMap<u64, ThreadStats>>,
    next_id: AtomicU64,
    total_created: AtomicU64,
    total_finished: AtomicU64,
}

#[cfg(feature = "debug")]
impl ThreadTracker {
    fn new() -> Self {
        Self {
            threads: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            total_created: AtomicU64::new(0),
            total_finished: AtomicU64::new(0),
        }
    }
    fn register_thread(&self, thread_type: &str, name: &str) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let stats = ThreadStats {
            id,
            thread_type: thread_type.to_string(),
            name: name.to_string(),
            created_at: Instant::now(),
            finished_at: None,
            peak_memory_kb: 0,
            cpu_time_ms: 0,
            tasks_completed: 0,
        };
        self.threads.lock().unwrap().insert(id, stats);
        self.total_created.fetch_add(1, Ordering::Relaxed);
        id
    }
    fn finish_thread(&self, id: u64, final_stats: &ResourceStats) {
        if let Some(stats) = self.threads.lock().unwrap().get_mut(&id) {
            stats.finished_at = Some(Instant::now());
            stats.peak_memory_kb = final_stats.memory_kb;
            stats.cpu_time_ms = final_stats.cpu_time_ms;
        }
        self.total_finished.fetch_add(1, Ordering::Relaxed);
    }
    fn update_thread_statistics(&self, id: u64, stats: &ResourceStats, tasks: u64) {
        if let Some(ts) = self.threads.lock().unwrap().get_mut(&id) {
            ts.peak_memory_kb = ts.peak_memory_kb.max(stats.memory_kb);
            ts.cpu_time_ms = stats.cpu_time_ms;
            ts.tasks_completed += tasks;
        }
    }
    fn get_summary(&self) -> String {
        let threads = self.threads.lock().unwrap();
        let active = threads.values().filter(|t| t.finished_at.is_none()).count();
        let finished = threads.values().filter(|t| t.finished_at.is_some()).count();
        let total_created = self.total_created.load(Ordering::Relaxed);
        let total_finished = self.total_finished.load(Ordering::Relaxed);
        let mut summary = format!("{}=== СТАТИСТИКА ПОТОКОВ ==={}\n", BOLD, RESET);
        summary += &format!(
            "Создано всего: {}\nЗавершено: {}\nАктивных: {}\nЗавершённых в истории: {}\n\n",
            total_created, total_finished, active, finished
        );
        if active > 0 {
            summary += &format!("{}Активные потоки:{}\n", GREEN, RESET);
            for stats in threads.values().filter(|t| t.finished_at.is_none()) {
                summary += &format!(
                    "  [{}] {} ({}) - создан {:?} назад\n",
                    stats.id,
                    stats.name,
                    stats.thread_type,
                    stats.created_at.elapsed()
                );
            }
        }
        summary += &format!("\n{}Последние 5 завершённых:{}\n", YELLOW, RESET);
        let mut ft: Vec<_> = threads
            .values()
            .filter(|t| t.finished_at.is_some())
            .collect();
        ft.sort_by_key(|a| std::cmp::Reverse(a.finished_at));
        for stats in ft.iter().take(5) {
            let duration = stats.finished_at.unwrap().duration_since(stats.created_at);
            summary += &format!(
                "  [{}] {} ({}) - работал {:?}, пик памяти: {} KB, CPU: {} ms, задач: {}\n",
                stats.id,
                stats.name,
                stats.thread_type,
                duration,
                stats.peak_memory_kb,
                stats.cpu_time_ms,
                stats.tasks_completed
            );
        }
        summary
    }
}

pub struct ChildGuard(pub std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
use std::ops::{Deref, DerefMut};
impl Deref for ChildGuard {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

struct AppState {
    running_streams: Mutex<HashMap<String, ChildGuard>>,
    watch_positions: Mutex<HashMap<String, WatchState>>,
    #[cfg(feature = "debug")]
    is_debug: bool,
    shutdown: Arc<AtomicBool>,
    #[cfg(feature = "debug")]
    thread_tracker: Arc<ThreadTracker>,
    pool: Arc<ThreadPool>,
}

#[cfg(feature = "debug")]
#[derive(Copy, Clone)]
struct ResourceStats {
    memory_kb: u64,
    cpu_time_ms: u128,
}

#[cfg(feature = "debug")]
fn get_resource_stats() -> ResourceStats {
    let mut memory_kb = 0;
    let mut cpu_time_ms = 0;
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                if let Some(kb_str) = line.split_whitespace().nth(1) {
                    memory_kb = kb_str.parse().unwrap_or(0);
                }
                break;
            }
        }
    }
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
        let user_ms =
            (usage.ru_utime.tv_sec as u128 * 1000) + (usage.ru_utime.tv_usec as u128 / 1000);
        let sys_ms =
            (usage.ru_stime.tv_sec as u128 * 1000) + (usage.ru_stime.tv_usec as u128 / 1000);
        cpu_time_ms = user_ms + sys_ms;
    }
    ResourceStats {
        memory_kb,
        cpu_time_ms,
    }
}

#[cfg(feature = "debug")]
fn log_profile(action: &str, duration: Duration, stats: &ResourceStats) {
    if duration > Duration::from_millis(100) {
        eprintln!(
            "{}[Руник Профиль]{} {} '{}' выполнен за {:?} | Память: {} KB | CPU: {} ms",
            MAGENTA, RESET, BOLD, action, duration, stats.memory_kb, stats.cpu_time_ms
        );
    }
}

fn cleanup_children(state: &Arc<AppState>) {
    #[cfg(feature = "debug")]
    if state.is_debug {
        eprintln!(
            "{}[Руник ОТЛАДКА]{} {}=== НАЧАЛО ОЧИСТКИ ПРОЦЕССОВ ==={}",
            CYAN, RESET, BOLD, RESET
        );
    }
    state.shutdown.store(true, Ordering::Relaxed);
    let pids: Vec<u32> = {
        let mut streams = state.running_streams.lock().unwrap();
        let pids: Vec<u32> = streams.values().map(|c| c.id()).collect();
        streams.clear();
        pids
    };
    if pids.is_empty() {
        #[cfg(feature = "debug")]
        if state.is_debug {
            eprintln!("{}[Руник ОТЛАДКА]{} Нечего очищать.", CYAN, RESET);
        }
        return;
    }
    #[cfg(feature = "debug")]
    if state.is_debug {
        for &pid in &pids {
            eprintln!(
                "{}[Руник ОТЛАДКА]{} Отправка SIGTERM группе процессов -{}",
                CYAN, RESET, pid
            );
        }
    }
    for &pid in &pids {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
    let timeout = Duration::from_secs(5);
    let start = Instant::now();
    while start.elapsed() < timeout {
        let mut status: libc::c_int = 0;
        let ret = unsafe {
            libc::waitpid(-1, &mut status, libc::WNOHANG)
        };
        if ret <= 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    for &pid in &pids {
        let mut status: libc::c_int = 0;
        let ret = unsafe {
            libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG)
        };
        if ret == 0 {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
                libc::waitpid(pid as libc::pid_t, &mut status, 0);
            }
        }
    }
    #[cfg(feature = "debug")]
    if state.is_debug {
        eprintln!(
            "{}[Руник ОТЛАДКА]{} {}=== ОЧИСТКА ПРОЦЕССОВ ЗАВЕРШЕНА ==={}",
            CYAN, RESET, BOLD, RESET
        );
    }
}

fn kill_and_clear_tracked(state: &Arc<AppState>) {
    #[cfg(feature = "debug")]
    if state.is_debug {
        eprintln!(
            "{}[Руник ОТЛАДКА]{} {}=== ПРИНУДИТЕЛЬНАЯ ОЧИСТКА ==={}",
            CYAN, RESET, BOLD, RESET
        );
    }
    state.shutdown.store(true, Ordering::Relaxed);
    {
        let mut streams = state.running_streams.lock().unwrap();
        streams.drain();
    }
    #[cfg(feature = "debug")]
    if state.is_debug {
        eprintln!(
            "{}[Руник ОТЛАДКА]{} {}=== ПРИНУДИТЕЛЬНАЯ ОЧИСТКА ЗАВЕРШЕНА ==={}",
            CYAN, RESET, BOLD, RESET
        );
    }
}

fn build_js_callback(response: &CompactResponse) -> String {
    let mut js = String::with_capacity(128);
    js.push_str("if (window.onRunicResponse) window.onRunicResponse(");
    if let Ok(json) = serde_json::to_string(response) {
        js.push_str(&json);
    }
    js.push_str(");");
    js
}

fn handle_action(state: Arc<AppState>, request: JsMessage, js_sender: Sender<String>) {
    #[cfg(feature = "debug")]
    let is_debug = state.is_debug;
    #[cfg(feature = "debug")]
    let start_time = Instant::now();
    #[cfg(feature = "debug")]
    let start_stats = if state.is_debug {
        Some(get_resource_stats())
    } else {
        None
    };
    let pool = Arc::clone(&state.pool);
    pool.execute(move || {
        let send_js = |action: &str, status: &str, message: &str, data: Option<String>| {
            let mut js = String::with_capacity(128);
            js.push_str("if (window.onRunicResponse) window.onRunicResponse({\"action\":\"");
            js.push_str(action);
            js.push_str("\",\"status\":\"");
            js.push_str(status);
            js.push_str("\",\"message\":\"");
            js.push_str(message);
            js.push_str("\",");
            if let Some(d) = data {
                js.push_str("\"data\":");
                js.push_str(&serde_json::to_string(&d).unwrap_or_else(|_| "null".to_string()));
            } else {
                js.push_str("\"data\":null");
            }
            js.push_str("});");
            let _ = js_sender.try_send(js);
        };
        match request.action.as_str() {
            "read" => {
                if let Some(path) = request.payload.get("path").and_then(|v| v.as_str()) {
                    #[cfg(feature = "debug")]
                    let action_start = Instant::now();
                    match fs::read_to_string(path) {
                        Ok(content) => {
                            #[cfg(feature = "debug")]
                            if is_debug {
                                let preview: String = content.chars().take(100).collect();
                                eprintln!(
                                    "{}[Руник РЕЗУЛЬТАТ]{} read '{}' => OK, {} байт, {:?}, содержимое: '{}{}'",
                                    GREEN, RESET, path, content.len(), action_start.elapsed(),
                                    preview, if content.len() > 100 { "..." } else { "" }
                                );
                            }
                            send_js("read", "success", "OK", Some(content));
                        }
                        Err(e) => {
                            #[cfg(feature = "debug")]
                            if is_debug {
                                eprintln!("{}[Руник РЕЗУЛЬТАТ]{} read '{}' => ОШИБКА: {}", RED, RESET, path, e);
                            }
                            send_js("read", "error", &format!("{}", e), None);
                        }
                    }
                } else {
                    #[cfg(feature = "debug")]
                    if is_debug {
                        eprintln!("{}[Руник РЕЗУЛЬТАТ]{} read => не указан путь", YELLOW, RESET);
                    }
                }
            }
            "write" => {
                let path = request.payload.get("path").and_then(|v| v.as_str());
                let data = request.payload.get("data").and_then(|v| v.as_str());
                if let (Some(p), Some(d)) = (path, data) {
                    #[cfg(feature = "debug")]
                    let action_start = Instant::now();
                    let path = Path::new(p);
                    let append = request
                        .payload
                        .get("append")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if let Some(parent) = path.parent() {
                        if let Err(e) = fs::create_dir_all(parent) {
                            #[cfg(feature = "debug")]
                            if is_debug {
                                eprintln!(
                                    "{}[Руник РЕЗУЛЬТАТ]{} write '{}' => ОШИБКА create_dir_all: {}",
                                    RED, RESET, p, e
                                );
                            }
                            return send_js(
                                "write",
                                "error",
                                &format!("create_dir_all: {}", e),
                                None,
                            );
                        }
                    }
                    match OpenOptions::new()
                        .create(true)
                        .write(true)
                        .append(append)
                        .truncate(!append)
                        .open(path)
                    {
                        Ok(mut file) => {
                            match file.write_all(d.as_bytes()).and_then(|_| file.flush()) {
                                Ok(_) => {
                                    #[cfg(feature = "debug")]
                                    if is_debug {
                                        let mode = if append { "добавление" } else { "перезапись" };
                                        eprintln!(
                                            "{}[Руник РЕЗУЛЬТАТ]{} write '{}' => OK, {} байт, режим: {}, {:?}",
                                            GREEN, RESET, p, d.len(), mode, action_start.elapsed()
                                        );
                                    }
                                    send_js("write", "success", "OK", None);
                                }
                                Err(e) => {
                                    #[cfg(feature = "debug")]
                                    if is_debug {
                                        eprintln!(
                                            "{}[Руник РЕЗУЛЬТАТ]{} write '{}' => ОШИБКА записи: {}",
                                            RED, RESET, p, e
                                        );
                                    }
                                    send_js("write", "error", &format!("{}", e), None);
                                }
                            }
                        }
                        Err(e) => {
                            #[cfg(feature = "debug")]
                            if is_debug {
                                eprintln!(
                                    "{}[Руник РЕЗУЛЬТАТ]{} write '{}' => ОШИБКА открытия: {}",
                                    RED, RESET, p, e
                                );
                            }
                            send_js("write", "error", &format!("{}", e), None);
                        }
                    }
                } else {
                    #[cfg(feature = "debug")]
                    if is_debug {
                        eprintln!(
                            "{}[Руник РЕЗУЛЬТАТ]{} write => не указан путь или данные",
                            YELLOW, RESET
                        );
                    }
                }
            }
            "exec" => {
                if let Some(cmd) = request.payload.get("command").and_then(|v| v.as_str()) {
                    let cmd_clone = cmd.to_string();
                    let sender_clone = js_sender.clone();
                    #[cfg(feature = "debug")]
                    let tracker = Arc::clone(&state.thread_tracker);
                    #[cfg(feature = "debug")]
                    let is_debug_exec = state.is_debug;
                    #[cfg(feature = "debug")]
                    let thread_id = tracker.register_thread("exec", &format!("exec-{}", cmd_clone));
                    thread::spawn(move || {
                        #[cfg(feature = "debug")]
                        let exec_start = Instant::now();
                        let mut cmd_obj = if needs_shell(&cmd_clone) {
                            let mut cmd_obj = Command::new("sh");
                            cmd_obj.arg("-c").arg(&cmd_clone);
                            cmd_obj
                        } else {
                            let parts = parse_command(&cmd_clone);
                            if parts.is_empty() {
                                #[cfg(feature = "debug")]
                                if is_debug_exec {
                                    eprintln!("{}[Руник РЕЗУЛЬТАТ]{} exec '{}' => пустая команда", RED, RESET, cmd_clone);
                                }
                                let response = RustResponse {
                                    action: "exec".to_string(),
                                    status: "error".to_string(),
                                    message: "пустая команда".to_string(),
                                    data: None,
                                };
                                if let Ok(json) = serde_json::to_string(&response) {
                                    let _ = sender_clone.try_send(format!("if (window.onRunicResponse) window.onRunicResponse({});", json));
                                }
                                return;
                            }
                            let mut cmd_obj = Command::new(&parts[0]);
                            for arg in &parts[1..] {
                                cmd_obj.arg(arg);
                            }
                            cmd_obj
                        };
                        unsafe {
                            cmd_obj.pre_exec(|| {
                                libc::setpgid(0, 0);
                                Ok(())
                            });
                        }
                        #[cfg(feature = "debug")]
                        if is_debug_exec {
                            eprintln!("{}[Руник ОТЛАДКА]{} exec запуск: '{}'", CYAN, RESET, cmd_clone);
                        }
                        match cmd_obj.output() {
                            Ok(out) => {
                                let stdout = String::from_utf8(out.stdout).unwrap_or_else(|e| {
                                    String::from_utf8_lossy(e.as_bytes()).into_owned()
                                });
                                let stderr = String::from_utf8(out.stderr).unwrap_or_else(|e| {
                                    String::from_utf8_lossy(e.as_bytes()).into_owned()
                                });
                                let result = if out.status.success() {
                                    stdout.clone()
                                } else {
                                    format!("STDERR:\n{}", stderr)
                                };
                                #[cfg(feature = "debug")]
                                if is_debug_exec {
                                    let exec_stats = get_resource_stats();
                                    let preview: String = result.chars().take(200).collect();
                                    eprintln!(
                                        "{}[Руник РЕЗУЛЬТАТ]{} exec '{}' => код выхода: {:?}, {:?}, вывод: '{}{}'",
                                        GREEN, RESET, cmd_clone,
                                        out.status.code(),
                                        exec_start.elapsed(),
                                        preview,
                                        if result.len() > 200 { "..." } else { "" }
                                    );
                                    log_profile("exec", exec_start.elapsed(), &exec_stats);
                                    let final_stats = get_resource_stats();
                                    tracker.finish_thread(thread_id, &final_stats);
                                }
                                let response = RustResponse {
                                    action: "exec".to_string(),
                                    status: "success".to_string(),
                                    message: format!("OK: {}", cmd_clone),
                                    data: Some(result),
                                };
                                if let Ok(json) = serde_json::to_string(&response) {
                                    let _ = sender_clone.try_send(format!("if (window.onRunicResponse) window.onRunicResponse({});", json));
                                }
                            }
                            Err(e) => {
                                #[cfg(feature = "debug")]
                                if is_debug_exec {
                                    eprintln!(
                                        "{}[Руник РЕЗУЛЬТАТ]{} exec '{}' => ОШИБКА запуска: {}",
                                        RED, RESET, cmd_clone, e
                                    );
                                }
                                let response = RustResponse {
                                    action: "exec".to_string(),
                                    status: "error".to_string(),
                                    message: format!("{}", e),
                                    data: None,
                                };
                                if let Ok(json) = serde_json::to_string(&response) {
                                    let _ = sender_clone.try_send(format!("if (window.onRunicResponse) window.onRunicResponse({});", json));
                                }
                            }
                        }
                    });
                } else {
                    #[cfg(feature = "debug")]
                    if is_debug {
                        eprintln!(
                            "{}[Руник РЕЗУЛЬТАТ]{} exec => не указана команда",
                            YELLOW, RESET
                        );
                    }
                }
            }
            "stream" => {
                let target = request
                    .payload
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if target.is_empty() {
                    #[cfg(feature = "debug")]
                    if is_debug {
                        eprintln!(
                            "{}[Руник РЕЗУЛЬТАТ]{} stream => не указан target",
                            YELLOW, RESET
                        );
                    }
                    send_js("stream", "error", "target не указан", None);
                    return;
                }
                let target_clone = target.clone();
                let state_clone = Arc::clone(&state);
                let sender_clone = js_sender.clone();
                let mut cmd_obj = if needs_shell(&target_clone) {
                    let mut cmd_obj = Command::new("sh");
                    cmd_obj
                        .arg("-c")
                        .arg(&target_clone)
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped());
                    cmd_obj
                } else {
                    let parts = parse_command(&target_clone);
                    if parts.is_empty() {
                        #[cfg(feature = "debug")]
                        if is_debug {
                            eprintln!(
                                "{}[Руник РЕЗУЛЬТАТ]{} stream '{}' => пустая команда",
                                RED, RESET, target_clone
                            );
                        }
                        send_js("stream", "error", "пустая команда", None);
                        return;
                    }
                    let mut cmd_obj = Command::new(&parts[0]);
                    for arg in &parts[1..] {
                        cmd_obj.arg(arg);
                    }
                    cmd_obj.stdout(Stdio::piped()).stderr(Stdio::piped());
                    cmd_obj
                };
                unsafe {
                    cmd_obj.pre_exec(|| {
                        libc::setpgid(0, 0);
                        Ok(())
                    });
                }
                let mut running = state.running_streams.lock().unwrap();
                if running.contains_key(&target) {
                    #[cfg(feature = "debug")]
                    if is_debug {
                        eprintln!(
                            "{}[Руник РЕЗУЛЬТАТ]{} stream '{}' => уже выполняется",
                            YELLOW, RESET, target
                        );
                    }
                    send_js("stream", "success", "Уже выполняется (игнорируется)", None);
                    return;
                }
                match cmd_obj.spawn() {
                    Ok(mut proc) => {
                        #[cfg(feature = "debug")]
                        let thread_id = state_clone.thread_tracker.register_thread(
                            "stream",
                            &format!("stream-{}", target_clone),
                        );
                        #[cfg(feature = "debug")]
                        if state_clone.is_debug {
                            eprintln!(
                                "{}[Руник РЕЗУЛЬТАТ]{} stream '{}' => запущен, PID={} [Поток #{}]",
                                GREEN, RESET, target_clone, proc.id(), thread_id
                            );
                        }
                        let stdout = proc.stdout.take().unwrap();
                        let stderr = proc.stderr.take();
                        running.insert(target_clone.clone(), ChildGuard(proc));
                        drop(running);
                        send_js("stream", "success", "Запущен", None);
                        if let Some(stderr_pipe) = stderr {
                            #[cfg(feature = "debug")]
                            let target_for_stderr = target_clone.clone();
                            #[cfg(feature = "debug")]
                            let is_debug_stderr = state_clone.is_debug;
                            thread::spawn(move || {
                                let reader = BufReader::new(stderr_pipe);
                                for _l in reader.lines().map_while(Result::ok) {
                                    #[cfg(feature = "debug")]
                                    if is_debug_stderr {
                                        eprintln!(
                                            "{}[Руник STDERR]{}[{}] {}",
                                            RED, RESET, target_for_stderr, _l
                                        );
                                    }
                                }
                            });
                        }
                        #[cfg(feature = "debug")]
                        let tracker = Arc::clone(&state_clone.thread_tracker);
                        #[cfg(feature = "debug")]
                        let is_debug_stream = state_clone.is_debug;
                        #[cfg(feature = "debug")]
                        let target_for_stdout = target_clone.clone();
                        thread::spawn(move || {
                            let reader = BufReader::new(stdout);
                            #[cfg(feature = "debug")]
                            let mut line_count = 0;
                            for line in reader.lines() {
                                match line {
                                    Ok(l) => {
                                        #[cfg(feature = "debug")]
                                        {
                                            line_count += 1;
                                            if is_debug_stream {
                                                eprintln!(
                                                    "{}[Руник STDOUT]{}[{}] {}",
                                                    GREEN, RESET, target_for_stdout, l
                                                );
                                            }
                                        }
                                        let js = build_js_callback(&CompactResponse {
                                            action: "stream",
                                            status: "stream_data",
                                            message: &target_clone,
                                            data: Some(&l),
                                        });
                                        let _ = sender_clone.send_blocking(js);
                                        #[cfg(feature = "debug")]
                                        if is_debug_stream && line_count % STREAM_STATS_BATCH == 0 {
                                            let stats = get_resource_stats();
                                            tracker.update_thread_statistics(
                                                thread_id,
                                                &stats,
                                                STREAM_STATS_BATCH,
                                            );
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            #[cfg(feature = "debug")]
                            if is_debug_stream {
                                let final_stats = get_resource_stats();
                                tracker.finish_thread(thread_id, &final_stats);
                                eprintln!(
                                    "{}[Руник РЕЗУЛЬТАТ]{} stream '{}' => завершён, прочитано строк: {}",
                                    CYAN, RESET, target_for_stdout, line_count
                                );
                            }
                            if let Some(child_guard) =
                                state_clone.running_streams.lock().unwrap().remove(&target_clone)
                            {
                                #[cfg(feature = "debug")]
                                if is_debug_stream {
                                    eprintln!(
                                        "{}[Руник РЕЗУЛЬТАТ]{} stream '{}' => завершён, PID={}, прочитано строк: {}",
                                        CYAN, RESET, target_for_stdout, child_guard.id(), line_count
                                    );
                                }
                                let _ = child_guard;
                            }
                        });
                    }
                    Err(e) => {
                        drop(running);
                        #[cfg(feature = "debug")]
                        if is_debug {
                            eprintln!(
                                "{}[Руник РЕЗУЛЬТАТ]{} stream '{}' => ОШИБКА запуска: {}",
                                RED, RESET, target_clone, e
                            );
                        }
                        send_js(
                            "stream",
                            "error",
                            &format!("ОШИБКА ВЫПОЛНЕНИЯ: {}", e),
                            Some(target_clone),
                        );
                    }
                }
            }
            "unstream" => {
                let target = request
                    .payload
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let mut running = state.running_streams.lock().unwrap();
                if let Some(child_guard) = running.remove(target) {
                    let _pid = child_guard.id();
                    #[cfg(feature = "debug")]
                    if is_debug {
                        eprintln!(
                            "{}[Руник РЕЗУЛЬТАТ]{} unstream '{}' => остановлен, PID={}",
                            GREEN, RESET, target, _pid
                        );
                    }
                    send_js("unstream", "success", "Остановлен", None);
                    drop(child_guard);
                } else {
                    #[cfg(feature = "debug")]
                    if is_debug {
                        eprintln!(
                            "{}[Руник РЕЗУЛЬТАТ]{} unstream '{}' => не найден",
                            YELLOW, RESET, target
                        );
                    }
                    send_js("unstream", "error", "Не найден", None);
                }
            }
            "watch" => {
                let path = request.payload.get("path").and_then(|v| v.as_str());
                let tail = request.payload.get("tail").and_then(|v| v.as_u64());
                if let Some(p) = path {
                    let path_str = p.to_string();
                    let path_buf = std::path::PathBuf::from(&path_str);
                    {
                        let mut positions = state.watch_positions.lock().unwrap();
                        if positions.contains_key(&path_str) {
                            #[cfg(feature = "debug")]
                            if is_debug {
                                eprintln!(
                                    "{}[Руник РЕЗУЛЬТАТ]{} watch '{}' => уже мониторится",
                                    YELLOW, RESET, path_str
                                );
                            }
                            send_js("watch", "success", "Уже мониторится (игнорируется)", None);
                            return;
                        }
                        let initial_state = if path_buf.is_dir() {
                            WatchState::Dir(path_buf.metadata().ok().and_then(|m| {
                                m.modified()
                                    .ok()
                                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                    .map(|d| d.as_secs())
                            }))
                         } else {
                             WatchState::File(
                                 if path_buf.exists() {
                                     path_buf.metadata().ok().map(|m| m.len())
                                 } else {
                                     None
                                 },
                                 None,
                             )
                         };
                        positions.insert(path_str.clone(), initial_state);
                    }
                    #[cfg(feature = "debug")]
                    if is_debug {
                        let mode = if path_buf.is_dir() { "директория" } else { "файл" };
                        eprintln!(
                            "{}[Руник РЕЗУЛЬТАТ]{} watch '{}' => мониторинг запущен, тип: {}",
                            GREEN, RESET, path_str, mode
                        );
                    }
                    if let Some(n) = tail {
                        if !path_buf.is_dir() && path_buf.exists() {
                            let path_clone = path_str.clone();
                            let sender_clone = js_sender.clone();
                            thread::spawn(move || {
                                if let Ok(mut file) = fs::File::open(&path_clone) {
                                    if let Ok(meta) = file.metadata() {
                                        let file_len = meta.len();
                                        if file_len > 0 {
                                            let read_size = std::cmp::min(file_len, TAIL_BUF_LIMIT);
                                            let _ = file.seek(SeekFrom::End(-(read_size as i64)));
                                            let reader = BufReader::new(file);
                                            let lines: Vec<String> =
                                                reader.lines().map_while(Result::ok).collect();
                                            let start = if lines.len() > n as usize {
                                                lines.len() - n as usize
                                            } else {
                                                0
                                            };
                                            for line in lines.iter().skip(start) {
                                                let js = build_js_callback(&CompactResponse {
                                                    action: "watch",
                                                    status: "watch_data",
                                                    message: &path_clone,
                                                    data: Some(line.as_str()),
                                                });
                                                let _ = sender_clone.try_send(js);
                                            }
                                        }
                                    }
                                }
                            });
                        }
                    }
                    let path_clone = path_str.clone();
                    let state_clone = Arc::clone(&state);
                    let sender_clone = js_sender.clone();
                    let is_dir = path_buf.is_dir();
                    let path_buf_clone = path_buf.clone();

                    if is_dir {
                        let file = gio::File::for_path(&path_clone);
                        if let Ok(monitor) = file.monitor_directory(
                            gio::FileMonitorFlags::NONE,
                            None::<&gio::Cancellable>,
                        ) {
                            monitor.connect_changed(move |_, _, _, _| {
                                {
                                    let positions = state_clone.watch_positions.lock().unwrap();
                                    if !positions.contains_key(&path_clone) {
                                        return;
                                    }
                                }
                                let current_mod = path_buf_clone.metadata().ok().and_then(|m| {
                                    m.modified()
                                        .ok()
                                        .and_then(|t| {
                                            t.duration_since(std::time::UNIX_EPOCH).ok()
                                        })
                                        .map(|d| d.as_secs())
                                });
                                let mut positions =
                                    state_clone.watch_positions.lock().unwrap();
                                if let Some(WatchState::Dir(last_mod)) =
                                    positions.get_mut(&path_clone)
                                {
                                    if *last_mod == current_mod {
                                        return;
                                    }
                                    *last_mod = current_mod;
                                } else {
                                    positions.insert(
                                        path_clone.clone(),
                                        WatchState::Dir(current_mod),
                                    );
                                }
                                let js = build_js_callback(&CompactResponse {
                                    action: "watch",
                                    status: "watch_data",
                                    message: &path_clone,
                                    data: Some("OK"),
                                });
                                let _ = sender_clone.try_send(js);
                            });
                            std::mem::forget(monitor);
                        }
                    } else {
                        if let Some(parent) = path_buf.parent() {
                            let parent_path = parent.to_path_buf();
                            let file = gio::File::for_path(&parent_path);
                            if let Ok(monitor) = file.monitor_directory(
                                gio::FileMonitorFlags::NONE,
                                None::<&gio::Cancellable>,
                            ) {
                                let file_name = path_buf_clone.file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("")
                                    .to_string();
                                monitor.connect_changed(move |_, gio_file, _, event_type| {
                                    if event_type != gio::FileMonitorEvent::Changed
                                        && event_type != gio::FileMonitorEvent::ChangesDoneHint
                                    {
                                        return;
                                    }
                                    if let Some(name) = gio_file.path() {
                                        if let Some(fname) = name.file_name() {
                                            let fname_str = fname.to_string_lossy();
                                            if fname_str != file_name {
                                                return;
                                            }
                                        }
                                    }
                                    {
                                        let positions = state_clone.watch_positions.lock().unwrap();
                                        if !positions.contains_key(&path_clone) {
                                            return;
                                        }
                                    }
                                    send_new_lines(
                                        &path_clone,
                                        &sender_clone,
                                        &state_clone,
                                    );
                                    let js = build_js_callback(&CompactResponse {
                                        action: "watch",
                                        status: "watch_data",
                                        message: &path_clone,
                                        data: Some("OK"),
                                    });
                                    let _ = sender_clone.try_send(js);
                                });
                                std::mem::forget(monitor);
                            }
                        }
                    }
                    send_js("watch", "success", "Мониторинг запущен", None);
                } else {
                    #[cfg(feature = "debug")]
                    if is_debug {
                        eprintln!(
                            "{}[Руник РЕЗУЛЬТАТ]{} watch => не указан путь",
                            YELLOW, RESET
                        );
                    }
                }
            }
            "unwatch" => {
                let path = request.payload.get("path").and_then(|v| v.as_str());
                if let Some(p) = path {
                    let mut positions = state.watch_positions.lock().unwrap();
                    if positions.remove(p).is_some() {
                        #[cfg(feature = "debug")]
                        if is_debug {
                            eprintln!(
                                "{}[Руник РЕЗУЛЬТАТ]{} unwatch '{}' => мониторинг остановлен",
                                GREEN, RESET, p
                            );
                        }
                        send_js("unwatch", "success", "Мониторинг остановлен", None);
                    } else {
                        #[cfg(feature = "debug")]
                        if is_debug {
                            eprintln!(
                                "{}[Руник РЕЗУЛЬТАТ]{} unwatch '{}' => файл не мониторится",
                                YELLOW, RESET, p
                            );
                        }
                        send_js("unwatch", "error", "Файл не мониторится", None);
                    }
                } else {
                    #[cfg(feature = "debug")]
                    if is_debug {
                        eprintln!(
                            "{}[Руник РЕЗУЛЬТАТ]{} unwatch => не указан путь",
                            YELLOW, RESET
                        );
                    }
                    send_js("unwatch", "error", "Не указан путь к файлу", None);
                }
            }
            "stats" => {
                #[cfg(feature = "debug")]
                {
                    let summary = state.thread_tracker.get_summary();
                    if is_debug {
                        eprintln!(
                            "{}[Руник РЕЗУЛЬТАТ]{} stats => запрошена статистика потоков",
                            CYAN, RESET
                        );
                    }
                    send_js("stats", "success", &summary, None);
                }
                #[cfg(not(feature = "debug"))]
                send_js("stats", "error", "Недоступно в release-сборке", None);
            }
            _ => {
                #[cfg(feature = "debug")]
                if is_debug {
                    eprintln!(
                        "{}[Руник РЕЗУЛЬТАТ]{} неизвестное действие: '{}'",
                        YELLOW, RESET, request.action
                    );
                }
                send_js(&request.action, "error", "Неизвестное действие", None);
            }
        }
        #[cfg(feature = "debug")]
        if is_debug {
            if let Some(ss) = start_stats {
                let total_duration = start_time.elapsed();
                let end_stats = get_resource_stats();
                let memory_delta = end_stats.memory_kb as i64 - ss.memory_kb as i64;
                let cpu_delta = end_stats.cpu_time_ms as i64 - ss.cpu_time_ms as i64;
                if total_duration > Duration::from_millis(100)
                    || memory_delta.abs() > 1024
                    || cpu_delta > 50
                {
                    eprintln!(
                        "{}[Руник Профиль]{} {} '{}' | Время: {:?} | ΔПамять: {} KB | ΔCPU: {} ms",
                        MAGENTA,
                        RESET,
                        BOLD,
                        request.action,
                        total_duration,
                        memory_delta,
                        cpu_delta
                    );
                }
            }
        }
    });
}

fn create_ui(
    config: &UiConfig,
    file_url: &str,
    app_state: &Arc<AppState>,
    js_sender: &Sender<String>,
) -> (gtk::Window, WebView) {
    let window = gtk::Window::new(gtk::WindowType::Popup);
    window.set_default_size(800, 600);
    if let Some(screen) = gtk::prelude::GtkWindowExt::screen(&window) {
        if let Some(visual) = screen.rgba_visual() {
            window.set_visual(Some(&visual));
        }
    }
    window.set_app_paintable(true);
    let css = gtk::CssProvider::new();
    css.load_from_data(b"window { background-color: rgba(0,0,0,0); }")
        .expect("Ошибка загрузки CSS");
    if let Some(screen) = gtk::gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(
            &screen,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    init_for_window(&window);
    set_layer(&window, Layer::Background);
    set_keyboard_interactivity(&window, false);
    set_namespace(&window, "runic");
    match (config.width, config.height) {
        (Some(w), Some(h)) => {
            window.set_default_size(w, h);
            set_anchor(&window, Edge::Top, true);
            set_anchor(&window, Edge::Left, true);
            set_anchor(&window, Edge::Right, false);
            set_anchor(&window, Edge::Bottom, false);
            set_margin(&window, Edge::Top, config.y);
            set_margin(&window, Edge::Left, config.x);
        }
        _ => {
            set_anchor(&window, Edge::Top, true);
            set_anchor(&window, Edge::Left, true);
            set_anchor(&window, Edge::Right, true);
            set_anchor(&window, Edge::Bottom, true);
        }
    }
    set_exclusive_zone(&window, 0);
    let webview = WebView::new();
    webview.set_background_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    if let Some(settings) = WebViewExt::settings(&webview) {
        #[cfg(feature = "debug")]
        if config.is_debug {
            settings.set_enable_write_console_messages_to_stdout(true);
        }
        settings.set_enable_webgl(false);
        settings.set_enable_webaudio(false);
        settings.set_enable_media_stream(false);
        settings.set_enable_mediasource(false);
        settings.set_enable_encrypted_media(false);
        settings.set_enable_smooth_scrolling(false);
        settings.set_enable_page_cache(false);
    }
    let controller = webview
        .user_content_manager()
        .expect("WebView должен иметь user_content_manager");
    let state_clone = Arc::clone(app_state);
    let sender_for_handler = js_sender.clone();
    let registered = controller.register_script_message_handler("ipc");
    #[cfg(feature = "debug")]
    if config.is_debug {
        eprintln!(
            "{}[Руник ОТЛАДКА]{} Обработчик 'ipc' зарегистрирован: {}",
            CYAN, RESET, registered
        );
    }
    #[cfg(not(feature = "debug"))]
    let _ = registered;
    controller.connect_script_message_received(
        Some("ipc"),
        move |_, msg: &webkit2gtk::JavascriptResult| {
            if let Some(js_val) = msg.js_value() {
                let msg_str = js_val.to_string();
                #[cfg(feature = "debug")]
                if state_clone.is_debug {
                    eprintln!("{}[Руник ОТЛАДКА]{} Получено: {}", CYAN, RESET, msg_str);
                }
                if let Ok(request) = serde_json::from_str::<JsMessage>(&msg_str) {
                    handle_action(
                        Arc::clone(&state_clone),
                        request,
                        sender_for_handler.clone(),
                    );
                }
            }
        },
    );
    let shim = UserScript::new(
        r#"window.ipc = { postMessage: function(msg) { try { if (window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.ipc) { window.webkit.messageHandlers.ipc.postMessage(msg); } else { console.error('[Руник] Критическая ошибка: IPC мост недоступен'); } } catch(e) { console.error('[Руник] Ошибка отправки IPC:', e); } } };"#,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    );
    controller.add_script(&shim);
    webview.load_uri(file_url);
    window.add(&webview);
    let state_for_delete = Arc::clone(app_state);
    window.connect_delete_event(move |_, _| {
        cleanup_children(&state_for_delete);
        gtk::main_quit();
        Propagation::Stop
    });
    (window, webview)
}

fn main() {
    let args = Args::parse();
    let html_path = match args.html_path {
        Some(p) => p,
        None => {
            use clap::CommandFactory;
            let _ = Args::command().print_help();
            println!();
            std::process::exit(0);
        }
    };
    let is_debug = args.debug;
    #[cfg(not(feature = "debug"))]
    let _ = is_debug;
    let abs_path = match fs::canonicalize(&html_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "{}Ошибка:{} не удалось найти файл '{}': {}",
                RED, RESET, html_path, e
            );
            process::exit(1);
        }
    };
    let file_url = format!("file://{}", abs_path.display());
    gtk::init().expect("Не удалось инициализировать GTK");
    #[cfg(feature = "debug")]
    let thread_tracker = Arc::new(ThreadTracker::new());
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let pool = Arc::new(ThreadPool::new(THREAD_POOL_SIZE));
    let app_state = Arc::new(AppState {
        running_streams: Mutex::new(HashMap::new()),
        watch_positions: Mutex::new(HashMap::new()),
        #[cfg(feature = "debug")]
        is_debug,
        shutdown: Arc::clone(&shutdown_flag),
        #[cfg(feature = "debug")]
        thread_tracker: Arc::clone(&thread_tracker),
        pool: Arc::clone(&pool),
    });
    let (js_sender, js_receiver) = async_channel::bounded::<String>(IPC_CHANNEL_SIZE);
    let (event_sender, event_receiver) = async_channel::unbounded::<WidgetEvent>();
    let ui_config = UiConfig {
        width: args.width,
        height: args.height,
        x: args.x,
        y: args.y,
        #[cfg(feature = "debug")]
        is_debug: args.debug,
    };
    let (initial_window, initial_webview) =
        create_ui(&ui_config, &file_url, &app_state, &js_sender);
    let window_rc = Rc::new(RefCell::new(Some(initial_window)));
    let webview_rc = Rc::new(RefCell::new(Some(initial_webview)));
    window_rc.borrow().as_ref().unwrap().show_all();
    #[cfg(feature = "debug")]
    if is_debug {
        eprintln!(
            "{}[Руник ОТЛАДКА]{} Виджет успешно запущен и отображен",
            CYAN, RESET
        );
    }
    let webview_rc_for_js = webview_rc.clone();
    #[cfg(feature = "debug")]
    let is_debug_for_js = is_debug;
    glib::spawn_future_local(async move {
        while let Ok(js_code) = js_receiver.recv().await {
            let wv_opt = webview_rc_for_js.borrow().clone();
            if let Some(wv) = wv_opt {
                wv.run_javascript(&js_code, None::<&gio::Cancellable>, move |result| {
                    #[cfg(feature = "debug")]
                    if is_debug_for_js {
                        if let Err(e) = result {
                            eprintln!(
                                "{}[Руник ОТЛАДКА]{} Ошибка выполнения JS: {:?}",
                                YELLOW, RESET, e
                            );
                        }
                    }
                    #[cfg(not(feature = "debug"))]
                    drop(result);
                });
            } else {
                #[cfg(feature = "debug")]
                if is_debug_for_js {
                    eprintln!(
                        "{}[Руник ОТЛАДКА]{} Пропуск выполнения JS: WebView уничтожен",
                        YELLOW, RESET
                    );
                }
            }
        }
    });
    let window_rc_for_events = window_rc.clone();
    let webview_rc_for_events = webview_rc.clone();
    let app_state_for_events = app_state.clone();
    let js_sender_for_events = js_sender.clone();
    let file_url_for_events = file_url.clone();
    let ui_config_for_events = ui_config.clone();
    glib::spawn_future_local(async move {
        while let Ok(event) = event_receiver.recv().await {
            match event {
                WidgetEvent::Stop(_reason) => {
                    #[cfg(feature = "debug")]
                    if ui_config_for_events.is_debug {
                        eprintln!(
                            "{}[Руник ОТЛАДКА]{} Событие остановки: {}. Приостанавливаем фоновые задачи...",
                            CYAN, RESET, _reason
                        );
                    }
                    kill_and_clear_tracked(&app_state_for_events);
                    if let Ok(mut positions) = app_state_for_events.watch_positions.lock() {
                        positions.clear();
                    }
                    app_state_for_events.shutdown.store(true, Ordering::Relaxed);
                    *webview_rc_for_events.borrow_mut() = None;
                    if let Some(w) = window_rc_for_events.borrow_mut().take() {
                        unsafe {
                            w.destroy();
                        }
                    }
                }
                WidgetEvent::Start(_reason) => {
                    #[cfg(feature = "debug")]
                    if ui_config_for_events.is_debug {
                        eprintln!(
                            "{}[Руник ОТЛАДКА]{} Событие запуска: {}. Возобновляем фоновые задачи...",
                            CYAN, RESET, _reason
                        );
                    }
                    kill_and_clear_tracked(&app_state_for_events);
                    if let Ok(mut positions) = app_state_for_events.watch_positions.lock() {
                        positions.clear();
                    }
                    app_state_for_events
                        .shutdown
                        .store(false, Ordering::Relaxed);
                    *webview_rc_for_events.borrow_mut() = None;
                    if let Some(w) = window_rc_for_events.borrow_mut().take() {
                        unsafe {
                            w.destroy();
                        }
                    }
                    let (new_window, new_webview) = create_ui(
                        &ui_config_for_events,
                        &file_url_for_events,
                        &app_state_for_events,
                        &js_sender_for_events,
                    );
                    new_window.show_all();
                    *window_rc_for_events.borrow_mut() = Some(new_window);
                    *webview_rc_for_events.borrow_mut() = Some(new_webview);
                }
            }
        }
    });
    glib::source::unix_signal_add(libc::SIGTERM, {
        let state_clone = Arc::clone(&app_state);
        move || {
            cleanup_children(&state_clone);
            gtk::main_quit();
            glib::ControlFlow::Break
        }
    });
    glib::source::unix_signal_add(libc::SIGINT, {
        let state_clone = Arc::clone(&app_state);
        move || {
            cleanup_children(&state_clone);
            gtk::main_quit();
            glib::ControlFlow::Break
        }
    });

    #[cfg(feature = "debug")]
    if is_debug {
        eprintln!("{}[Руник ОТЛАДКА]{} Режим отладки включен", CYAN, RESET);
        let tracker_for_stats = Arc::clone(&thread_tracker);
        let shutdown_for_stats = Arc::clone(&shutdown_flag);
        std::thread::spawn(move || {
            while !shutdown_for_stats.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(THREAD_STATS_INTERVAL_SECS));
                if !shutdown_for_stats.load(Ordering::Relaxed) {
                    eprintln!("{}", tracker_for_stats.get_summary());
                }
            }
        });
        if let Some(html_dir) = abs_path.parent() {
            let html_dir = html_dir.to_path_buf();
            let (reload_sender, reload_receiver) = async_channel::bounded::<()>(16);
            let event_sender_for_reload = event_sender.clone();
            glib::spawn_future_local(async move {
                while let Ok(()) = reload_receiver.recv().await {
                    eprintln!(
                        "{}[Руник ОТЛАДКА]{} Обнаружены изменения файлов. Полное пересоздание виджета...",
                        CYAN, RESET
                    );
                    let _ = event_sender_for_reload
                        .send(WidgetEvent::Stop(
                            "hot-reload (изменение файлов)".to_string(),
                        ))
                        .await;
                    glib::timeout_future(Duration::from_millis(200)).await;
                    let _ = event_sender_for_reload
                        .send(WidgetEvent::Start(
                            "hot-reload (изменение файлов)".to_string(),
                        ))
                        .await;
                }
            });
            let file = gio::File::for_path(&html_dir);
            if let Ok(monitor) =
                file.monitor_directory(gio::FileMonitorFlags::NONE, None::<&gio::Cancellable>)
            {
                let reload_sender_for_monitor = reload_sender.clone();
                let last_reload = std::cell::RefCell::new(Instant::now());
                monitor.connect_changed(move |_, file, _other_file, event_type| {
                    if event_type != gio::FileMonitorEvent::ChangesDoneHint
                        && event_type != gio::FileMonitorEvent::Created
                        && event_type != gio::FileMonitorEvent::Deleted
                        && event_type != gio::FileMonitorEvent::Moved
                    {
                        return;
                    }
                    if let Some(path) = file.path() {
                        if let Some(name) = path.file_name() {
                            let name_str = name.to_string_lossy();
                            if (name_str.ends_with(".html")
                                || name_str.ends_with(".css")
                                || name_str.ends_with(".js"))
                                && !name_str.ends_with('~')
                                && !name_str.contains(".swp")
                                && !name_str.contains(".tmp")
                            {
                                let now = Instant::now();
                                let mut should_reload = last_reload.borrow_mut();
                                if now.duration_since(*should_reload)
                                    > Duration::from_millis(HOT_RELOAD_DEBOUNCE_MS)
                                {
                                    *should_reload = now;
                                    *should_reload = now;
                                    eprintln!(
                                        "{}[Руник ОТЛАДКА]{} Изменён файл: {}",
                                        CYAN, RESET, name_str
                                    );
                                    let _ = reload_sender_for_monitor.send_blocking(());
                                }
                            }
                        }
                    }
                });
                std::mem::forget(monitor);
            } else {
                eprintln!(
                    "{}[Руник ОТЛАДКА]{} Ошибка создания FileMonitor для директории",
                    YELLOW, RESET
                );
            }
        }
    }
    std::thread::spawn({
        let event_sender = event_sender.clone();
        #[cfg(feature = "debug")]
        let is_debug_events = is_debug;
        move || {
            let conn = match zbus::blocking::Connection::system() {
                Ok(c) => c,
                #[cfg(feature = "debug")]
                Err(e) => {
                    if is_debug_events {
                        eprintln!("{}[Руник ОТЛАДКА]{} Ошибка подключения к DBus: {}", RED, RESET, e);
                    }
                    return;
                }
                #[cfg(not(feature = "debug"))]
                Err(_e) => {
                    return;
                }
            };
            let proxy = match zbus::blocking::Proxy::new(
                &conn,
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
            ) {
                Ok(p) => p,
                #[cfg(feature = "debug")]
                Err(e) => {
                    if is_debug_events {
                        eprintln!("{}[Руник ОТЛАДКА]{} Ошибка прокси logind: {}", RED, RESET, e);
                    }
                    return;
                }
                #[cfg(not(feature = "debug"))]
                Err(_e) => {
                    return;
                }
            };
            let signal_stream = match proxy.receive_signal("PrepareForSleep") {
                Ok(s) => s,
                #[cfg(feature = "debug")]
                Err(e) => {
                    if is_debug_events {
                        eprintln!(
                            "{}[Руник ОТЛАДКА]{} Ошибка подписки на PrepareForSleep: {}",
                            RED, RESET, e
                        );
                    }
                    return;
                }
                #[cfg(not(feature = "debug"))]
                Err(_e) => {
                    return;
                }
            };
            #[cfg(feature = "debug")]
            if is_debug_events {
                eprintln!(
                    "{}[Руник ОТЛАДКА]{} Подписка на PrepareForSleep активна (событийный режим)",
                    CYAN, RESET
                );
            }
            for signal in signal_stream {
                let entering_sleep: bool = match signal.body().deserialize() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if entering_sleep {
                    #[cfg(feature = "debug")]
                    if is_debug_events {
                        eprintln!(
                            "{}[Руник ОТЛАДКА]{} Сигнал PrepareForSleep(true) — уход в сон",
                            CYAN, RESET
                        );
                    }
                    let _ = event_sender.send_blocking(WidgetEvent::Stop(
                        "suspend (системный сон)".to_string(),
                    ));
                } else {
                    #[cfg(feature = "debug")]
                    if is_debug_events {
                        eprintln!(
                            "{}[Руник ОТЛАДКА]{} Сигнал PrepareForSleep(false) — пробуждение",
                            CYAN, RESET
                        );
                    }
                    let _ = event_sender.send_blocking(WidgetEvent::Start(
                        "resume (выход из сна)".to_string(),
                    ));
                }
            }
        }
    });
    gtk::main();
}

#[cfg(all(test, feature = "debug"))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::env;
    use std::thread::sleep;

    #[allow(clippy::type_complexity)]
    fn setup_test_state() -> (
        Arc<AppState>,
        Sender<String>,
        async_channel::Receiver<String>,
    ) {
        let state = Arc::new(AppState {
            running_streams: Mutex::new(HashMap::new()),
            watch_positions: Mutex::new(HashMap::new()),
            is_debug: false,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread_tracker: Arc::new(ThreadTracker::new()),
            pool: Arc::new(ThreadPool::new(THREAD_POOL_SIZE)),
        });
        let (sender, receiver) = async_channel::bounded::<String>(IPC_CHANNEL_SIZE);
        (state, sender, receiver)
    }

    fn get_response(receiver: &async_channel::Receiver<String>) -> RustResponse {
        let js_code = receiver
            .recv_blocking()
            .expect("Должно прийти сообщение от Rust");
        let json_str = js_code
            .trim_start_matches("if (window.onRunicResponse) window.onRunicResponse(")
            .trim_end_matches(");");
        serde_json::from_str(json_str).expect("Ответ должен быть валидным JSON")
    }

    #[test]
    fn test_handle_action_read_success() {
        let (state, sender, receiver) = setup_test_state();
        let temp_dir = env::temp_dir();
        let file_path = temp_dir.join("runic_test_read.txt");
        std::fs::write(&file_path, "test content").unwrap();
        let request = JsMessage {
            action: "read".to_string(),
            payload: json!({"path": file_path.to_str().unwrap()}),
        };
        handle_action(Arc::clone(&state), request, sender);
        let response = get_response(&receiver);
        assert_eq!(response.action, "read");
        assert_eq!(response.status, "success");
        assert_eq!(response.data, Some("test content".to_string()));
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_handle_action_read_error() {
        let (state, sender, receiver) = setup_test_state();
        let request = JsMessage {
            action: "read".to_string(),
            payload: json!({"path": "/nonexistent/path/file_12345.txt"}),
        };
        handle_action(Arc::clone(&state), request, sender);
        let response = get_response(&receiver);
        assert_eq!(response.action, "read");
        assert_eq!(response.status, "error");
    }

    #[test]
    fn test_handle_action_write_success() {
        let (state, sender, receiver) = setup_test_state();
        let temp_dir = env::temp_dir();
        let file_path = temp_dir.join("runic_test_write.txt");
        let _ = std::fs::remove_file(&file_path);
        let request = JsMessage {
            action: "write".to_string(),
            payload: json!({"path": file_path.to_str().unwrap(), "data": "appended data"}),
        };
        handle_action(Arc::clone(&state), request, sender);
        let response = get_response(&receiver);
        assert_eq!(response.action, "write");
        assert_eq!(response.status, "success");
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "appended data");
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_handle_action_stream_success_and_cleanup() {
        let (state, sender, receiver) = setup_test_state();
        let request = JsMessage {
            action: "stream".to_string(),
            payload: json!({"target": "echo 'stream_data_test'"}),
        };
        handle_action(Arc::clone(&state), request, sender);
        let start_response = get_response(&receiver);
        assert_eq!(start_response.status, "success");
        let data_response = get_response(&receiver);
        assert_eq!(data_response.status, "stream_data");
        assert_eq!(data_response.data, Some("stream_data_test".to_string()));
        sleep(Duration::from_millis(500));
        assert!(state.running_streams.lock().unwrap().is_empty());
    }

    #[test]
    fn test_handle_action_stream_duplicate_error() {
        let (state, sender, receiver) = setup_test_state();
        let request = JsMessage {
            action: "stream".to_string(),
            payload: json!({"target": "sleep 10"}),
        };
        handle_action(Arc::clone(&state), request, sender.clone());
        let _ = get_response(&receiver);
        let request2 = JsMessage {
            action: "stream".to_string(),
            payload: json!({"target": "sleep 10"}),
        };
        handle_action(Arc::clone(&state), request2, sender);
        let dup_response = get_response(&receiver);
        assert_eq!(dup_response.status, "success");
        assert_eq!(dup_response.message, "Уже выполняется (игнорируется)");
        state.running_streams.lock().unwrap().clear();
    }

    #[test]
    fn test_handle_action_watch_file_success() {
        let (state, sender, receiver) = setup_test_state();
        let temp_dir = env::temp_dir();
        let file_path = temp_dir.join("runic_test_watch.log");
        std::fs::write(&file_path, "initial line\n").unwrap();
        let request = JsMessage {
            action: "watch".to_string(),
            payload: json!({"path": file_path.to_str().unwrap(), "tail": 1}),
        };
        handle_action(Arc::clone(&state), request, sender);
        let mut got_success = false;
        let mut got_watch_data = false;
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(200) {
            match receiver.try_recv() {
                Ok(js_code) => {
                    let json_str = js_code
                        .trim_start_matches("if (window.onRunicResponse) window.onRunicResponse(")
                        .trim_end_matches(");");
                    if let Ok(response) = serde_json::from_str::<RustResponse>(json_str) {
                        if response.status == "success" && response.action == "watch" {
                            got_success = true;
                        } else if response.status == "watch_data" && response.action == "watch" {
                            got_watch_data = true;
                        }
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        assert!(got_success);
        assert!(got_watch_data);
        state
            .watch_positions
            .lock()
            .unwrap()
            .remove(file_path.to_str().unwrap());
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_kill_and_clear_tracked_no_double_kill() {
        let (state, _sender, _receiver) = setup_test_state();
        let child = Command::new("sleep")
            .arg("10")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _pid = child.id();
        state
            .running_streams
            .lock()
            .unwrap()
            .insert("test".to_string(), ChildGuard(child));
        kill_and_clear_tracked(&state);
        assert!(state.running_streams.lock().unwrap().is_empty());
    }

    #[test]
    fn test_widget_event_creation() {
        let stop_event = WidgetEvent::Stop("suspend".to_string());
        let start_event = WidgetEvent::Start("resume".to_string());
        match stop_event {
            WidgetEvent::Stop(reason) => assert_eq!(reason, "suspend"),
            _ => panic!("Ожидалось событие Stop"),
        }
        match start_event {
            WidgetEvent::Start(reason) => assert_eq!(reason, "resume"),
            _ => panic!("Ожидалось событие Start"),
        }
    }

    #[test]
    fn test_ui_config_clone() {
        let config = UiConfig {
            width: Some(1024),
            height: None,
            x: 5,
            y: 15,
            is_debug: false,
        };
        let cloned = config.clone();
        assert_eq!(cloned.width, Some(1024));
        assert_eq!(cloned.height, None);
        assert!(!cloned.is_debug);
    }
}
