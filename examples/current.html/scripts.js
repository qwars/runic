document.addEventListener("contextmenu", (e) => e.preventDefault());

// ═══════════════════════════════════════════════════════════════
// ЧАСЫ
// ═══════════════════════════════════════════════════════════════
const time = new Intl.DateTimeFormat("ru-RU", {
  hour: "2-digit",
  minute: "2-digit",
});
const date = new Intl.DateTimeFormat("ru-RU", {
  day: "numeric",
  month: "long",
});
const day = new Intl.DateTimeFormat("ru-RU", { weekday: "long" });

const elTime = document.getElementById("clock-time");
const elDate = document.getElementById("clock-date");
const elDay = document.getElementById("clock-day");

let lastDate = null;

function update() {
  const now = new Date();
  const d = now.getDate();

  elTime.textContent = time.format(now);

  if (lastDate !== d) {
    elDate.textContent = date.format(now);
    elDay.textContent = day.format(now);
    lastDate = d;
  }

  setTimeout(update, (60 - now.getSeconds()) * 1000);
}

update();

// ═══════════════════════════════════════════════════════════════
// УТИЛИТЫ
// ═══════════════════════════════════════════════════════════════

function formatBytes(bytes) {
  const b = parseInt(bytes, 10);
  if (isNaN(b) || b < 0) return "0 B";

  const units = ["B", "Kb", "Mb", "Gb", "Tb"];
  let value = b;
  let unitIndex = 0;

  while (value >= 1000 && unitIndex < units.length - 1) {
    value /= 1000;
    unitIndex++;
  }

  return value.toFixed(unitIndex === 0 ? 0 : 2) + " " + units[unitIndex];
}

function formatSpeed(mbit) {
  if (mbit >= 1000) {
    return (mbit / 1000).toFixed(1) + " Gbit/s";
  }
  return mbit + " Mbit/s";
}

function sendToRust(action, payload) {
  const msg = JSON.stringify({ action, payload });
  if (window.ipc && typeof window.ipc.postMessage === "function") {
    window.ipc.postMessage(msg);
  } else {
    console.error("[Runic] Не удалось отправить: IPC недоступен");
  }
}

// ═══════════════════════════════════════════════════════════════
// РАДИО-ПЛЕЕР
// ═══════════════════════════════════════════════════════════════

const urlsPlayer = [
  {
    url: "https://houseworld.stream.laut.fm/houseworld",
    icon: "houseworld.png",
  },
  {
    url: "https://play.sas-media.ru/play_256",
    icon: "https://noisefm.ru/wp-content/themes/NoiseFM/assets/img/logo.svg",
  },
  {
    url: "http://listen.uturnradio.com:7000/dubstep",
    icon: "https://www.uturnradio.com/images/logo_1.png",
  },
  {
    url: "https://stream.deadmoonradio.com/listen/live/DeadMoonRadio.mp3",
    icon: "deadmoonradio.png",
  },
  {
    url: "http://198.15.94.34:8018/stream/1/",
    icon: "https://www.dubstep.fm/images/dsfm_cover.jpg",
  },
  {
    url: "https://securestreams7.autopo.st/?uri=http://208.115.202.71:8407/stream",
    icon: "https://dubstep.fm/favicon.ico",
  },
  {
    url: "http://ice2.somafm.com/dubstep-256-mp3",
    icon: "https://somafm.com/logos/400/dubstep400.png",
  },
  {
    url: "https://radiorecord.hostingradio.ru/darkside96.aacp",
    icon: "darkside.png",
  },
];
let radioUrl = urlsPlayer[0];

const targetPlayer = () => `
  TARGET_DIR="\${HOME:-/home/qwars}/.cache"

  if [ -f "\$TARGET_DIR/mpv.pid" ]; then
    OLD_PID=\$(cat "\$TARGET_DIR/mpv.pid" 2>/dev/null)
    if [ -n "\$OLD_PID" ] && [ -d "/proc/\$OLD_PID" ] && grep -q awk "/proc/\$OLD_PID/cmdline" 2>/dev/null; then
      kill "\$OLD_PID" 2>/dev/null || true
    fi
    rm -f "\$TARGET_DIR/mpv.pid"
  fi

  if [ -z "${radioUrl.url}" ]; then
    rm -f "\$TARGET_DIR/.streamtitle"
    exit 0
  fi

  mpv --no-config --cache-secs=60 --demuxer-max-bytes=100MiB --cache-pause=yes --title="Runic Radio" "${radioUrl.url}" 2>&1 |
  awk -v outfile="\$TARGET_DIR/.streamtitle" '
    tolower(\$0) ~ /icy-title:/ {
      sub(/.*[Ii][Cc][Yy]-[Tt][Ii][Tt][Ll][Ee]:[ \\t]*/, "")
      print > outfile
      fflush()
    }
  ' >/dev/null 2>&1 &
  echo \$! > "\$TARGET_DIR/mpv.pid"
`;

