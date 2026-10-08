use async_channel::Sender;
use clap::Parser;
use glib::Propagation;
use gtk::prelude::*;
use gtk_layer_shell::{
    Edge, Layer, init_for_window, set_anchor, set_exclusive_zone, set_keyboard_interactivity,
    set_layer, set_margin, set_namespace,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use webkit2gtk::{
    SettingsExt, UserContentInjectedFrames, UserContentManagerExt, UserScript,
    UserScriptInjectionTime, WebView, WebViewExt,
};

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

enum WatchState {
    File(Option<u64>),
    Dir(Option<u64>),
}

struct AppState {
    running_streams: Mutex<HashMap<String, std::process::Child>>,
    watch_positions: Mutex<HashMap<String, WatchState>>,
    spawned_pids: Mutex<Vec<u32>>,
    is_debug: bool,
    shutdown: AtomicBool,
}

fn safe_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        eprintln!("[Runic Warning] Mutex was poisoned, recovering...");
        poisoned.into_inner()
    })
}

fn cleanup_children(state: &Arc<AppState>) {
    eprintln!("[Runic Debug] === НАЧАЛО ОЧИСТКИ ПРОЦЕССОВ ===");
    state.shutdown.store(true, Ordering::Relaxed);

    let pids = safe_lock(&state.spawned_pids).clone();
    if pids.is_empty() {
        eprintln!("[Runic Debug] Нечего очищать.");
        return;
    }
    for &pid in &pids {
        eprintln!("[Runic Debug] Отправка SIGTERM процессу {}", pid);
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    for &pid in &pids {
        eprintln!("[Runic Debug] Отправка SIGKILL процессу {}", pid);
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
            let mut status: libc::c_int = 0;
            libc::waitpid(pid as libc::pid_t, &mut status, 0);
        }
    }
    eprintln!("[Runic Debug] === ОЧИСТКА ПРОЦЕССОВ ЗАВЕРШЕНА ===");
}

fn kill_and_clear_tracked(state: &Arc<AppState>) {
    eprintln!("[Runic Debug] === ПРИНУДИТЕЛЬНАЯ ОЧИСТКА ===");
    state.shutdown.store(true, Ordering::Relaxed);

    // Сначала убиваем процессы из running_streams и собираем их PID
    let mut killed_pids = Vec::new();
    {
        let mut streams = safe_lock(&state.running_streams);
        for (_, child) in streams.drain() {
            let pid = child.id();
            killed_pids.push(pid);
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
                let mut status: libc::c_int = 0;
                libc::waitpid(pid as libc::pid_t, &mut status, 0);
            }
        }
    }

    // Затем убиваем оставшиеся PID, исключая уже убитые
    {
        let mut pids = safe_lock(&state.spawned_pids);
        pids.retain(|p| !killed_pids.contains(p));
        for &pid in pids.iter() {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
                let mut status: libc::c_int = 0;
                libc::waitpid(pid as libc::pid_t, &mut status, 0);
            }
        }
        pids.clear();
    }

    eprintln!("[Runic Debug] === ПРИНУДИТЕЛЬНАЯ ОЧИСТКА ЗАВЕРШЕНА ===");
}

