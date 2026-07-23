import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ─────────────────────────────────────────────────────────────────────────────
// Мессенджер: весь звук/DSP — на стороне Rust (sonic-protocol/sonic-audio). Здесь
// только UI: старт сессии, отправка текста, отрисовка входящих сообщений и телеметрии
// качества связи, приходящей событиями (см. events.rs / session.rs).
// ─────────────────────────────────────────────────────────────────────────────

type Role = "initiator" | "responder";
type Profile = "audible" | "ultrasonic";
type ModePolicy = "auto" | "css" | "mfsk" | "ofdm-qpsk" | "ofdm-qam";
type SelfTestMode = "css" | "mfsk" | "ofdm-qpsk" | "ofdm-qam";

interface MessageReceived {
  text: string;
}
interface MessageStatus {
  msg_id: number;
  status: "sent" | "delivered";
  text: string;
}
interface LinkQuality {
  snr_db: number;
  mode: string;
  retransmits: number;
  rtt_ms: number;
  frames_ok: number;
  frames_bad: number;
  per: number;
  in_flight: number;
}
interface SessionStateChanged {
  state: "up" | "down";
}
interface AudioDevices {
  inputs: string[];
  outputs: string[];
  default_input: string | null;
  default_output: string | null;
}

let selectedRole: Role = "initiator";
let selectedProfile: Profile = "audible";
let sessionUp = false;
// Сопоставление msg_id → DOM-элемент исходящего пузыря (для обновления статуса).
const outgoing = new Map<number, HTMLElement>();

const $ = <T extends HTMLElement>(sel: string) => document.querySelector<T>(sel);

// ── Маскот: живой индикатор состояния канала ─────────────────────────────────
// idle — слушаем фон, jump — хендшейк/подключение, angry — ошибка/сбой,
// sad — связь потеряна, happy — сообщение доставлено (кратковременно).

type Mood = "idle" | "jump" | "angry" | "sad" | "happy";
let mascotRevertTimer: ReturnType<typeof setTimeout> | undefined;

// Подпись под маскотом — словами описывает текущий статус передачи.
const MOOD_CAPTIONS: Record<Mood, string> = {
  idle: "На связи · слушаю эфир",
  jump: "Устанавливаю соединение…",
  angry: "Сбой передачи",
  sad: "Связь потеряна",
  happy: "Сообщение доставлено",
};

function setMascotMood(mood: Mood, revertMs?: number) {
  const el = $("#mascot-avatar");
  if (el) el.className = `mascot-avatar mascot-avatar-lg mood-${mood}`;
  const card = $("#mascot-card");
  if (card) card.className = `mascot-card mood-${mood}`;
  const caption = $("#mascot-status");
  if (caption) caption.textContent = MOOD_CAPTIONS[mood];
  if (mascotRevertTimer) {
    clearTimeout(mascotRevertTimer);
    mascotRevertTimer = undefined;
  }
  if (revertMs) {
    mascotRevertTimer = setTimeout(() => setMascotMood(sessionUp ? "idle" : "sad"), revertMs);
  }
}

// ── Переключатели (segmented controls) ──────────────────────────────────────

function wireSegmented(containerSel: string, onPick: (value: string) => void) {
  const container = $(containerSel);
  container?.querySelectorAll<HTMLButtonElement>("button").forEach((btn) => {
    btn.addEventListener("click", () => {
      if (btn.disabled) return;
      container.querySelectorAll("button").forEach((b) => b.classList.remove("active"));
      btn.classList.add("active");
      onPick(btn.dataset.role ?? btn.dataset.profile ?? btn.dataset.mode ?? "");
    });
  });
}

// ── Выбор аудио-устройств ────────────────────────────────────────────────────

const INPUT_DEVICE_KEY = "sonic-input-device";
const OUTPUT_DEVICE_KEY = "sonic-output-device";

