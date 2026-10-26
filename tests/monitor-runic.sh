#!/bin/bash
# monitor-runic.sh - Мониторинг виджета в реальном времени

watch -n 1 '
echo "═══════════════════════════════════════════════════"
echo "📊 МОНИТОРИНГ WIDGET RUNIC"
echo "═══════════════════════════════════════════════════"
echo ""
echo "🔹 Основные процессы:"
pgrep -af "target/debug/runic" | head -5
echo ""
echo "🔹 Дочерние процессы (mpv, streams):"
pgrep -af "mpv.*Runic|while.*sleep.*15|while.*sleep.*10" | head -10
echo ""
echo "🔹 Дерево процессов:"
pstree -p $(pgrep -f "target/debug/runic" | head -1) 2>/dev/null | head -20
echo ""
echo "🔹 Использование ресурсов:"
ps -p $(pgrep -f "target/debug/runic" | head -1) -o pid,ppid,%cpu,%mem,vsz,rss,etime 2>/dev/null
echo ""
echo "🔹 Открытые файлы (pipes):"
lsof -p $(pgrep -f "target/debug/runic" | head -1) 2>/dev/null | grep -E "pipe|FIFO" | head -10
echo ""
echo "═══════════════════════════════════════════════════"
'
