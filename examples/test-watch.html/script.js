const monitoringState = {
    active: new Set()
};

function toggleMonitor(btn) {
    const path = btn.dataset.path;
    if (monitoringState.active.has(path)) {
        stopMonitoring(path);
    } else {
        startMonitoring(path, 10);
    }
}

function startMonitoring(path, tail = 10) {
    if (monitoringState.active.has(path)) {
        logToUI(`⚠️ ${path} уже мониторится`, 'warning');
        return;
    }

    window.ipc.postMessage({
        action: "watch",
        payload: { path, tail }
    });

    monitoringState.active.add(path);
    updateUIState();
    logToUI(`✅ Запущен мониторинг: ${path}`, 'success');
}

function startMonitoring(path, tail = 10) {
    if (monitoringState.active.has(path)) {
        logToUI(`⚠️ ${path} уже мониторится`, 'warning');
        return;
    }

    window.ipc.postMessage(JSON.stringify({
        action: "watch",
        payload: { path, tail }
    }));

    monitoringState.active.add(path);
    updateUIState();
    logToUI(`✅ Запущен мониторинг: ${path}`, 'success');
}

function stopMonitoring(path) {
    if (!monitoringState.active.has(path)) {
        logToUI(`⚠️ ${path} не мониторится`, 'warning');
        return;
    }

    window.ipc.postMessage(JSON.stringify({
        action: "unwatch",
        payload: { path }
    }));

    monitoringState.active.delete(path);
    updateUIState();
    logToUI(`🛑 Остановлен мониторинг: ${path}`, 'info');
}
function updateUIState() {
    document.querySelectorAll('.monitor-btn').forEach(btn => {
        const path = btn.dataset.path;
        if (monitoringState.active.has(path)) {
            btn.classList.add('active');
            btn.textContent = `⏹ Остановить ${btn.dataset.name}`;
        } else {
            btn.classList.remove('active');
            btn.textContent = `▶ Запустить ${btn.dataset.name}`;
        }
    });
}

function logToUI(message, type = 'info') {
    const logContainer = document.getElementById('log-container');
    const entry = document.createElement('div');
    entry.className = `log-entry log-${type}`;

    const time = new Date().toLocaleTimeString();
    entry.innerHTML = `<span class="timestamp">[${time}]</span> ${escapeHtml(message)}`;

    logContainer.appendChild(entry);
    logContainer.scrollTop = logContainer.scrollHeight;
}

function escapeHtml(text) {
    const div = document.createElement('div');
    div.textContent = text;
    return div.innerHTML;
}

window.onRunicResponse = function(response) {
    if (response.action === "watch") {
        if (response.status === "success") {
            logToUI(`Мониторинг запущен: ${response.message}`, 'success');
        } else if (response.status === "error") {
            logToUI(`Ошибка мониторинга: ${response.message} ${response.data ? '(' + response.data + ')' : ''}`, 'error');
        } else if (response.status === "watch_data") {
            const logLine = response.data;
            const filePath = response.message;

            let type = 'info';
            let prefix = 'ℹ️';

            if (filePath.includes("auth.log")) {
                if (logLine.includes("Failed password")) {
                    type = 'error'; prefix = '🚨 [AUTH FAIL]';
                } else if (logLine.includes("sudo")) {
                    type = 'warning'; prefix = '🔐 [SUDO]';
                } else if (logLine.includes("Accepted")) {
                    type = 'success'; prefix = '✅ [AUTH OK]';
                } else {
                    prefix = '🔐 [AUTH]';
                }
            } else if (filePath.includes("kern.log")) {
                const lower = logLine.toLowerCase();
                if (lower.includes("error") || lower.includes("fail")) {
                    type = 'error'; prefix = '❌ [KERNEL ERROR]';
                } else if (logLine.includes("amdgpu")) {
                    type = 'info'; prefix = '🎮 [GPU]';
                } else if (logLine.includes("rtw_8822ce")) {
                    type = 'info'; prefix = '📶 [WIFI]';
                } else {
                    prefix = '🐧 [KERNEL]';
                }
            } else if (filePath.includes("syslog")) {
                const lower = logLine.toLowerCase();
                if (lower.includes("error") || lower.includes("fail")) {
                    type = 'error'; prefix = '❌ [SYSLOG ERROR]';
                } else if (lower.includes("networkmanager")) {
                    type = 'info'; prefix = '🌐 [NETWORK]';
                } else if (lower.includes("bluetooth")) {
                    type = 'info'; prefix = '🔵 [BLUETOOTH]';
                } else {
                    prefix = 'ℹ️ [SYSLOG]';
                }
            } else if (filePath.includes("tor")) {
                prefix = '🧅 [TOR]';
            } else {
                prefix = '📄 [LOG]';
            }

            logToUI(`${prefix} ${logLine}`, type);
        }
    } else if (response.action === "unwatch") {
        if (response.status === "success") {
            logToUI(`Мониторинг остановлен: ${response.message}`, 'info');
        } else {
            logToUI(`Ошибка остановки: ${response.message}`, 'error');
        }
    }
};

document.addEventListener('DOMContentLoaded', () => {
    updateUIState();
});