/// Заполняет <select> списком устройств. Пустое значение = системное по умолчанию.
///
/// Живой выбор пользователя важнее запомненного; в localStorage при перерисовке НЕ
/// пишем — иначе временно выдернутое устройство забылось бы навсегда, а так оно
/// автоматически выберется снова, когда его воткнут обратно.
function fillDeviceSelect(
  select: HTMLSelectElement | null,
  devices: string[],
  systemDefault: string | null,
  storageKey: string
) {
  if (!select) return;
  const wanted = select.value || localStorage.getItem(storageKey) || "";
  const defaultLabel = systemDefault
    ? `Системный по умолчанию — ${systemDefault}`
    : "Системный по умолчанию";

  select.innerHTML =
    `<option value="">${defaultLabel}</option>` +
    devices.map((d) => `<option value="${d}">${d}</option>`).join("");

  select.value = devices.includes(wanted) ? wanted : "";
}

function renderDevices(devices: AudioDevices) {
  fillDeviceSelect(
    $<HTMLSelectElement>("#input-device"),
    devices.inputs,
    devices.default_input,
    INPUT_DEVICE_KEY
  );
  fillDeviceSelect(
    $<HTMLSelectElement>("#output-device"),
    devices.outputs,
    devices.default_output,
    OUTPUT_DEVICE_KEY
  );
  updateToolsDevicesHint();
}

async function loadDevices() {
  const errorEl = $("#setup-error");
  try {
    renderDevices(await invoke<AudioDevices>("list_audio_devices"));
  } catch (err) {
    if (errorEl) errorEl.textContent = `Не удалось получить список устройств: ${err}`;
  }
}

/// Текущий выбор устройств — им пользуются и сессия, и инструменты канала.
function currentDevices(): { inputDevice: string; outputDevice: string } {
  return {
    inputDevice: $<HTMLSelectElement>("#input-device")?.value ?? "",
    outputDevice: $<HTMLSelectElement>("#output-device")?.value ?? "",
  };
}

/// Показывает в блоке инструментов, какие устройства реально будут использованы —
/// иначе непонятно, почему зонд «не звучит» (например, вывод ушёл на гарнитуру).
function updateToolsDevicesHint() {
  const hint = $("#tools-devices");
  if (!hint) return;
  const label = (sel: string) => {
    const el = $<HTMLSelectElement>(sel);
    return el?.selectedOptions[0]?.text ?? "—";
  };
  hint.textContent = `Используются: 🎤 ${label("#input-device")} → 🔊 ${label("#output-device")}`;
}

// ── Сессия ──────────────────────────────────────────────────────────────────

async function startSession() {
  const errorEl = $("#setup-error");
  const startBtn = $<HTMLButtonElement>("#start-btn");
  if (errorEl) errorEl.textContent = "";
  if (startBtn) startBtn.disabled = true;
  setMascotMood("jump");
  try {
    await invoke("start_session", {
      profile: selectedProfile,
      role: selectedRole,
      ...currentDevices(),
    });
    // Дальше UI переключит session-state-changed → "up".
  } catch (err) {
    if (errorEl) errorEl.textContent = `Ошибка запуска: ${err}`;
    if (startBtn) startBtn.disabled = false;
    setMascotMood("angry", 2200);
  }
}

async function stopSession() {
  try {
    await invoke("stop_session");
  } catch (err) {
    console.error("stop_session", err);
  }
  setSessionUp(false);
}

function setSessionUp(up: boolean) {
  sessionUp = up;
  const badge = $("#session-badge");
  const setup = $("#setup");
  const chat = $("#chat");
  const startBtn = $<HTMLButtonElement>("#start-btn");
  if (badge) {
    badge.textContent = up ? "в эфире" : "офлайн";
    badge.classList.toggle("off", !up);
    badge.classList.toggle("on", up);
  }
  setup?.classList.toggle("hidden", up);
  chat?.classList.toggle("hidden", !up);
  $("#main-empty")?.classList.toggle("hidden", up);
  $("#main-chat")?.classList.toggle("hidden", !up);
  if (startBtn) startBtn.disabled = false;
  setMascotMood(up ? "idle" : "sad");
  if (up) $<HTMLInputElement>("#msg-input")?.focus();
}

