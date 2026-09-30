document.addEventListener("contextmenu", (e) => e.preventDefault());

const time = new Intl.DateTimeFormat("ru-RU", {
  hour: "2-digit",
  minute: "2-digit",
});
const date = new Intl.DateTimeFormat("ru-RU", {
  day: "2-digit",
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

const urlsPlayer = [
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
    icon: "",
  }, // https://deadmoonradio.com/ - Без иконки и картинок
  {
    url: "http://198.15.94.34:8018/stream/1/",
    icon: "https://www.dubstep.fm/images/dsfm_cover.jpg",
  },
  {
    url: "https://securestreams7.autopo.st/?uri=http://208.115.202.71:8407/stream",
    icon: "https://dubstep.fm/favicon.ico",
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

  mpv --no-config --title="Runic Radio" "${radioUrl.url}" 2>&1 |
  awk -v outfile="\$TARGET_DIR/.streamtitle" '
    tolower(\$0) ~ /icy-title:/ {
      sub(/.*[Ii][Cc][Yy]-[Tt][Ii][Tt][Ll][Ee]:[ \\t]*/, "")
      print > outfile
      fflush()
    }
  ' >/dev/null 2>&1 &
  echo \$! > "\$TARGET_DIR/mpv.pid"
`;

function payerPlayPause(el) {
  if (el instanceof HTMLElement)
    radioUrl = urlsPlayer[urlsPlayer.indexOf(radioUrl) + 1] || urlsPlayer[0];
  else radioUrl = { url: "" };

  const musicTitle = document.getElementById("music-title");
  musicTitle.removeAttribute("style");
  musicTitle.textContent = "";
  if (radioUrl.url) musicTitle.style.backgroundImage = `url(${radioUrl.icon})`;

  sendToRust("exec", {
    command: targetPlayer(),
  });
}

function updateNetData(ssid, freq, signal, txRate, rxRate, rxBytes, txBytes) {
  document.getElementById("net-name").textContent = ssid + " • " + freq;
  document.getElementById("net-speed-up").textContent =
    formatBytes(rxRate) + "t" + " • " + formatBytes(rxBytes);
  document.getElementById("net-speed-down").textContent =
    formatBytes(txRate) + "t" + " • " + formatBytes(txBytes);
  document.getElementById("wifi-signal").textContent = signal + "dBm";
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
  if (datastate <= 30) rect.classList.add("warn");
}

// ═══════════════════════════════════════════════════════════════
// СЕТЬ
// ═══════════════════════════════════════════════════════════════
const targetNet = `while true; do
  IFACE=$(/sbin/iw dev | awk '/Interface/{print $2; exit}')
  /sbin/iw dev "$IFACE" link | awk -v iface="$IFACE" -v OFS=";" '
    BEGIN {
      rx=0; tx=0
      cmd="cat /sys/class/net/"iface"/statistics/rwx_bytes"
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
  sleep 10
done`;

// ═══════════════════════════════════════════════════════════════
// ТЕМПЕРАТУРА
// ═══════════════════════════════════════════════════════════════
const targetTempSensors = `while true; do
  sensors | awk '
    /Tctl:/ {cpu=$2}
    /Composite:/ {ssd=$2}
    END {print cpu, "|", ssd}
  '
  sleep 15
done`;

// ═══════════════════════════════════════════════════════════════
// CPU
// ═══════════════════════════════════════════════════════════════
const targetCpuUsage = `while true; do
  awk '/^cpu / {
    usage = ($2+$4)*100/($2+$4+$5)
    printf "%.0f%%\\n", usage
    exit
  }' /proc/stat
  sleep 7
done`;

// ═══════════════════════════════════════════════════════════════
// ПРОЦЕССЫ
// ═══════════════════════════════════════════════════════════════
const targetProcessesCount = `while true; do
  ls -d /proc/[0-9]* 2>/dev/null | wc -l | awk '{print $1"/"$1}'
  sleep 10
done`;

// ═══════════════════════════════════════════════════════════════
// UPTIME
// ═══════════════════════════════════════════════════════════════
const targetUptime = `awk '{
  t=$1
  w=int(t/604800); t%=604800
  d=int(t/86400); t%=86400
  h=int(t/3600)
  printf "%s%s%sh\\n", (w>0?w"w ":""), (d>0?d"d ":""), h
}' /proc/uptime`;

// ═══════════════════════════════════════════════════════════════
// ПИТАНИЕ
// ═══════════════════════════════════════════════════════════════
const targetPowerStatus = `read v < /sys/class/power_supply/BAT0/voltage_now
read c < /sys/class/power_supply/BAT0/current_now
awk -v v="$v" -v c="$c" 'BEGIN {
  printf "%.2f V | %.0f mA\\n", v/1000000, c/1000
}'`;

// ═══════════════════════════════════════════════════════════════
// ПАМЯТЬ
// ═══════════════════════════════════════════════════════════════
const targetMemUsage = `while true; do
  free -m | awk '/^Mem:/ {printf "%.1fGb\\n", $7/1024}'
  sleep 15
done`;

// ═══════════════════════════════════════════════════════════════
// SWAP
// ═══════════════════════════════════════════════════════════════
const targetSwapUsage = `while true; do
  free -m | awk '/^Swap:/ {printf "%.1fGb\\n", $2/1024}'
  sleep 30
done`;

// ═══════════════════════════════════════════════════════════════
// ДИСК
// ═══════════════════════════════════════════════════════════════
const targetDiskUsage = `while true; do
  df -h / | awk 'NR==2 {printf "%s/%s\\n", $4, $2}'
  sleep 60
done`;

// ═══════════════════════════════════════════════════════════════
// ТАБЛИЦА ПРОЦЕССОВ
// ═══════════════════════════════════════════════════════════════
const targetTableTop = `while true; do
  top -b -n 1 | awk 'NR>7 && NR<=15 {printf "%-15s %5s %5s\\n", $12, $9, $10}' | grep -v -E '^(runic|top|ps)' | xargs
  sleep 5  # ВАЖНО: sleep 1 с командой top создаст высокую нагрузку на CPU. 5 сек - оптимальный компромисс.
done`;

// ═══════════════════════════════════════════════════════════════
// EMAILS
// ═══════════════════════════════════════════════════════════════
const targetEmails = `while true; do
  find /home/qwars/.cache/evolution/mail -name "folders.db" 2>/dev/null | while read db; do
    sqlite3 "$db" "SELECT COALESCE(SUM(unread_count),0) FROM folders WHERE folder_name LIKE '%Inbox%' OR folder_name LIKE '%INBOX%';" 2>/dev/null
  done | awk '{sum+=$1} END {print sum+0}'
  sleep 300
done`;

// ═══════════════════════════════════════════════════════════════
// NEWS
// ═══════════════════════════════════════════════════════════════
const targetNews = `while true; do
  sqlite3 ~/.local/share/evolution/mail/rss/folders.db "SELECT SUM(unread_count) FROM folders;" 2>/dev/null || echo 0
  sleep 60
done`;

// ═══════════════════════════════════════════════════════════════
// TODOS
// ═══════════════════════════════════════════════════════════════
const targetTodos = `while true; do
  TODO=$(grep -r '^\\*+ TODO' ~/.emacs.d/org-tasks/ 2>/dev/null | wc -l)
  OVERDUE=$(grep -r 'DEADLINE:' ~/.emacs.d/org-tasks/ 2>/dev/null | awk -F'DEADLINE: <' '{print $2}' | awk -F'>' -v today="$(date +%Y-%m-%d)" '$1 < today' | wc -l)
  echo $((TODO + OVERDUE))
  sleep 30
done`;

// ═══════════════════════════════════════════════════════════════
// VOLUME
// ═══════════════════════════════════════════════════════════════
const targetVolumeState = `while true; do
  wpctl get-volume @DEFAULT_AUDIO_SINK@ | awk '{printf "%.0f%%\\n", $2 * 100}'
  sleep 5
done`;

const targetIsVolumeState = `while true; do
  wpctl get-volume @DEFAULT_AUDIO_SINK@ | grep -q MUTED && echo "muted" || echo "unmuted"
  sleep 2
done`;

const targetMicrophoneState = `while true; do
  wpctl get-volume @DEFAULT_AUDIO_SOURCE@ | awk '{printf "%.0f%%\\n", $2 * 100}'
  sleep 5
done`;

const targetIsMicrophoneState = `while true; do
  wpctl get-volume @DEFAULT_AUDIO_SOURCE@ | grep -q MUTED && echo "muted" || echo "unmuted"
  sleep 2
done`;

function changeMicrophoneState(step) {
  const state = parseInt(
    document.getElementById("microphone-status").textContent,
  );
  if (state < 150 && state > 0) {
    sendToRust("exec", {
      command: `wpctl set-volume @DEFAULT_AUDIO_SOURCE@ ${step}`,
    });
    if (state > 0 && step.includes("-"))
      document.getElementById("microphone-status").textContent =
        `${state - 5}%`;
    else if (state < 150)
      document.getElementById("microphone-status").textContent =
        `${state + 5}%`;
  }
}

// ═══════════════════════════════════════════════════════════════
// TOR STATUS
// ═══════════════════════════════════════════════════════════════
const targetTorStatus = `while true; do
  if curl --socks5-hostname 127.0.0.1:9050 -s --max-time 5 https://check.torproject.org/api/ip | grep -q '"IsTor":true'; then
    echo "⎈"
  else
    echo "⦸"
  fi
  sleep 60
done`;

// ═══════════════════════════════════════════════════════════════
// ПОГОДА
// ═══════════════════════════════════════════════════════════════
const targetWeatherStatus = `while true; do
  DATA=$(curl -s -m 3 'wttr.in/?format=1&lang=ru')
  if [ -n "$DATA" ] && [ "$DATA" != "Unknown location;" ]; then 
    echo "$DATA"
    sleep 3600
  else 
    sleep 10
  fi
done`;

// ═══════════════════════════════════════════════════════════════
// СТАТУС БАТАРЕИ / ПИТАНИЯ
// ═══════════════════════════════════════════════════════════════
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

const targetBattaryEnergy = `while true; do
    CAPACITY=$(cat /sys/class/power_supply/BAT0/capacity 2>/dev/null || cat /sys/class/power_supply/BAT1/capacity 2>/dev/null)
    echo "$CAPACITY"
  sleep 10
done`;

const readStreamtitle = `while true; do cat ~/.cache/.streamtitle; sleep 3; done`;

window.onRunicResponse = function (response) {
  try {
    if (response.status === "error") {
      console.error(`[Runic Error] ${response.action}: ${response.message}`);
    } else {
      if (response.message.includes(targetNet))
        updateNetData(...response.data.split(";"));
      else if (response.message.includes(targetTableTop))
        updateTopData(response.data.split(/\s+/));
      else if (response.message.includes(targetBattaryStatus))
        document.getElementById("battery-status").textContent = response.data;
      else if (response.message.includes(targetBattaryEnergy))
        updateBattaryStatus(response.data);
      else if (response.message.includes(readStreamtitle))
        document.getElementById("music-title").textContent = response.data;
      else if (response.message.includes(targetTempSensors))
        document.getElementById("temp-sensors").textContent = response.data;
      else if (response.message.includes(targetCpuUsage))
        document.getElementById("cpu-usage").textContent = response.data;
      else if (response.message.includes(targetProcessesCount))
        document.getElementById("processes-count").textContent = response.data;
      else if (response.message.includes(targetUptime))
        document.getElementById("uptime").textContent = response.data;
      else if (response.message.includes(targetMemUsage))
        document.getElementById("mem-usage").textContent = response.data;
      else if (response.message.includes(targetSwapUsage))
        document.getElementById("swap-usage").textContent = response.data;
      else if (response.message.includes(targetDiskUsage))
        document.getElementById("disk-usage").textContent = response.data;
      else if (response.message.includes(targetPowerStatus))
        document.getElementById("power-status").textContent = response.data;
      else if (response.message.includes(targetTodos))
        document.getElementById("todos").textContent = response.data;
      else if (response.message.includes(targetNews))
        document.getElementById("news").textContent = response.data;
      else if (response.message.includes(targetEmails))
        document.getElementById("emails").textContent = response.data;
      else if (response.message.includes(targetVolumeState))
        document.getElementById("volume-status").textContent = response.data;
      else if (response.message.includes(targetIsVolumeState))
        response.data !== "muted"
          ? document.getElementById("volume-state").classList.add("is-muted")
          : document
              .getElementById("volume-state")
              .classList.remove("is-muted");
      else if (response.message.includes(targetMicrophoneState))
        document.getElementById("microphone-status").textContent =
          response.data;
      else if (response.message.includes(targetIsMicrophoneState))
        response.data !== "muted"
          ? document
              .getElementById("microphone-state")
              .classList.add("is-muted")
          : document
              .getElementById("microphone-state")
              .classList.remove("is-muted");
      else if (response.message.includes(targetTorStatus))
        document.getElementById("tor-status").textContent = response.data;
      else if (response.message.includes(targetWeatherStatus))
        document.getElementById("weather-status").textContent = response.data;
    }
  } catch (e) {
    console.error("[Runic] Ошибка обработки ответа:", e, response);
  }
};

function sendToRust(action, payload) {
  const msg = JSON.stringify({ action, payload });
  if (window.ipc && typeof window.ipc.postMessage === "function") {
    window.ipc.postMessage(msg);
  } else {
    console.error("[Runic] Не удалось отправить: IPC недоступен");
  }
}

sendToRust("stream", {
  target: targetNet,
});

sendToRust("stream", {
  target: targetTempSensors,
});

sendToRust("stream", {
  target: targetCpuUsage,
});

sendToRust("stream", {
  target: targetProcessesCount,
});
sendToRust("stream", {
  target: targetMemUsage,
});
sendToRust("stream", {
  target: targetSwapUsage,
});

sendToRust("stream", {
  target: targetDiskUsage,
});

sendToRust("stream", {
  target: targetTableTop,
});

sendToRust("stream", {
  target: targetVolumeState,
});
sendToRust("stream", {
  target: targetIsVolumeState,
});
sendToRust("stream", {
  target: targetMicrophoneState,
});
sendToRust("stream", {
  target: targetIsMicrophoneState,
});

setInterval(
  () =>
    sendToRust("exec", {
      command: targetUptime,
    }),
  60 * 1000 * 60 * 3,
);
sendToRust("exec", {
  command: targetUptime,
});

setInterval(
  () =>
    sendToRust("exec", {
      command: targetPowerStatus,
    }),
  60 * 1000 * 2,
);

sendToRust("exec", {
  command: targetPowerStatus,
});

sendToRust("stream", {
  target: targetEmails,
});

sendToRust("stream", {
  target: targetNews,
});

sendToRust("stream", {
  target: targetTodos,
});

sendToRust("stream", {
  target: targetTorStatus,
});

sendToRust("stream", {
  target: targetWeatherStatus,
});

sendToRust("stream", { target: targetBattaryStatus });
sendToRust("stream", { target: targetBattaryEnergy });

payerPlayPause();
sendToRust("stream", { target: readStreamtitle });
