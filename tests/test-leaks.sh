#!/bin/bash
# test-leaks.sh - Тест на утечки ресурсов

log() {
    echo "[$(date '+%H:%M:%S')] $1"
}

cd ~/Public/GitVerse.project/runic
./target/debug/runic -d ~/.config/runic/index.html > /dev/null 2>&1 &
WIDGET_PID=$!

log "Виджет запущен (PID: $WIDGET_PID)"
sleep 3

# Начальные показатели
INITIAL_MEM=$(ps -p $WIDGET_PID -o rss= | tr -d ' ')
INITIAL_FDS=$(ls /proc/$WIDGET_PID/fd 2>/dev/null | wc -l)

log "Начальная память: ${INITIAL_MEM} KB"
log "Начальные FD: $INITIAL_FDS"

# Цикл hot-reload
for i in {1..20}; do
    echo "/* iteration $i */" >> ~/.config/runic/styles.css
    sleep 2
    
    # Получаем новый PID после hot-reload
    NEW_PID=$(pgrep -f "target/debug/runic" | head -1)
    
    if [ -n "$NEW_PID" ]; then
        CURRENT_MEM=$(ps -p $NEW_PID -o rss= | tr -d ' ')
        CURRENT_FDS=$(ls /proc/$NEW_PID/fd 2>/dev/null | wc -l)
        
        log "Итерация $i: PID=$NEW_PID, MEM=${CURRENT_MEM}KB, FD=$CURRENT_FDS"
        
        WIDGET_PID=$NEW_PID
    else
        log "❌ Виджет упал на итерации $i"
        exit 1
    fi
done

# Финальные показатели
FINAL_MEM=$(ps -p $WIDGET_PID -o rss= | tr -d ' ')
FINAL_FDS=$(ls /proc/$WIDGET_PID/fd 2>/dev/null | wc -l)

log ""
log "═══════════════════════════════════════════════════"
log "РЕЗУЛЬТАТЫ ТЕСТА УТЕЧЕК"
log "═══════════════════════════════════════════════════"
log "Память: ${INITIAL_MEM}KB → ${FINAL_MEM}KB (Δ: $((FINAL_MEM - INITIAL_MEM))KB)"
log "FD: $INITIAL_FDS → $FINAL_FDS (Δ: $((FINAL_FDS - INITIAL_FDS)))"

# Критерии
MEM_DELTA=$((FINAL_MEM - INITIAL_MEM))
FD_DELTA=$((FINAL_FDS - INITIAL_FDS))

if [ $MEM_DELTA -gt 50000 ]; then
    log "❌ УТЕЧКА ПАМЯТИ: рост более 50MB"
elif [ $FD_DELTA -gt 20 ]; then
    log "❌ УТЕЧКА FD: рост более 20 дескрипторов"
else
    log "✅ Утечки не обнаружены"
fi

# Очистка
kill -TERM $WIDGET_PID