// ── Сообщения ────────────────────────────────────────────────────────────────

function hideEmptyHint() {
  $("#empty-hint")?.remove();
}

function appendBubble(text: string, kind: "in" | "out"): HTMLElement {
  hideEmptyHint();
  const list = $("#messages");
  const bubble = document.createElement("div");
  bubble.className = `bubble ${kind}`;
  const body = document.createElement("div");
  body.className = "bubble-text";
  body.textContent = text;
  bubble.appendChild(body);
  if (kind === "out") {
    const status = document.createElement("span");
    status.className = "bubble-status mono";
    status.textContent = "отправляется…";
    bubble.appendChild(status);
  }
  list?.appendChild(bubble);
  if (list) list.scrollTop = list.scrollHeight;
  return bubble;
}

async function sendMessage(e: Event) {
  e.preventDefault();
  const input = $<HTMLInputElement>("#msg-input");
  if (!input) return;
  const text = input.value.trim();
  if (!text) return;
  input.value = "";
  try {
    // Пузырь появится по событию message-status "sent" — там придёт msg_id для
    // последующего обновления статуса на "доставлено".
    await invoke("send_message", { text });
  } catch (err) {
    appendBubble(`⚠ не отправлено: ${err}`, "out");
    setMascotMood("angry", 2200);
  }
}

// ── Режим модуляции ───────────────────────────────────────────────────────────

async function setMode(mode: ModePolicy) {
  try {
    await invoke("set_mode", { mode });
  } catch (err) {
    console.error("set_mode", err);
  }
}

function modeBadgeClass(mode: string): string {
  if (mode.startsWith("OFDM-16")) return "mode-16qam";
  if (mode.startsWith("OFDM")) return "mode-qpsk";
  if (mode === "MFSK") return "mode-mfsk";
  if (mode === "CSS") return "mode-css";
  return "";
}

function renderTelemetry(q: LinkQuality) {
  const modeEl = $("#tele-mode");
  if (modeEl) {
    modeEl.textContent = q.mode;
    modeEl.className = `tele-value mode-badge ${modeBadgeClass(q.mode)}`;
  }
  const set = (sel: string, val: string) => {
    const el = $(sel);
    if (el) el.textContent = val;
  };
  set("#tele-snr", `${q.snr_db.toFixed(1)} дБ`);
  set("#tele-rtt", q.rtt_ms > 0 ? `${q.rtt_ms.toFixed(0)} мс` : "— мс");
  set("#tele-retx", String(q.retransmits));
  set("#tele-per", `${(q.per * 100).toFixed(0)}%`);
  set("#tele-inflight", String(q.in_flight));
}

// ── Инициализация ─────────────────────────────────────────────────────────────

function wireMessenger() {
  wireSegmented("#role-seg", (v) => (selectedRole = v as Role));
  wireSegmented("#profile-seg", (v) => (selectedProfile = v as Profile));
  wireSegmented("#mode-seg", (v) => setMode(v as ModePolicy));

  $("#start-btn")?.addEventListener("click", startSession);
  $("#stop-btn")?.addEventListener("click", stopSession);
  $("#composer")?.addEventListener("submit", sendMessage);
  $("#refresh-devices")?.addEventListener("click", loadDevices);

  // Запоминаем выбор устройств между запусками и держим подсказку в актуальном виде.
  $<HTMLSelectElement>("#input-device")?.addEventListener("change", (e) => {
    localStorage.setItem(INPUT_DEVICE_KEY, (e.target as HTMLSelectElement).value);
    updateToolsDevicesHint();
  });
  $<HTMLSelectElement>("#output-device")?.addEventListener("change", (e) => {
    localStorage.setItem(OUTPUT_DEVICE_KEY, (e.target as HTMLSelectElement).value);
    updateToolsDevicesHint();
  });
  // Hot-plug: бэкенд следит за списком и присылает событие только при реальном изменении.
  listen<AudioDevices>("audio-devices-changed", (e) => renderDevices(e.payload));
  loadDevices();

  listen<MessageReceived>("message-received", (e) => {
    appendBubble(e.payload.text, "in");
  });

  listen<MessageStatus>("message-status", (e) => {
    const { msg_id, status, text } = e.payload;
    if (status === "sent") {
      const bubble = appendBubble(text, "out");
      outgoing.set(msg_id, bubble);
    } else if (status === "delivered") {
      const bubble = outgoing.get(msg_id);
      const st = bubble?.querySelector<HTMLElement>(".bubble-status");
      if (st) {
        st.textContent = "✓✓ доставлено";
        st.classList.add("delivered");
      }
      setMascotMood("happy", 1800);
    }
  });

  listen<LinkQuality>("link-quality", (e) => renderTelemetry(e.payload));

  listen<SessionStateChanged>("session-state-changed", (e) => {
    if (e.payload.state === "up") setSessionUp(true);
    else {
      // Не роняем UI при временном down (LINK_DOWN), только помечаем бейдж.
      const badge = $("#session-badge");
      if (badge && sessionUp) {
        badge.textContent = "нет связи";
        badge.classList.add("off");
      }
      if (sessionUp) setMascotMood("sad");
    }
  });
}

