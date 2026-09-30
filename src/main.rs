use async_channel::Sender;
use clap::Parser;
use glib::Propagation;
use gtk::prelude::*;
use gtk_layer_shell::{
    Edge, Layer, init_for_window, set_anchor, set_exclusive_zone, set_keyboard_interactivity,
    set_layer, set_margin, set_namespace,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{self, Command, Stdio};
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
    /// Путь к HTML-файлу виджета
    html_path: String,
    /// Ширина окна (если не задана, окно растягивается на весь экран)
    #[arg(short = 'w', long)]
    width: Option<i32>,
    /// Высота окна (если не задана, окно растягивается на весь экран)
    #[arg(short = 'H', long)]
    height: Option<i32>,
    /// Координата X от левого края (работает только если задан размер)
    #[arg(short = 'x', long, default_value = "0")]
    x: i32,
    /// Координата Y от верхнего края (работает только если задан размер)
    #[arg(short = 'y', long, default_value = "0")]
    y: i32,
    /// Включить подробный вывод в терминал (отладка)
    #[arg(short = 'd', long, default_value = "false")]
    debug: bool,
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

struct AppState {
    running_streams: Mutex<HashSet<String>>,
    is_debug: bool,
}

fn handle_action(state: Arc<AppState>, request: JsMessage, js_sender: Sender<String>) {
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
                    let _ = fs::create_dir_all(parent);
                }
                match OpenOptions::new().create(true).append(true).open(path) {
                    Ok(mut file) => match file.write_all(d.as_bytes()).and_then(|_| file.flush()) {
                        Ok(_) => send_js("write", "success", "OK", None),
                        Err(e) => send_js("write", "error", &format!("{}", e), None),
                    },
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
            {
                let mut running = state.running_streams.lock().unwrap();
                if !running.insert(target.clone()) {
                    send_js("stream", "error", "Уже выполняется", None);
                    return;
                }
            }
            let target_clone = target.clone();
            let state_clone = Arc::clone(&state);
            let sender_clone = js_sender.clone();
            thread::spawn(move || {
                let child = Command::new("sh")
                    .arg("-c")
                    .arg(&target_clone)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn();
                match child {
                    Ok(mut proc) => {
                        if let Some(stdout) = proc.stdout.take() {
                            let reader = BufReader::new(stdout);
                            for line in reader.lines() {
                                match line {
                                    Ok(l) => {
                                        let response = RustResponse {
                                            action: "stream".to_string(),
                                            status: "stream_data".to_string(),
                                            message: target_clone.clone(),
                                            data: Some(l),
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
                        }
                        let _ = proc.wait();
                    }
                    Err(e) => {
                        let response = RustResponse {
                            action: "stream".to_string(),
                            status: "error".to_string(),
                            message: format!("EXEC ERROR: {}", e),
                            data: Some(target_clone.clone()),
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
                state_clone
                    .running_streams
                    .lock()
                    .unwrap()
                    .remove(&target_clone);
            });
            send_js("stream", "success", "Запущен", None);
        }
        _ => send_js(&request.action, "error", "Неизвестное действие", None),
    }
}

fn main() {
    let args = Args::parse();
    let html_path = args.html_path.clone();
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

    if let Some(screen) = gtk::prelude::GtkWindowExt::screen(&window)
        && let Some(visual) = screen.rgba_visual()
    {
        window.set_visual(Some(&visual));
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
            if is_debug {
                eprintln!("[Runic Debug] Размер: {}x{}, Позиция: ({}, {})", w, h, x, y);
            }
        }
        _ => {
            set_anchor(&window, Edge::Top, true);
            set_anchor(&window, Edge::Left, true);
            set_anchor(&window, Edge::Right, true);
            set_anchor(&window, Edge::Bottom, true);
            if is_debug {
                eprintln!("[Runic Debug] Размер: на весь экран");
            }
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
        running_streams: Mutex::new(HashSet::new()),
        is_debug,
    });

    let (js_sender, js_receiver) = async_channel::unbounded::<String>();
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
                } else if state_clone.is_debug {
                    eprintln!("[Runic Debug] Ошибка парсинга JSON: {}", msg_str);
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
    glib::spawn_future_local(async move {
        while let Ok(js_code) = js_receiver.recv().await {
            webview_for_async.run_javascript(&js_code, None::<&gio::Cancellable>, |_| ());
        }
    });

    window.connect_delete_event(|_, _| {
        gtk::main_quit();
        Propagation::Stop
    });

    if is_debug {
        eprintln!("[Runic Debug] Режим отладки включен");
        if let Some(html_dir) = abs_path.parent() {
            let html_dir = html_dir.to_path_buf();
            let (reload_sender, reload_receiver) = async_channel::unbounded::<()>();
            let webview_for_reload = webview.clone();
            glib::spawn_future_local(async move {
                while let Ok(()) = reload_receiver.recv().await {
                    eprintln!("[Runic Debug] Обнаружены изменения, перезагрузка WebView...");
                    webview_for_reload.reload();
                }
            });
            eprintln!(
                "[Runic Debug] Hot-reload включен для: {}",
                html_dir.display()
            );
            std::thread::spawn(move || {
                let mut inotify = inotify::Inotify::init().expect("Failed to init inotify");
                inotify
                    .watches()
                    .add(
                        &html_dir,
                        inotify::WatchMask::MODIFY
                            | inotify::WatchMask::CREATE
                            | inotify::WatchMask::DELETE
                            | inotify::WatchMask::MOVED_TO
                            | inotify::WatchMask::CLOSE_WRITE,
                    )
                    .expect("Failed to add watch");
                let mut buffer = [0; 1024];
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
                            eprintln!("[Runic Debug] inotify error: {}", e);
                            break;
                        }
                    }
                }
            });
        }
    }

    gtk::main();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::env;

    fn setup_test_state() -> (
        Arc<AppState>,
        Sender<String>,
        async_channel::Receiver<String>,
    ) {
        let state = Arc::new(AppState {
            running_streams: Mutex::new(HashSet::new()),
            is_debug: false,
        });
        let (sender, receiver) = async_channel::unbounded::<String>();
        (state, sender, receiver)
    }

    fn get_response(receiver: &async_channel::Receiver<String>) -> RustResponse {
        let js_code = receiver.recv_blocking().expect("Должно прийти сообщение");
        // Извлекаем JSON из JS-обертки
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
            payload: json!({"path": "/nonexistent/path/file.txt"}),
        };

        handle_action(state, request, sender);

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

        handle_action(state, request, sender);

        let response = get_response(&receiver);
        assert_eq!(response.action, "write");
        assert_eq!(response.status, "success");

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "appended data");

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
}
