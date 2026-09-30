# Runic

Runic — легковесный движок для отображения HTML/CSS/JS виджетов на рабочем столе Linux. Альтернатива Conky с современным веб-стеком: пишите виджеты на привычных HTML, CSS и JavaScript, а Runic обеспечит их интеграцию с системой через прозрачный IPC-мост.

Построен на Rust + GTK3 + WebKitGTK + Layer Shell (Wayland).

## Возможности

- **Настоящий веб-рендеринг** — HTML5, CSS3, современный JavaScript через WebKitGTK
- **Layer Shell интеграция** — виджеты располагаются под всеми окнами (или на других слоях)
- **Прозрачный фон** — виджеты органично вписываются в рабочий стол
- **Мощный IPC-мост** — 4 действия из коробки: чтение/запись файлов, выполнение команд, потоковый вывод
- **Гибкое позиционирование** — размер, координаты, полноэкранный режим
- **Тихий режим** — по умолчанию не засоряет логи, отладка включается флагом `--debug`
- **Асинхронный стриминг** — вывод долгоиграющих команд поступает в виджет в реальном времени
- **Hot-reload** — автоматическая перезагрузка виджета при изменении `.html`, `.css` или `.js` файлов (в режиме `--debug`)

## Требования

- **ОС**: Linux с Wayland-композитором (Sway, Hyprland, Wayfire и т.п.)
- **Системные библиотеки**: GTK 3.24+, WebKitGTK 4.0, gtk-layer-shell
- **Rust**: 1.70+ (для сборки)

## Установка

### 1. Системные зависимости

**Debian 13 / Ubuntu:**
```bash
sudo apt install -y \
    build-essential pkg-config \
    libgtk-3-dev libwebkit2gtk-4.0-dev \
    libgtk-layer-shell-dev
```

**Arch Linux:**
```bash
sudo pacman -S gtk3 webkit2gtk gtk-layer-shell
```

**Fedora:**
```bash
sudo dnf install gtk3-devel webkit2gtk4.0-devel gtk-layer-shell-devel
```

### 2. Сборка

```bash
git clone <repository-url>
cd runic
cargo build --release
```

Бинарник появится в `target/release/runic`.

## Использование

### Базовый запуск

```bash
# Виджет на весь экран (по умолчанию)
./target/release/runic examples/test.html/index.html

# Фиксированный размер 400×300 в левом верхнем углу
./target/release/runic examples/test.html/index.html -w 400 -H 300

# С отступом от краёв
./target/release/runic examples/test.html/index.html -w 400 -H 300 -x 50 -y 100

# Режим отладки (подробный вывод в терминал)
./target/release/runic examples/test.html/index.html --debug
```

### Параметры командной строки

| Параметр | Описание | По умолчанию |
|---|---|---|
| `html_path` | Путь к HTML-файлу виджета | (обязательно) |
| `-w, --width <N>` | Ширина окна в пикселях | на весь экран |
| `-H, --height <N>` | Высота окна в пикселях | на весь экран |
| `-x, --x <N>` | Отступ от левого края | 0 |
| `-y, --y <N>` | Отступ от верхнего края | 0 |
| `-d, --debug` | Включить подробный вывод | false |

> **Примечание:** координаты `-x` и `-y` работают только если задан размер окна (`-w` и `-H`).

### Автозапуск в Sway

Добавьте в `~/.config/sway/config`:

```
exec /home/user/runic/target/release/runic /home/user/runic/widgets/clock.html -w 300 -H 150 -x 50 -y 50
```

## API для виджетов

Runic предоставляет JavaScript-объект `window.ipc` для взаимодействия с системой.

### Отправка сообщения

```javascript
window.ipc.postMessage(JSON.stringify({
    action: "exec",
    payload: { command: "uname -a" }
}));
```

### Получение ответа

Определите глобальный обработчик:

```javascript
window.onRunicResponse = function(response) {
    // response.action  — имя действия
    // response.status  — "success" | "error" | "stream_data"
    // response.message — текстовое сообщение
    // response.data    — полезные данные (строка или null)
    console.log(response);
};
```

### Доступные действия

#### `read` — чтение файла

```json
{ "action": "read", "payload": { "path": "/etc/os-release" } }
```

Ответ: `data` содержит содержимое файла.

#### `write` — дозапись в файл

```json
{ "action": "write", "payload": { "path": "/tmp/log.txt", "data": "строка\n" } }
```

Родительские директории создаются автоматически. Файл открывается в режиме `append`.

#### `exec` — выполнение команды

```json
{ "action": "exec", "payload": { "command": "df -h" } }
```

Ответ: `data` содержит `stdout` при успехе или `STDERR:\n...` при ошибке. Команда выполняется через `sh -c`.

#### `stream` — потоковое выполнение

```json
{ "action": "stream", "payload": { "target": "ping -c 10 8.8.8.8" } }
```

Ответ приходит несколько раз, по одной строке за раз, со статусом `stream_data`. Идеально для `top`, `ping`, `tail -f` и других долгоиграющих команд. Повторный запуск той же команды блокируется, пока предыдущая не завершится.


## Режим отладки

Флаг `--debug` включает:

- Вывод всех IPC-сообщений в терминал
- Перенаправление `console.log` из JS в stdout
- Информацию о размерах и позиционировании
- **Hot-reload**: автоматическая перезагрузка виджета при изменении файлов `.html`, `.css`, `.js` в директории виджета (защита от дребезга — 300 мс)

```bash
cargo run -- examples/test.html/index.html --debug
```

## Лицензия

MIT

## Благодарности

Проект вдохновлён [Conky](https://github.com/brndnmtthws/conky) и построен на отличных Rust-привязках:

- [gtk-rs](https://github.com/gtk-rs/gtk3-rs)
- [webkit2gtk-rs](https://github.com/nicokosi/webkit2gtk-rs)
- [gtk-layer-shell-rs](https://github.com/Smithay/gtk-layer-shell-rs)