// ─────────────────────────────────────────────────────────────────────────────
// Инструменты канала (самопроверка + акустическое обнаружение) — прежний функционал.
// ─────────────────────────────────────────────────────────────────────────────

interface TonePoint {
  freq_hz: number;
  snr_db: number;
}
interface ChannelReport {
  noise_floor_db: number;
  signal_db: number;
  snr_db: number;
  quality_label: string;
  recommended_mode: string;
  mode_label: string;
  estimated_bitrate_bps: number;
  per_tone: TonePoint[];
}
interface DiscoveredDevice {
  nickname: string;
  snr_db: number;
  quality_label: string;
  recommended_mode: string;
  mode_label: string;
  estimated_bitrate_bps: number;
  round: number;
}

function modeClass(mode: string): string {
  switch (mode) {
    case "CSS":
      return "mode-css";
    case "MFSK":
      return "mode-mfsk";
    case "OFDM_QPSK":
      return "mode-qpsk";
    case "OFDM_16QAM":
      return "mode-16qam";
    case "OFDM_64QAM":
      return "mode-64qam";
    default:
      return "";
  }
}
/// Короткое имя режима для компактного бейджа в списке чатов.
/// (mode_label с бэкенда — длинный, вида «CSS (Chirp Spread Spectrum) — …», в чип не влезает.)
function shortMode(mode: string): string {
  switch (mode) {
    case "CSS":
      return "CSS";
    case "MFSK":
      return "MFSK";
    case "OFDM_QPSK":
      return "QPSK";
    case "OFDM_16QAM":
      return "16-QAM";
    case "OFDM_64QAM":
      return "64-QAM";
    default:
      return mode;
  }
}
function formatBitrate(bps: number): string {
  return bps >= 1000 ? `~${(bps / 1000).toFixed(1)} кбит/с` : `~${bps} бит/с`;
}
function toneBarsHtml(perTone: TonePoint[]): string {
  return perTone
    .map((t) => {
      const heightPct = Math.max(4, Math.min(100, t.snr_db * 2.5));
      return `
        <div class="tone-bar" title="${t.freq_hz.toFixed(0)} Гц: ${t.snr_db.toFixed(1)} дБ SNR">
          <div class="tone-bar-track"><div class="tone-bar-fill" style="height:${heightPct}%"></div></div>
          <span class="tone-freq">${(t.freq_hz / 1000).toFixed(1)}к</span>
        </div>`;
    })
    .join("");
}
function channelReportHtml(report: ChannelReport): string {
  return `
    <div class="channel-metrics">
      <div class="metric"><span class="metric-label">Уровень шума</span><span class="metric-value">${report.noise_floor_db.toFixed(1)} дБФС</span></div>
      <div class="metric"><span class="metric-label">SNR сигнала</span><span class="metric-value">${report.snr_db.toFixed(1)} дБ</span></div>
      <div class="metric"><span class="metric-label">Чистота канала</span><span class="metric-value">${report.quality_label}</span></div>
    </div>
    <div class="mode-badge big ${modeClass(report.recommended_mode)}">
      <div class="mode-name">${report.mode_label}</div>
      <div class="mode-rate">${formatBitrate(report.estimated_bitrate_bps)}</div>
    </div>
    <div class="tone-bars">${toneBarsHtml(report.per_tone)}</div>`;
}
async function checkChannel() {
  const btn = $<HTMLButtonElement>("#check-channel-btn");
  const statusEl = $("#channel-status");
  const resultEl = $("#channel-result");
  if (!btn || !statusEl || !resultEl) return;
  btn.disabled = true;
  resultEl.innerHTML = "";
  statusEl.textContent = "Проверка: тишина, затем тестовый сигнал (~2 c)…";
  try {
    const report = await invoke<ChannelReport>("check_channel", currentDevices());
    statusEl.textContent = "Готово";
    resultEl.innerHTML = channelReportHtml(report);
  } catch (err) {
    statusEl.textContent = `Ошибка: ${err}`;
  } finally {
    btn.disabled = false;
  }
}