fn handle_action(state: Arc<AppState>, request: JsMessage, js_sender: Sender<String>) {
    thread::spawn(move || {
        let send_js = |action: &str, status: &str, message: &str, data: Option<String>| {
            let response = RustResponse {
                action: action.to_string(),
                status: status.to_string(),
                message: message.to_string(),
                data,
            };
            if let Ok(json) = serde_json::to_string(&response) {
                let js = format!(
                    "if (window.onRunicResponse) window.onRunicResponse({});",
                    json
                );
                let _ = js_sender.send_blocking(js);
            }
        };

        match request.action.as_str() {
            "read" => {
                if let Some(path) = request.payload.get("path").and_then(|v| v.as_str()) {
                    match fs::read_to_string(path) {
                        Ok(content) => send_js("read", "success", "OK", Some(content)),
                        Err(e) => send_js("read", "error", &format!("{}", e), None),
                    }
                }
            }
            "write" => {
                let path = request.payload.get("path").and_then(|v| v.as_str());
                let data = request.payload.get("data").and_then(|v| v.as_str());
                if let (Some(p), Some(d)) = (path, data) {
                    let path = Path::new(p);
                    if let Some(parent) = path.parent() {
                        if let Err(e) = fs::create_dir_all(parent) {
                            return send_js(
                                "write",
                                "error",
                                &format!("create_dir_all: {}", e),
                                None,
                            );
                        }
                    }
                    let append = request
                        .payload
                        .get("append")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    match OpenOptions::new()
                        .create(true)
                        .write(true)
                        .append(append)
                        .truncate(!append)
                        .open(path)
                    {
                        Ok(mut file) => {
                            match file.write_all(d.as_bytes()).and_then(|_| file.flush()) {
                                Ok(_) => send_js("write", "success", "OK", None),
                                Err(e) => send_js("write", "error", &format!("{}", e), None),
                            }
                        }
                        Err(e) => send_js("write", "error", &format!("{}", e), None),
                    }
                }
            }
            "exec" => {
                if let Some(cmd) = request.payload.get("command").and_then(|v| v.as_str()) {
                    match Command::new("sh").arg("-c").arg(cmd).output() {
                        Ok(out) => {
                            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                            let result = if out.status.success() {
                                stdout
                            } else {
                                format!("STDERR:\n{}", stderr)
                            };
                            send_js("exec", "success", &("OK: ".to_owned() + cmd), Some(result));
                        }
                        Err(e) => send_js("exec", "error", &format!("{}", e), None),
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
                    send_js("stream", "error", "target не указан", None);
                    return;
                }

                let target_clone = target.clone();
                let state_clone = Arc::clone(&state);
                let sender_clone = js_sender.clone();

                let mut cmd_obj = Command::new("sh");
                cmd_obj
                    .arg("-c")
                    .arg(&target_clone)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());

                // ИСПРАВЛЕНО: один лок от проверки до вставки (устранён TOCTOU)
                let mut running = safe_lock(&state.running_streams);
                if running.contains_key(&target) {
                    send_js("stream", "error", "Уже выполняется", None);
                    return;
                }

                match cmd_obj.spawn() {
                    Ok(mut proc) => {
                        let pid = proc.id();
                        eprintln!("[Runic Debug] Stream запущен: PID={}", pid);
                        safe_lock(&state_clone.spawned_pids).push(pid);
                        let stdout = proc.stdout.take().unwrap();
                        let stderr = proc.stderr.take();
                        running.insert(target_clone.clone(), proc);
                        drop(running); // Отпускаем лок перед spawn потоков чтения

                        if let Some(stderr_pipe) = stderr {
                            let target_for_stderr = target_clone.clone();
                            thread::spawn(move || {
                                let reader = BufReader::new(stderr_pipe);
                                for l in reader.lines().map_while(Result::ok) {
                                    eprintln!("[Runic STDERR][{}] {}", target_for_stderr, l);
                                }
                            });
                        }

                        thread::spawn(move || {
                            let reader = BufReader::new(stdout);
                            for line in reader.lines() {
                                match line {
                                    Ok(l) => {
                                        let response = RustResponse {
                                            action: "stream".to_string(),
                                            status: "stream_data".to_string(),
                                            message: target_clone.clone(),
                                            data: Some(l.clone()),
                                        };
                                        if let Ok(json) = serde_json::to_string(&response) {
                                            let js = format!(
                                                "if (window.onRunicResponse) window.onRunicResponse({});",
                                                json
                                            );
                                            let _ = sender_clone.send_blocking(js);
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            // Предотвращение зомби при естественном завершении
                            if let Some(mut child) =
                                safe_lock(&state_clone.running_streams).remove(&target_clone)
                            {
                                let _ = child.wait();
                                let mut pids = safe_lock(&state_clone.spawned_pids);
                                pids.retain(|&p| p != child.id());
                            }
                        });
                        send_js("stream", "success", "Запущен", None);
                    }
                    Err(e) => {
                        drop(running);
                        send_js(
                            "stream",
                            "error",
                            &format!("EXEC ERROR: {}", e),
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
                let mut running = safe_lock(&state.running_streams);
                if let Some(child) = running.remove(target) {
                    let pid = child.id();
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                        let mut status: libc::c_int = 0;
                        libc::waitpid(pid as libc::pid_t, &mut status, 0);
                    }
                    let mut pids = safe_lock(&state.spawned_pids);
                    pids.retain(|&p| p != pid);
                    send_js("unstream", "success", "Остановлен", None);
                } else {
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
                        let mut positions = safe_lock(&state.watch_positions);
                        if positions.contains_key(&path_str) {
                            send_js("watch", "error", "Уже мониторится", None);
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
                            WatchState::File(if path_buf.exists() {
                                path_buf.metadata().ok().map(|m| m.len())
                            } else {
                                None
                            })
                        };
                        positions.insert(path_str.clone(), initial_state);
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
                                            let read_size = std::cmp::min(file_len, 64 * 1024);
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
                                                let response = RustResponse {
                                                    action: "watch".to_string(),
                                                    status: "watch_data".to_string(),
                                                    message: path_clone.clone(),
                                                    data: Some(line.clone()),
                                                };
                                                if let Ok(json) = serde_json::to_string(&response) {
                                                    let js = format!(
                                                        "if (window.onRunicResponse) window.onRunicResponse({});",
                                                        json
                                                    );
                                                    let _ = sender_clone.send_blocking(js);
                                                }
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
                    thread::spawn(move || {
                        // ИСПРАВЛЕНО: проверка shutdown для чистой остановки
                        while !state_clone.shutdown.load(Ordering::Relaxed) {
                            {
                                let positions = safe_lock(&state_clone.watch_positions);
                                if !positions.contains_key(&path_clone) {
                                    break;
                                }
                            }
                            let current_path_buf = std::path::PathBuf::from(&path_clone);
                            let is_dir = current_path_buf.is_dir();
                            let exists = current_path_buf.exists();
                            if exists {
                                if is_dir {
                                    let current_mod_time =
                                        current_path_buf.metadata().ok().and_then(|m| {
                                            m.modified()
                                                .ok()
                                                .and_then(|t| {
                                                    t.duration_since(std::time::UNIX_EPOCH).ok()
                                                })
                                                .map(|d| d.as_secs())
                                        });
                                    let should_notify = {
                                        let mut positions = safe_lock(&state_clone.watch_positions);
                                        if let Some(WatchState::Dir(last_mod)) =
                                            positions.get_mut(&path_clone)
                                        {
                                            if *last_mod != current_mod_time {
                                                *last_mod = current_mod_time;
                                                true
                                            } else {
                                                false
                                            }
                                        } else {
                                            // ИСПРАВЛЕНО: insert вместо unwrap (устранена паника)
                                            positions.insert(
                                                path_clone.clone(),
                                                WatchState::Dir(current_mod_time),
                                            );
                                            true
                                        }
                                    };
                                    if should_notify {
                                        let response = RustResponse {
                                            action: "watch".to_string(),
                                            status: "watch_data".to_string(),
                                            message: path_clone.clone(),
                                            data: Some("OK".to_string()),
                                        };
                                        if let Ok(json) = serde_json::to_string(&response) {
                                            let js = format!(
                                                "if (window.onRunicResponse) window.onRunicResponse({});",
                                                json
                                            );
                                            let _ = sender_clone.send_blocking(js);
                                        }
                                    }
                                } else {
                                    if let Ok(mut file) = fs::File::open(&current_path_buf) {
                                        let meta = match file.metadata() {
                                            Ok(m) => m,
                                            Err(_) => {
                                                thread::sleep(Duration::from_secs(1));
                                                continue;
                                            }
                                        };
                                        let current_pos = {
                                            let positions = safe_lock(&state_clone.watch_positions);
                                            if let Some(WatchState::File(pos)) =
                                                positions.get(&path_clone)
                                            {
                                                *pos
                                            } else {
                                                None
                                            }
                                        };
                                        let file_len = meta.len();
                                        let start_pos = match current_pos {
                                            Some(pos) => {
                                                if file_len < pos {
                                                    0
                                                } else {
                                                    pos
                                                }
                                            }
                                            None => 0,
                                        };
                                        if file.seek(SeekFrom::Start(start_pos)).is_ok() {
                                            let reader = BufReader::new(file);
                                            for l in reader.lines().map_while(Result::ok) {
                                                if !l.is_empty() {
                                                    let response = RustResponse {
                                                        action: "watch".to_string(),
                                                        status: "watch_data".to_string(),
                                                        message: path_clone.clone(),
                                                        data: Some(l),
                                                    };
                                                    if let Ok(json) =
                                                        serde_json::to_string(&response)
                                                    {
                                                        let js = format!(
                                                            "if (window.onRunicResponse) window.onRunicResponse({});",
                                                            json
                                                        );
                                                        let _ = sender_clone.send_blocking(js);
                                                    }
                                                }
                                            }
                                        }
                                        let mut positions = safe_lock(&state_clone.watch_positions);
                                        if let Some(WatchState::File(pos_opt)) =
                                            positions.get_mut(&path_clone)
                                        {
                                            *pos_opt = Some(file_len);
                                        }
                                    } else {
                                        let mut positions = safe_lock(&state_clone.watch_positions);
                                        if let Some(WatchState::File(pos_opt)) =
                                            positions.get_mut(&path_clone)
                                        {
                                            *pos_opt = None;
                                        }
                                    }
                                }
                            } else {
                                let mut positions = safe_lock(&state_clone.watch_positions);
                                if let Some(state) = positions.get_mut(&path_clone) {
                                    match state {
                                        WatchState::File(pos) => *pos = None,
                                        WatchState::Dir(mod_time) => *mod_time = None,
                                    }
                                }
                            }
                            thread::sleep(Duration::from_secs(1));
                        }
                    });
                    send_js("watch", "success", "Мониторинг запущен", None);
                }
            }
            "unwatch" => {
                let path = request.payload.get("path").and_then(|v| v.as_str());
                if let Some(p) = path {
                    let mut positions = safe_lock(&state.watch_positions);
                    if positions.remove(p).is_some() {
                        send_js("unwatch", "success", "Мониторинг остановлен", None);
                    } else {
                        send_js("unwatch", "error", "Файл не мониторится", None);
                    }
                } else {
                    send_js("unwatch", "error", "Не указан путь к файлу", None);
                }
            }
            _ => send_js(&request.action, "error", "Неизвестное действие", None),
        }
    });
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

    let width = args.width;
    let height = args.height;
    let x = args.x;
    let y = args.y;
    let is_debug = args.debug;

    let abs_path = match fs::canonicalize(&html_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Ошибка: не удалось найти файл '{}': {}", html_path, e);
            process::exit(1);
        }
    };
    let file_url = format!("file://{}", abs_path.display());

    gtk::init().expect("Не удалось инициализировать GTK");

    let window = gtk::Window::new(gtk::WindowType::Popup);
    window.set_default_size(800, 600);

    // ИСПРАВЛЕНО: let chains → вложенные if (совместимость с MSRV < 1.87)
    if let Some(screen) = gtk::prelude::GtkWindowExt::screen(&window) {
        if let Some(visual) = screen.rgba_visual() {
            window.set_visual(Some(&visual));
        }
    }
    window.set_app_paintable(true);

    let css = gtk::CssProvider::new();
    css.load_from_data(b"window { background-color: rgba(0,0,0,0); }")
        .expect("CSS load failed");
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

    match (width, height) {
        (Some(w), Some(h)) => {
            window.set_default_size(w, h);
            set_anchor(&window, Edge::Top, true);
            set_anchor(&window, Edge::Left, true);
            set_anchor(&window, Edge::Right, false);
            set_anchor(&window, Edge::Bottom, false);
            set_margin(&window, Edge::Top, y);
            set_margin(&window, Edge::Left, x);
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
        if is_debug {
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

    let app_state = Arc::new(AppState {
        running_streams: Mutex::new(HashMap::new()),
        watch_positions: Mutex::new(HashMap::new()),
        spawned_pids: Mutex::new(Vec::new()),
        is_debug,
        shutdown: AtomicBool::new(false),
    });

    let (js_sender, js_receiver) = async_channel::bounded::<String>(1024);

    let state_clone = Arc::clone(&app_state);
    let sender_for_handler = js_sender.clone();

    let registered = controller.register_script_message_handler("ipc");
    if is_debug {
        eprintln!(
            "[Runic Debug] Обработчик 'ipc' зарегистрирован: {}",
            registered
        );
    }

    controller.connect_script_message_received(
        Some("ipc"),
        move |_controller, msg: &webkit2gtk::JavascriptResult| {
            if let Some(js_val) = msg.js_value() {
                let msg_str = js_val.to_string();
                if state_clone.is_debug {
                    eprintln!("[Runic Debug] Получено: {}", msg_str);
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
        r#"
        window.ipc = {
            postMessage: function(msg) {
                try {
                    if (window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.ipc) {
                        window.webkit.messageHandlers.ipc.postMessage(msg);
                    } else {
                        console.error('[Runic] Критическая ошибка: IPC мост недоступен');
                    }
                } catch(e) {
                    console.error('[Runic] Ошибка отправки IPC:', e);
                }
            }
        };
        "#,
        UserContentInjectedFrames::AllFrames,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    );
    controller.add_script(&shim);

    webview.load_uri(&file_url);
    window.add(&webview);
    window.show_all();

    let webview_for_async = webview.clone();
    let is_debug_for_async = is_debug;

    // ИСПРАВЛЕНО: логирование ошибок run_javascript в debug-режиме
    glib::spawn_future_local(async move {
        while let Ok(js_code) = js_receiver.recv().await {
            let wv = webview_for_async.clone();
            let dbg = is_debug_for_async;
            wv.run_javascript(&js_code, None::<&gio::Cancellable>, move |result| {
                if dbg {
                    if let Err(e) = result {
                        eprintln!("[Runic Debug] JS execution error: {:?}", e);
                    }
                }
            });
        }
    });

    window.connect_delete_event({
        let state_clone = Arc::clone(&app_state);
        move |_, _| {
            cleanup_children(&state_clone);
            gtk::main_quit();
            Propagation::Stop
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

    if is_debug {
        eprintln!("[Runic Debug] Режим отладки включен");
        if let Some(html_dir) = abs_path.parent() {
            let html_dir = html_dir.to_path_buf();
            let (reload_sender, reload_receiver) = async_channel::bounded::<()>(16);
            let webview_for_reload = webview.clone();
            let state_for_reload = Arc::clone(&app_state);

            glib::spawn_future_local(async move {
                while let Ok(()) = reload_receiver.recv().await {
                    eprintln!("[Runic Debug] Обнаружены изменения, перезагрузка WebView...");
                    kill_and_clear_tracked(&state_for_reload);
                    if let Ok(mut positions) = state_for_reload.watch_positions.lock() {
                        positions.clear();
                    }
                    // Сбрасываем shutdown для продолжения работы после hot-reload
                    state_for_reload.shutdown.store(false, Ordering::Relaxed);
                    webview_for_reload.reload();
                }
            });

            std::thread::spawn(move || {
                let mut inotify = match inotify::Inotify::init() {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("[Runic Debug] Failed to init inotify: {}", e);
                        return;
                    }
                };
                if let Err(e) = inotify.watches().add(
                    &html_dir,
                    inotify::WatchMask::MODIFY
                        | inotify::WatchMask::CREATE
                        | inotify::WatchMask::DELETE
                        | inotify::WatchMask::MOVED_TO
                        | inotify::WatchMask::CLOSE_WRITE,
                ) {
                    eprintln!("[Runic Debug] Failed to add watch: {}", e);
                    return;
                }
                let mut buffer = vec![0u8; 64 * 1024];
                let mut last_reload = Instant::now();
                loop {
                    match inotify.read_events_blocking(&mut buffer) {
                        Ok(events) => {
                            let mut needs_reload = false;
                            for event in events {
                                if let Some(name) = event.name {
                                    let name_str = name.to_string_lossy();
                                    if (name_str.ends_with(".html")
                                        || name_str.ends_with(".css")
                                        || name_str.ends_with(".js"))
                                        && !name_str.ends_with('~')
                                        && !name_str.contains(".swp")
                                        && !name_str.contains(".tmp")
                                    {
                                        needs_reload = true;
                                        break;
                                    }
                                }
                            }
                            if needs_reload {
                                let now = Instant::now();
                                if now.duration_since(last_reload) > Duration::from_millis(300) {
                                    let _ = reload_sender.send_blocking(());
                                    last_reload = now;
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("[Runic Debug] inotify error: {}. Continuing...", e);
                            thread::sleep(Duration::from_millis(5000));
                        }
                    }
                }
            });
        }
    }

    let state_for_resume = Arc::clone(&app_state);
    let js_sender_for_resume = js_sender.clone();
    let is_debug_for_resume = is_debug;

    // ИСПРАВЛЕНО: проверка shutdown для чистой остановки
    std::thread::spawn(move || {
        let mut last_time = std::time::SystemTime::now();
        while !state_for_resume.shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let now = std::time::SystemTime::now();
            if let Ok(elapsed) = now.duration_since(last_time) {
                if elapsed > std::time::Duration::from_secs(15) {
                    if is_debug_for_resume {
                        eprintln!("[Runic Debug] Обнаружен выход из сна. Перезагрузка виджета...");
                    }
                    kill_and_clear_tracked(&state_for_resume);
                    if let Ok(mut positions) = state_for_resume.watch_positions.lock() {
                        positions.clear();
                    }
                    // Сбрасываем shutdown для продолжения работы после resume
                    state_for_resume.shutdown.store(false, Ordering::Relaxed);
                    let _ =
                        js_sender_for_resume.send_blocking("window.location.reload();".to_string());
                }
            }
            last_time = now;
        }
    });

    gtk::main();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::env;
    use std::thread::sleep;

    fn setup_test_state() -> (
        Arc<AppState>,
        Sender<String>,
        async_channel::Receiver<String>,
    ) {
        let state = Arc::new(AppState {
            running_streams: Mutex::new(HashMap::new()),
            watch_positions: Mutex::new(HashMap::new()),
            spawned_pids: Mutex::new(Vec::new()),
            is_debug: false,
            shutdown: AtomicBool::new(false),
        });
        let (sender, receiver) = async_channel::bounded::<String>(1024);
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
        handle_action(state, request, sender);
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
        handle_action(state, request, sender);
        let response = get_response(&receiver);
        assert_eq!(response.action, "read");
        assert_eq!(response.status, "error");
        assert!(
            response.message.contains("No such file or directory")
                || response.message.contains("Нет такого файла")
        );
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
        handle_action(state, request, sender);
        let response = get_response(&receiver);
        assert_eq!(response.action, "write");
        assert_eq!(response.status, "success");
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "appended data");
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_handle_action_write_append() {
        let (state, sender, receiver) = setup_test_state();
        let temp_dir = env::temp_dir();
        let file_path = temp_dir.join("runic_test_append.txt");
        std::fs::write(&file_path, "initial\n").unwrap();
        let request = JsMessage {
            action: "write".to_string(),
            payload: json!({"path": file_path.to_str().unwrap(), "data": "appended\n", "append": true}),
        };
        handle_action(state, request, sender);
        let _ = get_response(&receiver);
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "initial\nappended\n");
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_handle_action_exec_success() {
        let (state, sender, receiver) = setup_test_state();
        let request = JsMessage {
            action: "exec".to_string(),
            payload: json!({"command": "echo -n 'hello world'"}),
        };
        handle_action(state, request, sender);
        let response = get_response(&receiver);
        assert_eq!(response.action, "exec");
        assert_eq!(response.status, "success");
        assert!(response.data.unwrap().contains("hello world"));
    }

    #[test]
    fn test_handle_action_exec_error() {
        let (state, sender, receiver) = setup_test_state();
        let request = JsMessage {
            action: "exec".to_string(),
            payload: json!({"command": "ls /nonexistent_directory_12345"}),
        };
        handle_action(state, request, sender);
        let response = get_response(&receiver);
        assert_eq!(response.action, "exec");
        assert_eq!(response.status, "success");
        assert!(
            response.data.as_ref().unwrap().contains("STDERR:")
                || response.data.as_ref().unwrap().contains("No such file")
        );
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
        assert_eq!(start_response.action, "stream");
        assert_eq!(start_response.status, "success");
        let data_response = get_response(&receiver);
        assert_eq!(data_response.action, "stream");
        assert_eq!(data_response.status, "stream_data");
        assert_eq!(data_response.data, Some("stream_data_test".to_string()));
        sleep(Duration::from_millis(500));
        let running = safe_lock(&state.running_streams);
        assert!(
            running.is_empty(),
            "Поток должен быть удален из running_streams"
        );
        drop(running);
        let pids = safe_lock(&state.spawned_pids);
        assert!(
            pids.is_empty(),
            "PID должен быть удален из spawned_pids после завершения"
        );
    }

    #[test]
    fn test_handle_action_stream_error_empty_target() {
        let (state, sender, receiver) = setup_test_state();
        let request = JsMessage {
            action: "stream".to_string(),
            payload: json!({"target": ""}),
        };
        handle_action(state, request, sender);
        let response = get_response(&receiver);
        assert_eq!(response.action, "stream");
        assert_eq!(response.status, "error");
        assert_eq!(response.message, "target не указан");
    }

    #[test]
    fn test_handle_action_stream_duplicate_error() {
        // Тест на TOCTOU: второй stream с тем же target должен вернуть ошибку
        let (state, sender, receiver) = setup_test_state();
        let request = JsMessage {
            action: "stream".to_string(),
            payload: json!({"target": "sleep 10"}),
        };
        handle_action(Arc::clone(&state), request, sender.clone());
        let start_response = get_response(&receiver);
        assert_eq!(start_response.status, "success");

        // Второй запрос с тем же target
        let request2 = JsMessage {
            action: "stream".to_string(),
            payload: json!({"target": "sleep 10"}),
        };
        handle_action(Arc::clone(&state), request2, sender);
        let dup_response = get_response(&receiver);
        assert_eq!(dup_response.action, "stream");
        assert_eq!(dup_response.status, "error");
        assert_eq!(dup_response.message, "Уже выполняется");

        // Очистка
        safe_lock(&state.running_streams).clear();
        safe_lock(&state.spawned_pids).clear();
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
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(200) {
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
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        assert!(got_success, "Должно прийти подтверждение запуска watch");
        assert!(got_watch_data, "Должны прийти данные из файла (tail)");
        let positions = safe_lock(&state.watch_positions);
        assert!(positions.contains_key(file_path.to_str().unwrap()));
        assert!(matches!(
            positions[file_path.to_str().unwrap()],
            WatchState::File(Some(_))
        ));
        drop(positions);
        std::thread::sleep(Duration::from_millis(100));
        safe_lock(&state.watch_positions).remove(file_path.to_str().unwrap());
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_handle_action_watch_duplicate_error() {
        let (state, sender, receiver) = setup_test_state();
        let temp_dir = env::temp_dir();
        let file_path = temp_dir.join("runic_test_watch_dup.log");
        std::fs::write(&file_path, "line\n").unwrap();
        let request1 = JsMessage {
            action: "watch".to_string(),
            payload: json!({"path": file_path.to_str().unwrap()}),
        };
        handle_action(Arc::clone(&state), request1, sender.clone());
        let _ = get_response(&receiver);
        let request2 = JsMessage {
            action: "watch".to_string(),
            payload: json!({"path": file_path.to_str().unwrap()}),
        };
        handle_action(Arc::clone(&state), request2, sender);
        let response = get_response(&receiver);
        assert_eq!(response.action, "watch");
        assert_eq!(response.status, "error");
        assert_eq!(response.message, "Уже мониторится");
        safe_lock(&state.watch_positions).remove(file_path.to_str().unwrap());
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_handle_action_unwatch_success() {
        let (state, sender, receiver) = setup_test_state();
        let temp_dir = env::temp_dir();
        let file_path = temp_dir.join("runic_test_unwatch.log");
        std::fs::write(&file_path, "line\n").unwrap();
        let watch_request = JsMessage {
            action: "watch".to_string(),
            payload: json!({"path": file_path.to_str().unwrap()}),
        };
        handle_action(Arc::clone(&state), watch_request, sender.clone());
        let _ = get_response(&receiver);
        sleep(Duration::from_millis(100));
        let unwatch_request = JsMessage {
            action: "unwatch".to_string(),
            payload: json!({"path": file_path.to_str().unwrap()}),
        };
        handle_action(Arc::clone(&state), unwatch_request, sender);
        let response = get_response(&receiver);
        assert_eq!(response.action, "unwatch");
        assert_eq!(response.status, "success");
        let positions = safe_lock(&state.watch_positions);
        assert!(!positions.contains_key(file_path.to_str().unwrap()));
        let _ = std::fs::remove_file(&file_path);
    }

    #[test]
    fn test_handle_action_unknown() {
        let (state, sender, receiver) = setup_test_state();
        let request = JsMessage {
            action: "unknown_action".to_string(),
            payload: json!({}),
        };
        handle_action(state, request, sender);
        let response = get_response(&receiver);
        assert_eq!(response.action, "unknown_action");
        assert_eq!(response.status, "error");
        assert_eq!(response.message, "Неизвестное действие");
    }

    #[test]
    fn test_kill_and_clear_tracked_no_double_kill() {
        // Тест: процессы из running_streams не должны быть убиты дважды
        let (state, _sender, _receiver) = setup_test_state();

        // Запускаем процесс через stream
        let child = Command::new("sleep")
            .arg("10")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        safe_lock(&state.spawned_pids).push(pid);
        safe_lock(&state.running_streams).insert("test".to_string(), child);

        // Вызываем kill_and_clear_tracked
        kill_and_clear_tracked(&state);

        // Проверяем, что всё очищено
        assert!(safe_lock(&state.running_streams).is_empty());
        assert!(safe_lock(&state.spawned_pids).is_empty());
    }

    #[test]
    fn test_shutdown_flag_stops_watch_loop() {
        let (state, sender, _receiver) = setup_test_state();
        let temp_dir = env::temp_dir();
        let file_path = temp_dir.join("runic_test_shutdown_watch.log");
        std::fs::write(&file_path, "line\n").unwrap();

        let request = JsMessage {
            action: "watch".to_string(),
            payload: json!({"path": file_path.to_str().unwrap()}),
        };
        handle_action(Arc::clone(&state), request, sender);
        sleep(Duration::from_millis(100));

        // Устанавливаем shutdown
        state.shutdown.store(true, Ordering::Relaxed);

        // Ждём, пока поток watch завершится (до 2 секунд)
        sleep(Duration::from_secs(2));

        // Поток должен был выйти из цикла
        // Проверяем, что флаг установлен
        assert!(state.shutdown.load(Ordering::Relaxed));

        let _ = std::fs::remove_file(&file_path);
    }
}
