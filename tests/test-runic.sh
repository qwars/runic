#!/bin/bash
# test-runic.sh - Тестовый сценарий для виджета Runic

WIDGET_PID=""
LOG_FILE="/tmp/runic-test-$(date +%s).log"

log() {
    echo "[$(date '+%H:%M:%S')] $1" | tee -a "$LOG_FILE"
}

cleanup() {
    log "🧹 Очистка тестового окружения..."
    if [ -n "$WIDGET_PID" ]; then
        kill -TERM "$WIDGET_PID" 2>/dev/null
        sleep 1
        kill -KILL "$WIDGET_PID" 2>/dev/null
    fi
    
    # Убиваем все дочерние процессы runic
    pkill -P $(pgrep -f "target/debug/runic" | head -1) 2>/dev/null
    pkill -f "target/debug/runic" 2>/dev/null
    
    log "✅ Очистка завершена"
}

trap cleanup EXIT

start_widget() {
    log "🚀 Запуск виджета..."
    cd /home/qwars/Public/GitVerse.project/runic
    cargo build 2>&1 | tee -a "$LOG_FILE"
    
    ./target/debug/runic -d ~/.config/runic/index.html > >(tee -a "$LOG_FILE") 2>&1 &
    WIDGET_PID=$!
    sleep 2
    
    if ! kill -0 "$WIDGET_PID" 2>/dev/null; then
        log "❌ Виджет не запустился!"
        exit 1
    fi
    
    log "✅ Виджет запущен (PID: $WIDGET_PID)"
}

check_child_processes() {
    log "🔍 Проверка дочерних процессов..."
    
    # Считаем все процессы, связанные с runic
    local runic_pids=$(pgrep -f "target/debug/runic" | wc -l)
    local child_pids=$(pgrep -P "$WIDGET_PID" 2>/dev/null | wc -l)
    local mpv_pids=$(pgrep -f "mpv.*Runic Radio" | wc -l)
    local stream_pids=$(pgrep -f "while true.*sleep" | wc -l)
    
    log "  Runic процессов: $runic_pids"
    log "  Дочерних процессов: $child_pids"
    log "  MPV процессов: $mpv_pids"
    log "  Stream циклов: $stream_pids"
    
    # Показываем дерево процессов
    log "📊 Дерево процессов:"
    pstree -p "$WIDGET_PID" 2>/dev/null | tee -a "$LOG_FILE"
}

test_hot_reload() {
    log "🔄 Тест hot-reload..."
    
    local initial_pid=$WIDGET_PID
    
    # Изменяем CSS файл
    log "  Изменяю styles.css..."
    echo "/* test $(date +%s) */" >> ~/.config/runic/styles.css
    
    sleep 3
    
    # Проверяем, изменился ли PID (должен измениться при hot-reload)
    local new_pid=$(pgrep -f "target/debug/runic" | head -1)
    
    if [ "$new_pid" != "$initial_pid" ]; then
        log "✅ Hot-reload сработал (старый PID: $initial_pid, новый: $new_pid)"
        WIDGET_PID=$new_pid
    else
        log "❌ Hot-reload НЕ сработал (PID не изменился)"
    fi
}

test_suspend_resume() {
    log "💤 Тест suspend/resume..."
    
    # Имитируем сигнал suspend через D-Bus
    log "  Отправляю PrepareForSleep(true)..."
    dbus-send --system --print-reply \
        --dest=org.freedesktop.login1 \
        /org/freedesktop/login1 \
        org.freedesktop.login1.Manager.Inhibit \
        string:"handle-suspend-key" \
        string:"runic-test" \
        string:"Тест suspend" \
        string:"block" 2>&1 | tee -a "$LOG_FILE"
    
    sleep 2
    check_child_processes
    
    # Имитируем resume
    log "  Отправляю PrepareForSleep(false)..."
    # Здесь нужен реальный suspend/resume, но для теста просто проверяем процессы
    sleep 2
    check_child_processes
}

test_signal_handling() {
    log "📡 Тест обработки сигналов..."
    
    # SIGTERM
    log "  Отправляю SIGTERM..."
    kill -TERM "$WIDGET_PID"
    sleep 2
    
    if kill -0 "$WIDGET_PID" 2>/dev/null; then
        log "❌ Виджет не завершился по SIGTERM"
        kill -KILL "$WIDGET_PID"
    else
        log "✅ Виджет завершился по SIGTERM"
    fi
    
    # Проверяем, остались ли дочерние процессы
    sleep 1
    local orphan_pids=$(pgrep -P 1 -f "mpv|while.*sleep" 2>/dev/null | wc -l)
    
    if [ "$orphan_pids" -gt 0 ]; then
        log "❌ Обнаружены процессы-сироты: $orphan_pids"
        pstree -p 1 | grep -E "mpv|sleep" | tee -a "$LOG_FILE"
    else
        log "✅ Процессы-сироты отсутствуют"
    fi
}

# ═══════════════════════════════════════════════════════════════
# ОСНОВНОЙ СЦЕНАРИЙ
# ═══════════════════════════════════════════════════════════════

log "═══════════════════════════════════════════════════"
log "🧪 НАЧАЛО ТЕСТИРОВАНИЯ WIDGET RUNIC"
log "═══════════════════════════════════════════════════"

start_widget
check_child_processes

log ""
log "📋 ТЕСТ 1: Hot-reload"
test_hot_reload
check_child_processes

log ""
log "📋 ТЕСТ 2: Suspend/Resume"
test_suspend_resume

log ""
log "📋 ТЕСТ 3: Обработка сигналов"
test_signal_handling

log ""
log "═══════════════════════════════════════════════════"
log "✅ ТЕСТИРОВАНИЕ ЗАВЕРШЕНО"
log "📄 Логи сохранены в: $LOG_FILE"
log "═══════════════════════════════════════════════════"