const NICKNAME_STORAGE_KEY = "sonic-nickname";
const discoveredDevices = new Map<string, DiscoveredDevice>();
function loadOrCreateNickname(): string {
  const stored = localStorage.getItem(NICKNAME_STORAGE_KEY);
  if (stored) return stored;
  const generated = `DEV${Math.floor(Math.random() * 900 + 100)}`;
  localStorage.setItem(NICKNAME_STORAGE_KEY, generated);
  return generated;
}
function qualityClass(label: string): string {
  if (label === "Отличная" || label === "Хорошая") return "quality-good";
  if (label === "Средняя") return "quality-mid";
  return "quality-bad";
}
function renderDiscoveryList() {
  const listEl = $("#discovery-list");
  const emptyEl = $("#discovery-empty");
  if (!listEl) return;
  const devices = [...discoveredDevices.values()].sort((a, b) => b.snr_db - a.snr_db);
  if (emptyEl) emptyEl.classList.toggle("hidden", devices.length > 0);
  listEl.querySelectorAll(".discovery-item").forEach((el) => el.remove());
  for (const d of devices) {
    const li = document.createElement("li");
    li.className = "discovery-item";
    li.title = `${d.mode_label} · ${d.quality_label} · ${d.snr_db.toFixed(1)} дБ`;
    li.innerHTML = `
      <span class="discovery-avatar">${d.nickname.slice(0, 1).toUpperCase()}</span>
      <span class="discovery-info">
        <span class="discovery-nickname">${d.nickname}</span>
        <span class="discovery-snr ${qualityClass(d.quality_label)}">${d.quality_label.toLowerCase()} · ${d.snr_db.toFixed(1)} дБ</span>
      </span>
      <span class="mode-badge inline ${modeClass(d.recommended_mode)}">${shortMode(d.recommended_mode)}</span>`;
    listEl.appendChild(li);
  }
}
function setDiscoveryBusy(busy: boolean) {
  const btn = $<HTMLButtonElement>("#discover-btn");
  const input = $<HTMLInputElement>("#nickname-input");
  if (btn) btn.disabled = busy;
  if (input) input.disabled = busy;
}
async function discoverDevices() {
  const statusEl = $("#discovery-status");
  const nicknameInput = $<HTMLInputElement>("#nickname-input");
  if (!statusEl || !nicknameInput) return;
  const nickname = nicknameInput.value.trim() || loadOrCreateNickname();
  localStorage.setItem(NICKNAME_STORAGE_KEY, nickname);
  nicknameInput.value = nickname;
  discoveredDevices.clear();
  renderDiscoveryList();
  setDiscoveryBusy(true);
  statusEl.textContent = "Слушаем и проигрываем маячок (~20 c)…";
  try {
    await invoke("discover_devices", { nickname, ...currentDevices() });
  } catch (err) {
    statusEl.textContent = `Ошибка: ${err}`;
    setDiscoveryBusy(false);
  }
}