let timeoutPlayer;

function payerPlayPause(t) {
  if (t)
    radioUrl = urlsPlayer[urlsPlayer.indexOf(radioUrl) + 1] || urlsPlayer[0];
  else radioUrl = { url: "" };

  const musicTitle = document.getElementById("music-title");
  musicTitle.removeAttribute("style");
  musicTitle.lastElementChild.textContent = "";
  if (radioUrl.url) musicTitle.style.backgroundImage = `url(${radioUrl.icon})`;

  sendToRust("exec", { command: targetPlayer() });
}

function payerIPTVPlayPause(command) {
  sendToRust("exec", { command: command });
}

// ═══════════════════════════════════════════════════════════════
// ОБНОВЛЕНИЕ UI
// ═══════════════════════════════════════════════════════════════

function updateNetData(ssid, freq, signal, txRate, rxRate, rxBytes, txBytes) {
  document.getElementById("net-name").textContent =
    ssid + " • " + freq + " MHz";

  document.getElementById("net-speed-up").textContent =
    formatSpeed(txRate) + " • " + formatBytes(txBytes);

  document.getElementById("net-speed-down").textContent =
    formatSpeed(rxRate) + " • " + formatBytes(rxBytes);

  document.getElementById("wifi-signal").textContent = signal + " dBm";

  const statusWIFI = document.getElementById("wifi-status");
  statusWIFI.removeAttribute("class");
  if (signal < -71) statusWIFI.classList.add("error");
  else if (signal < -61) statusWIFI.classList.add("warn");
  else if (signal < -51) statusWIFI.classList.add("main");
  else statusWIFI.classList.add("full");
}

function updateTopData(datastate) {
  document.querySelectorAll("#top-processes td").forEach((td, i) => {
    td.textContent = datastate[i];
  });
}

function updateBattaryStatus(datastate) {
  const rect = document.getElementById("battery");
  rect.setAttribute("width", Math.round(151 * (datastate / 100)));
  rect.removeAttribute("class");
  if (datastate <= 10) rect.classList.add("error");
  else if (datastate <= 30) rect.classList.add("warn");
}

function updateBattaryHeadsetEnergy(datastate) {
  const status = document.getElementById("headset-status");
  status.lastElementChild.textContent = status.firstElementChild.textContent =
    "";

  if (datastate[3]) {
    status.firstElementChild.textContent = datastate[3];
    status.lastElementChild.textContent = datastate[1];
  }
}

// ═══════════════════════════════════════════════════════════════
// ДИСПЕТЧЕР WATCH-СОБЫТИЙ
// ═══════════════════════════════════════════════════════════════

const watchHandlers = new Map();

function registerWatch(path, handler) {
  watchHandlers.set(path, handler);
  sendToRust("watch", { path, tail: handler.initialTail || 1 });
}

// ═══════════════════════════════════════════════════════════════
// РЕГИСТРАЦИЯ НАБЛЮДАТЕЛЕЙ (ТОЛЬКО ДЛЯ .streamtitle)
// ═══════════════════════════════════════════════════════════════

// Инициализация плеера
payerPlayPause();

// Название трека — оптимизация через watch
registerWatch("/home/qwars/.cache/.streamtitle", {
  initialTail: 1,
  onData: (data) => {
    document.getElementById("music-title").lastElementChild.textContent = data;
  },
});

// Директория Evolution Mail — ТРИГГЕР для exec
registerWatch("/home/qwars/.cache/evolution/mail", {
  onData: () => {
    sendToRust("exec", {
      command: `find /home/qwars/.cache/evolution/mail -name "folders.db" 2>/dev/null | while read db; do
        sqlite3 "$db" "SELECT COALESCE(SUM(unread_count),0) FROM folders WHERE folder_name LIKE '%Inbox%' OR folder_name LIKE '%INBOX%';" 2>/dev/null
      done | awk '{sum+=$1} END {print sum+0}'`,
    });
  },
});

