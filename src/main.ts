import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

let greetInputEl: HTMLInputElement | null;
let greetMsgEl: HTMLElement | null;

async function greet() {
  if (greetMsgEl && greetInputEl) {
    // Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
    greetMsgEl.textContent = await invoke("greet", {
      name: greetInputEl.value,
    });
  }
}

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

function formatBitrate(bps: number): string {
  return bps >= 1000 ? `~${(bps / 1000).toFixed(1)} кбит/с` : `~${bps} бит/с`;
}

function toneBarsHtml(perTone: TonePoint[]): string {
  return perTone
    .map((t) => {
      const heightPct = Math.max(4, Math.min(100, t.snr_db * 2.5));
      return `
        <div class="tone-bar" title="${t.freq_hz.toFixed(0)} Гц: ${t.snr_db.toFixed(1)} дБ SNR">
          <div class="tone-bar-track">
            <div class="tone-bar-fill" style="height:${heightPct}%"></div>
          </div>
          <span class="tone-freq">${(t.freq_hz / 1000).toFixed(1)}к</span>
        </div>
      `;
    })
    .join("");
}

function channelReportHtml(report: ChannelReport): string {
  return `
    <div class="channel-metrics">
      <div class="metric">
        <span class="metric-label">Уровень шума</span>
        <span class="metric-value">${report.noise_floor_db.toFixed(1)} дБФС</span>
      </div>
      <div class="metric">
        <span class="metric-label">SNR сигнала</span>
        <span class="metric-value">${report.snr_db.toFixed(1)} дБ</span>
      </div>
      <div class="metric">
        <span class="metric-label">Чистота канала</span>
        <span class="metric-value">${report.quality_label}</span>
      </div>
    </div>
    <div class="mode-badge ${modeClass(report.recommended_mode)}">
      <div class="mode-name">${report.mode_label}</div>
      <div class="mode-rate">${formatBitrate(report.estimated_bitrate_bps)}</div>
    </div>
    <div class="tone-bars">${toneBarsHtml(report.per_tone)}</div>
  `;
}

function renderChannelReport(report: ChannelReport) {
  const resultEl = document.querySelector<HTMLElement>("#channel-result");
  if (!resultEl) return;
  resultEl.innerHTML = channelReportHtml(report);
}

async function checkChannel() {
  const btn = document.querySelector<HTMLButtonElement>("#check-channel-btn");
  const statusEl = document.querySelector<HTMLElement>("#channel-status");
  const resultEl = document.querySelector<HTMLElement>("#channel-result");
  if (!btn || !statusEl || !resultEl) return;

  btn.disabled = true;
  resultEl.innerHTML = "";
  statusEl.textContent =
    "Проверка канала: тишина, затем тестовый сигнал через колонки (~2 c)…";

  try {
    const report = await invoke<ChannelReport>("check_channel");
    statusEl.textContent = "Готово";
    renderChannelReport(report);
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

function renderDiscoveryList() {
  const listEl = document.querySelector<HTMLElement>("#discovery-list");
  if (!listEl) return;
  const devices = [...discoveredDevices.values()].sort((a, b) => b.snr_db - a.snr_db);
  if (devices.length === 0) {
    listEl.innerHTML = "";
    return;
  }
  listEl.innerHTML = devices
    .map(
      (d) => `
        <li class="discovery-item">
          <span class="discovery-nickname">${d.nickname}</span>
          <span class="discovery-snr">${d.snr_db.toFixed(1)} дБ SNR — ${d.quality_label}</span>
          <span class="mode-badge inline ${modeClass(d.recommended_mode)}">
            <span class="mode-name">${d.mode_label}</span>
            <span class="mode-rate">${formatBitrate(d.estimated_bitrate_bps)}</span>
          </span>
        </li>
      `
    )
    .join("");
}

function setDiscoveryBusy(busy: boolean) {
  const btn = document.querySelector<HTMLButtonElement>("#discover-btn");
  const input = document.querySelector<HTMLInputElement>("#nickname-input");
  if (btn) btn.disabled = busy;
  if (input) input.disabled = busy;
}

async function discoverDevices() {
  const statusEl = document.querySelector<HTMLElement>("#discovery-status");
  const nicknameInput = document.querySelector<HTMLInputElement>("#nickname-input");
  if (!statusEl || !nicknameInput) return;

  const nickname = nicknameInput.value.trim() || loadOrCreateNickname();
  localStorage.setItem(NICKNAME_STORAGE_KEY, nickname);
  nicknameInput.value = nickname;

  discoveredDevices.clear();
  renderDiscoveryList();
  setDiscoveryBusy(true);
  statusEl.textContent = "Ищем устройства рядом (слушаем и проигрываем маячок, ~20 c)…";

  try {
    await invoke("discover_devices", { nickname });
  } catch (err) {
    statusEl.textContent = `Ошибка: ${err}`;
    setDiscoveryBusy(false);
  }
}

window.addEventListener("DOMContentLoaded", () => {
  greetInputEl = document.querySelector("#greet-input");
  greetMsgEl = document.querySelector("#greet-msg");
  document.querySelector("#greet-form")?.addEventListener("submit", (e) => {
    e.preventDefault();
    greet();
  });

  document
    .querySelector("#check-channel-btn")
    ?.addEventListener("click", () => checkChannel());

  const nicknameInput = document.querySelector<HTMLInputElement>("#nickname-input");
  if (nicknameInput) nicknameInput.value = loadOrCreateNickname();

  document.querySelector("#discover-btn")?.addEventListener("click", () => discoverDevices());

  listen<DiscoveredDevice>("device-discovered", (event) => {
    const device = event.payload;
    const existing = discoveredDevices.get(device.nickname);
    if (!existing || device.snr_db > existing.snr_db) {
      discoveredDevices.set(device.nickname, device);
      renderDiscoveryList();
    }
  });

  listen<string>("discovery-error", (event) => {
    const statusEl = document.querySelector<HTMLElement>("#discovery-status");
    if (statusEl) statusEl.textContent = `Ошибка: ${event.payload}`;
  });

  listen("discovery-finished", () => {
    const statusEl = document.querySelector<HTMLElement>("#discovery-status");
    if (statusEl) {
      statusEl.textContent =
        discoveredDevices.size > 0
          ? `Поиск завершён, найдено устройств: ${discoveredDevices.size}`
          : "Поиск завершён, никого не услышали";
    }
    setDiscoveryBusy(false);
  });
});