interface SelfTestReport {
  mode: string;
  detected: boolean;
  matched: boolean;
  snr_db: number;
  captured_peak: number;
  captured_rms: number;
  verdict: string;
}

const SELFTEST_BUTTONS = [
  "#selftest-css",
  "#selftest-mfsk",
  "#selftest-ofdm-qpsk",
  "#selftest-ofdm-qam",
];

function setSelfTestBusy(busy: boolean) {
  for (const sel of SELFTEST_BUTTONS) {
    const btn = $<HTMLButtonElement>(sel);
    if (btn) btn.disabled = busy;
  }
}

async function runSelfTest(mode: SelfTestMode) {
  const statusEl = $("#selftest-status");
  const resultEl = $("#selftest-result");
  if (!statusEl || !resultEl) return;
  setSelfTestBusy(true);
  resultEl.innerHTML = "";
  statusEl.textContent = "Играем кадр и слушаем микрофон (~1–2 с)…";
  try {
    const r = await invoke<SelfTestReport>("modem_self_test", { mode, ...currentDevices() });
    statusEl.textContent = "Готово";
    const icon = r.matched ? "✅" : r.detected ? "⚠️" : "❌";
    resultEl.innerHTML = `
      <div class="mode-badge big ${r.matched ? "mode-64qam" : r.detected ? "mode-16qam" : "mode-css"}">
        <div class="mode-name">${icon} ${r.mode}: ${r.matched ? "работает" : r.detected ? "ловится с ошибками" : "не декодировано"}</div>
        <div class="mode-rate">${r.verdict}</div>
      </div>
      <div class="channel-metrics">
        <div class="metric"><span class="metric-label">Уровень захвата (пик)</span><span class="metric-value">${r.captured_peak.toFixed(3)}</span></div>
        <div class="metric"><span class="metric-label">RMS</span><span class="metric-value">${r.captured_rms.toFixed(4)}</span></div>
        <div class="metric"><span class="metric-label">SNR</span><span class="metric-value">${r.detected ? r.snr_db.toFixed(1) + " дБ" : "—"}</span></div>
      </div>`;
  } catch (err) {
    statusEl.textContent = `Ошибка: ${err}`;
  } finally {
    setSelfTestBusy(false);
  }
}

function wireTools() {
  $("#check-channel-btn")?.addEventListener("click", checkChannel);
  $("#selftest-css")?.addEventListener("click", () => runSelfTest("css"));
  $("#selftest-mfsk")?.addEventListener("click", () => runSelfTest("mfsk"));
  $("#selftest-ofdm-qpsk")?.addEventListener("click", () => runSelfTest("ofdm-qpsk"));
  $("#selftest-ofdm-qam")?.addEventListener("click", () => runSelfTest("ofdm-qam"));
  const nicknameInput = $<HTMLInputElement>("#nickname-input");
  if (nicknameInput) nicknameInput.value = loadOrCreateNickname();
  $("#discover-btn")?.addEventListener("click", discoverDevices);

  listen<DiscoveredDevice>("device-discovered", (event) => {
    const device = event.payload;
    const existing = discoveredDevices.get(device.nickname);
    if (!existing || device.snr_db > existing.snr_db) {
      discoveredDevices.set(device.nickname, device);
      renderDiscoveryList();
    }
  });
  listen<string>("discovery-error", (event) => {
    const statusEl = $("#discovery-status");
    if (statusEl) statusEl.textContent = `Ошибка: ${event.payload}`;
  });
  listen("discovery-finished", () => {
    const statusEl = $("#discovery-status");
    if (statusEl) {
      statusEl.textContent =
        discoveredDevices.size > 0
          ? `Найдено устройств: ${discoveredDevices.size}`
          : "Никого не услышали";
    }
    setDiscoveryBusy(false);
  });
}

window.addEventListener("DOMContentLoaded", () => {
  wireMessenger();
  wireTools();
  // Список "устройств поблизости" в сайдбаре пуст, пока не запущено обнаружение —
  // включаем его сразу при старте, чтобы список начал жить сам по себе.
  discoverDevices();
});