// RSS база Evolution — ТРИГГЕР для exec
registerWatch("/home/qwars/.local/share/evolution/mail/rss/folders.db", {
  onData: () => {
    sendToRust("exec", {
      command: `sqlite3 ~/.local/share/evolution/mail/rss/folders.db "SELECT SUM(unread_count) FROM folders;" 2>/dev/null || echo 0`,
    });
  },
});

// Org-mode задачи — ТРИГГЕР для exec
registerWatch("/home/qwars/.emacs.d/org-tasks", {
  onData: () => {
    sendToRust("exec", {
      command: `TODO=$(grep -r '^\\*+ TODO' ~/.emacs.d/org-tasks/ 2>/dev/null | wc -l)
OVERDUE=$(grep -r 'DEADLINE:' ~/.emacs.d/org-tasks/ 2>/dev/null | awk -F'DEADLINE: <' '{print $2}' | awk -F'>' -v today="$(date +%Y-%m-%d)" '$1 < today' | wc -l)
echo $((TODO + OVERDUE))`,
    });
  },
});

// ═══════════════════════════════════════════════════════════════
// STREAM-ЗАПРОСЫ (ВОЗВРАЩАЕМ РАБОЧИЙ ПОДХОД ИЗ СТАРОГО СКРИПТА)
// ═══════════════════════════════════════════════════════════════

// ТЕМПЕРАТУРА: 15с — поиск hwmon по имени
const targetTemp = `while true; do
  CPU_TEMP=""; SSD_TEMP=""
  for d in /sys/class/hwmon/hwmon*; do
    name=\$(cat "\$d/name" 2>/dev/null)
    if [ "\$name" = "k10temp" ] && [ -f "\$d/temp1_input" ]; then
      CPU_TEMP=\$(cat "\$d/temp1_input")
    elif [ "\$name" = "nvme" ] && [ -f "\$d/temp1_input" ]; then
      SSD_TEMP=\$(cat "\$d/temp1_input")
    fi
  done
  echo "\${CPU_TEMP:-0};\${SSD_TEMP:-0}"
  sleep 15
done`;

// СЕТЬ: 15с
const targetNet = `while true; do
  IFACE=$(/sbin/iw dev | awk '/Interface/{print $2; exit}')
  /sbin/iw dev "$IFACE" link | awk -v iface="$IFACE" -v OFS=";" '
    BEGIN {
      rx=0; tx=0
      cmd="cat /sys/class/net/"iface"/statistics/rx_bytes"
      cmd | getline rx; close(cmd)
      cmd="cat /sys/class/net/"iface"/statistics/tx_bytes"
      cmd | getline tx; close(cmd)
    }
    /SSID/ {s=$2}
    /freq/ {f=$2}
    /signal/ {si=$2}
    /tx bitrate/ {txr=$3}
    /rx bitrate/ {rxr=$3}
    END {print s, f, si, txr, rxr, rx, tx}
  '
  sleep 15
done`;

// CPU: 15с
const targetCpuUsage = `while true; do
  awk '/^cpu / {
    usage = ($2+$4)*100/($2+$4+$5)
    printf "%.0f%%\\n", usage
    exit
  }' /proc/stat
  sleep 15
done`;

// ПРОЦЕССЫ: 30с
const targetProcessesCount = `while true; do
  ls -d /proc/[0-9]* 2>/dev/null | wc -l | awk '{print $1"/"$1}'
  sleep 30
done`;

// ДИСК: 300с
const targetDiskUsage = `while true; do
  df -h / | awk 'NR==2 {printf "%s/%s\\n", $4, $2}'
  sleep 300
done`;

// VOLUME: 10с
const targetVolumeState = `while true; do
  wpctl get-volume @DEFAULT_AUDIO_SINK@ | awk '{printf "%.0f%%\\n", $2 * 100}'
  sleep 10
done`;

const targetIsVolumeState = `while true; do
  wpctl get-volume @DEFAULT_AUDIO_SINK@ | grep -q MUTED && echo "muted" || echo "unmuted"
  sleep 5
done`;

// MICROPHONE: 10с
const targetMicrophoneState = `while true; do
  wpctl get-volume @DEFAULT_AUDIO_SOURCE@ | awk '{printf "%.0f%%\\n", $2 * 100}'
  sleep 10
done`;

const targetIsMicrophoneState = `while true; do
  wpctl get-volume @DEFAULT_AUDIO_SOURCE@ | grep -q MUTED && echo "muted" || echo "unmuted"
  sleep 5
done`;

// TOR: 300с
const targetTorStatus = `while true; do
  if curl --socks5-hostname 127.0.0.1:9050 -s --max-time 5 https://check.torproject.org/api/ip | grep -q '"IsTor":true'; then
    echo "⎈"
  else
    echo "∅"
  fi
  sleep 300
done`;

// ПОГОДА: 3600с
const targetWeatherStatus = `while true; do
  DATA=$(curl -s -m 3 'wttr.in/?format=1&lang=ru')
  if [ -n "$DATA" ] && [ "$DATA" != "Unknown location;" ]; then 
    echo "$DATA"
    sleep 3600
  else 
    sleep 10
  fi
done`;

// ГАРНИТУРА: 120с
const targetBattaryHeadsetEnergy = `while true; do
  echo $(upower -i $(upower -e | grep headset | head -n 1) | grep -E "percentage|model")
  sleep 120
done`;

// ═══════════════════════════════════════════════════════════════
// ВОЗВРАЩАЕМ РАБОЧИЕ СКРИПТЫ ИЗ СТАРОЙ ВЕРСИИ
// ═══════════════════════════════════════════════════════════════

// СТАТУС БАТАРЕИ / ПИТАНИЯ (как в старом скрипте)
const targetBattaryStatus = `while true; do
  AC_ONLINE=$(cat /sys/class/power_supply/AC/online 2>/dev/null || cat /sys/class/power_supply/ACAD/online 2>/dev/null)
  if [ "$AC_ONLINE" = "1" ]; then
    echo "⚡"
  else
    CAPACITY=$(cat /sys/class/power_supply/BAT0/capacity 2>/dev/null || cat /sys/class/power_supply/BAT1/capacity 2>/dev/null)
    echo "$CAPACITY%"
  fi
  sleep 60
done`;

// ЗАРЯД БАТАРЕИ (как в старом скрипте)
const targetBattaryEnergy = `while true; do
  CAPACITY=$(cat /sys/class/power_supply/BAT0/capacity 2>/dev/null || cat /sys/class/power_supply/BAT1/capacity 2>/dev/null)
  echo "$CAPACITY"
  sleep 10
done`;

// ПАМЯТЬ (как в старом скрипте — через free -m)
const targetMemUsage = `while true; do
  free -m | awk '/^Mem:/ {printf "%.1fGb\\n", $7/1024}'
  sleep 60
done`;

// SWAP (как в старом скрипте — через free -m)
const targetSwapUsage = `while true; do
  free -m | awk '/^Swap:/ {printf "%.1fGb\\n", $2/1024}'
  sleep 120
done`;

// UPTIME (разовый exec)
const targetUptime = `awk '{
  t=$1
  w=int(t/604800); t%=604800
  d=int(t/86400); t%=86400
  h=int(t/3600)
  printf "%s%s%sh\\n", (w>0?w"w ":""), (d>0?d"d ":""), h
}' /proc/uptime`;

// ПИТАНИЕ (детали: V и mA — как в старом скрипте)
const targetPowerStatus = `read v < /sys/class/power_supply/BAT0/voltage_now
read c < /sys/class/power_supply/BAT0/current_now
awk -v v="$v" -v c="$c" 'BEGIN {
  printf "%.2f V | %.0f mA\\n", v/1000000, c/1000
}'`;

// ═══════════════════════════════════════════════════════════════
// ОБРАБОТЧИК ОТВЕТОВ (ВОЗВРАЩАЕМ СТАРУЮ ЛОГИКУ)
// ═══════════════════════════════════════════════════════════════

window.onRunicResponse = function (response) {
  try {
    if (response.status === "error") {
      console.error(`[Runic Error] ${response.action}: ${response.message}`);
      return;
    }

    // === WATCH: маршрутизируем через диспетчер ===
    if (response.action === "watch" && response.status === "watch_data") {
      const handler = watchHandlers.get(response.message);
      if (handler.onData) {
        handler.onData(response.data);
      }
      return;
    }

    // === EXEC: обработка результатов команд ===
    if (response.action === "exec") {
      const msg = response.message;

      if (msg.includes("folders.db") && msg.includes("Inbox")) {
        document.getElementById("emails").textContent = response.data.trim();
        return;
      }
      if (msg.includes("rss/folders.db") && msg.includes("unread_count")) {
        document.getElementById("news").textContent = response.data.trim();
        return;
      }
      if (msg.includes("org-tasks") && msg.includes("DEADLINE")) {
        document.getElementById("todos").textContent = response.data.trim();
        return;
      }
      if (msg.includes("/proc/uptime")) {
        document.getElementById("uptime").textContent = response.data.trim();
        return;
      }
      if (msg.includes("voltage_now") || msg.includes("current_now")) {
        document.getElementById("power-status").textContent =
          response.data.trim();
        return;
      }
      return;
    }

    // === STREAM: обработка потоков (как в старом скрипте) ===
    if (response.action === "stream") {
      if (response.message.includes(targetNet))
        updateNetData(...response.data.split(";"));
      else if (response.message.includes(targetTemp)) {
        const [cpu, ssd] = response.data.split(";");
        const cpuC =
          cpu && cpu !== "0" ? (parseInt(cpu) / 1000).toFixed(0) : "--";
        const ssdC =
          ssd && ssd !== "0" ? (parseInt(ssd) / 1000).toFixed(0) : "--";
        document.getElementById("temp-sensors").textContent =
          `${cpuC}°C | ${ssdC}°C`;
      } else if (response.message.includes(targetBattaryStatus))
        document.getElementById("battery-status").textContent = response.data;
      else if (response.message.includes(targetBattaryEnergy))
        updateBattaryStatus(response.data);
      else if (response.message.includes(targetBattaryHeadsetEnergy))
        updateBattaryHeadsetEnergy(response.data.split(/\s+/));
      else if (response.message.includes(targetCpuUsage))
        document.getElementById("cpu-usage").textContent = response.data.trim();
      else if (response.message.includes(targetProcessesCount))
        document.getElementById("processes-count").textContent =
          response.data.trim();
      else if (response.message.includes(targetMemUsage))
        document.getElementById("mem-usage").textContent = response.data.trim();
      else if (response.message.includes(targetSwapUsage))
        document.getElementById("swap-usage").textContent =
          response.data.trim();
      else if (response.message.includes(targetDiskUsage))
        document.getElementById("disk-usage").textContent =
          response.data.trim();
      else if (response.message.includes(targetVolumeState))
        document.getElementById("volume-status").textContent =
          response.data.trim();
      else if (response.message.includes(targetIsVolumeState))
        response.data === "muted"
          ? document.getElementById("volume-state").classList.add("is-muted")
          : document
              .getElementById("volume-state")
              .classList.remove("is-muted");
      else if (response.message.includes(targetMicrophoneState))
        document.getElementById("microphone-status").textContent =
          response.data.trim();
      else if (response.message.includes(targetIsMicrophoneState))
        response.data !== "muted"
          ? document
              .getElementById("microphone-state")
              .classList.add("is-muted")
          : document
              .getElementById("microphone-state")
              .classList.remove("is-muted");
      else if (response.message.includes(targetTorStatus))
        document.getElementById("tor-status").textContent =
          response.data.trim();
      else if (response.message.includes(targetWeatherStatus))
        document.getElementById("weather-status").textContent =
          response.data.trim();
    }
  } catch (e) {
    console.error("[Runic] Ошибка обработки ответа:", e, response);
  }
};

// ═══════════════════════════════════════════════════════════════
// ЗАПУСК STREAM-ПОТОКОВ
// ═══════════════════════════════════════════════════════════════

sendToRust("stream", { target: targetTemp });
sendToRust("stream", { target: targetNet });
sendToRust("stream", { target: targetCpuUsage });
sendToRust("stream", { target: targetProcessesCount });
sendToRust("stream", { target: targetMemUsage });
sendToRust("stream", { target: targetSwapUsage });
sendToRust("stream", { target: targetDiskUsage });
sendToRust("stream", { target: targetVolumeState });
sendToRust("stream", { target: targetIsVolumeState });
sendToRust("stream", { target: targetMicrophoneState });
sendToRust("stream", { target: targetIsMicrophoneState });
sendToRust("stream", { target: targetTorStatus });
sendToRust("stream", { target: targetWeatherStatus });
sendToRust("stream", { target: targetBattaryStatus });
sendToRust("stream", { target: targetBattaryEnergy });
sendToRust("stream", { target: targetBattaryHeadsetEnergy });

// Разовые exec-запросы с периодическим повтором
setInterval(
  () => sendToRust("exec", { command: targetUptime }),
  60 * 1000 * 60 * 3,
);
sendToRust("exec", { command: targetUptime });

setInterval(
  () => sendToRust("exec", { command: targetPowerStatus }),
  60 * 1000 * 5,
);
sendToRust("exec", { command: targetPowerStatus });
