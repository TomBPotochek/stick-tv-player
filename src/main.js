import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import mpegts from "mpegts.js";

const $ = (id) => document.getElementById(id);
const video = $("video");

const state = {
  channels: [],
  current: null, // channel being watched
  player: null,
  playToken: 0, // guards against out-of-order play() results when zapping quickly
  scanning: false,
  osdTimer: null,
  mode: "web", // "mpv": native mpv window over the video area; "web": <video> fed by the ffmpeg transcode
  info: { mpv: null, ffmpeg: null },
  playing: false, // the backend is streaming a channel
  mpvPlaying: false, // mpv shows video
  hwdec: null,
  muted: false,
  fullscreen: false,
  encoder: "",
};

// ------------------------------------------------------------------ helpers

function key(ch) {
  return `${ch.freq_khz}:${ch.service_id}`;
}

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

function store(k, v) {
  try {
    localStorage.setItem(k, v);
  } catch {}
}

function recall(k) {
  try {
    return localStorage.getItem(k);
  } catch {
    return null;
  }
}

function showMessage(title, sub = "") {
  $("message").classList.remove("hidden");
  $("message").innerHTML = `<div class="message-title">${esc(title)}</div><div class="message-sub">${esc(sub)}</div>`;
  updateView();
}

function hideMessage() {
  $("message").classList.add("hidden");
  updateView();
}

// ------------------------------------------------------------------ mpv video window

let lastView = "";

/** Keeps mpv's native window on top of the video area; hidden while a message (or no video) should show instead. */
function updateView(force = false) {
  if (state.mode !== "mpv" && !force) return;
  const visible =
    state.mode === "mpv" && state.mpvPlaying && $("message").classList.contains("hidden") && $("scan-dialog").classList.contains("hidden");
  const r = $("video-wrap").getBoundingClientRect();
  const d = window.devicePixelRatio || 1;
  const v = { x: Math.round(r.left * d), y: Math.round(r.top * d), w: Math.round(r.width * d), h: Math.round(r.height * d), visible };
  const k = JSON.stringify(v);
  if (k === lastView) return;
  lastView = k;
  invoke("mpv_view", v).catch(() => {});
}

new ResizeObserver(() => updateView()).observe($("video-wrap"));
window.addEventListener("resize", () => updateView());

function osdText(ch) {
  return `${ch.virtual_number || `UHF ${ch.physical}`}  ${ch.name}`;
}

function mpvLabel() {
  return state.hwdec ? `mpv · ${state.hwdec === "no" ? "CPU decode" : state.hwdec}` : "mpv";
}

function updateRateLabel(mbit, dropped) {
  const parts = [];
  if (mbit != null) parts.push(`${mbit.toFixed(1)} Mbit/s`);
  if (dropped) parts.push(`dropped ${dropped}`);
  if (state.encoder) parts.push(state.encoder);
  $("rate").textContent = parts.join(" · ") || "–";
}

listen("mpv-event", (ev) => {
  const e = ev.payload;
  if (e.event === "key") {
    handleKey(e.key);
  } else if (e.event === "hwdec") {
    state.hwdec = e.value;
    if (state.mode === "mpv") state.encoder = mpvLabel();
  } else if (state.mode !== "mpv" || !state.current) {
    // the rest only matters while mpv plays a channel
  } else if (e.event === "playing") {
    state.mpvPlaying = true;
    hideMessage();
    invoke("mpv_osd", { text: osdText(state.current) }).catch(() => {});
  } else if (e.event === "error") {
    state.mpvPlaying = false;
    showMessage("Playback error (mpv)", e.detail);
  } else if (e.event === "exit") {
    state.mpvPlaying = false;
    showMessage("mpv closed", "Pick the channel again to restart it.");
  }
});

/** Sorted for display: by virtual number when known, else physical channel then service id. */
function sortChannels(list) {
  const num = (c) => {
    const m = /^(\d+)(?:\.(\d+))?$/.exec(c.virtual_number || "");
    return m ? [Number(m[1]), Number(m[2] || 0)] : [1000 + c.physical, c.service_id];
  };
  return [...list].sort((a, b) => {
    const [a1, a2] = num(a);
    const [b1, b2] = num(b);
    return a1 - b1 || a2 - b2;
  });
}

function orderedChannels() {
  const full = state.channels.filter((c) => !c.one_seg);
  const one = state.channels.filter((c) => c.one_seg);
  return [...full, ...one];
}

// ------------------------------------------------------------------ device

async function refreshDevice() {
  const pill = $("device");
  try {
    const st = await invoke("device_status");
    if (st.error) {
      pill.textContent = `USB error: ${st.error}`;
      pill.className = "pill bad";
    } else if (st.devices.length === 0) {
      pill.textContent = "No stick connected (PT275 / PT275C)";
      pill.className = "pill bad";
    } else {
      const d = st.devices[0];
      const id = d.pid.toString(16).padStart(4, "0");
      pill.textContent = `${d.profile} · 048d:${id}`;
      pill.className = "pill ok";
    }
    return st;
  } catch (e) {
    pill.textContent = String(e);
    pill.className = "pill bad";
    return null;
  }
}

// ------------------------------------------------------------------ channel list

function renderList() {
  const box = $("channel-list");
  if (state.channels.length === 0) {
    box.innerHTML = `<div class="empty">No channels yet.<br />Plug in the stick and scan.<br /><button class="btn" id="empty-scan">Scan channels</button></div>`;
    $("empty-scan").onclick = startScan;
    $("list-footer").textContent = "";
    return;
  }
  const full = state.channels.filter((c) => !c.one_seg);
  const one = state.channels.filter((c) => c.one_seg);
  const item = (c) => {
    const active = state.current && key(state.current) === key(c) ? " active" : "";
    const num = c.virtual_number || `${c.physical}`;
    const sub = [c.network_name || c.provider, `UHF ${c.physical}`, c.video].filter(Boolean).join(" · ");
    return `<div class="channel${active}" data-key="${esc(key(c))}" title="service 0x${c.service_id.toString(16).toUpperCase()} · ${(c.freq_khz / 1000).toFixed(3)} MHz">
      <span class="num">${esc(num)}</span>
      <div class="meta"><div class="name">${esc(c.name)}</div><div class="sub">${esc(sub)}</div></div>
    </div>`;
  };
  let html = "";
  if (full.length) html += `<div class="group">Full-seg (HD/SD) · ${full.length}</div>` + full.map(item).join("");
  if (one.length) html += `<div class="group">One-seg (mobile) · ${one.length}</div>` + one.map(item).join("");
  box.innerHTML = html;
  for (const el of box.querySelectorAll(".channel")) {
    el.onclick = () => {
      const c = state.channels.find((x) => key(x) === el.dataset.key);
      if (c) play(c);
    };
  }
}

function setChannels(list, scannedAt) {
  state.channels = sortChannels(list);
  renderList();
  if (scannedAt) {
    $("list-footer").textContent = `${list.length} channels · scanned ${new Date(scannedAt * 1000).toLocaleString()}`;
  }
}

// ------------------------------------------------------------------ playback

function destroyPlayer() {
  state.mpvPlaying = false;
  const p = state.player;
  state.player = null;
  if (!p) return;
  try {
    p.pause();
    p.unload();
    p.detachMediaElement();
    p.destroy();
  } catch {}
}

function showOsd(ch) {
  $("osd-num").textContent = ch.virtual_number || `UHF ${ch.physical}`;
  $("osd-name").textContent = ch.name;
  $("osd-sub").textContent = [ch.network_name, `UHF ${ch.physical} · ${(ch.freq_khz / 1000).toFixed(3)} MHz`, ch.one_seg ? "one-seg" : "full-seg"]
    .filter(Boolean)
    .join(" · ");
  const osd = $("osd");
  osd.classList.remove("hidden", "fade");
  clearTimeout(state.osdTimer);
  state.osdTimer = setTimeout(() => osd.classList.add("fade"), 4000);
}

async function play(ch) {
  if (state.scanning) return;
  const token = ++state.playToken;
  state.current = ch;
  store("last", key(ch));
  renderList();
  showOsd(ch);
  destroyPlayer();
  resetMeters();
  showMessage(`Tuning ${ch.name}…`, `UHF ${ch.physical} · ${(ch.freq_khz / 1000).toFixed(3)} MHz`);
  let res;
  try {
    res = await invoke("play", { freqKhz: ch.freq_khz, serviceId: ch.service_id, mode: state.mode });
  } catch (e) {
    if (token !== state.playToken) return;
    state.playing = false;
    showMessage("Could not tune", String(e));
    refreshDevice();
    return;
  }
  if (token !== state.playToken) return;
  state.playing = true;
  if (res.encoder === "mpv") {
    state.encoder = mpvLabel();
  } else {
    const names = { h264_nvenc: "NVENC", h264_qsv: "Quick Sync", h264_amf: "AMF", libx264: "x264 (CPU)" };
    state.encoder = `ffmpeg · ${names[res.encoder] || res.encoder}`;
  }
  if (!res.locked) {
    showMessage("No signal", "The tuner did not lock on this channel. Check the antenna; it keeps trying.");
  } else {
    showMessage("Waiting for video…");
  }
  if (state.mode === "mpv") {
    invoke("mpv_mute", { muted: state.muted }).catch(() => {});
  } else {
    startPlayer(res.url, token);
  }
}

function startPlayer(url, token) {
  if (!mpegts.isSupported()) {
    showMessage("Playback not supported", "This webview has no Media Source Extensions.");
    return;
  }
  const p = mpegts.createPlayer(
    { type: "mpegts", isLive: true, url },
    {
      enableWorker: false,
      enableStashBuffer: false,
      lazyLoad: false,
      autoCleanupSourceBuffer: true,
      liveBufferLatencyChasing: true,
      liveBufferLatencyMaxLatency: 2.5,
      liveBufferLatencyMinRemain: 0.6,
    },
  );
  state.player = p;
  p.attachMediaElement(video);
  p.on(mpegts.Events.ERROR, (type, detail, info) => {
    if (token !== state.playToken) return;
    console.warn("player error", type, detail, info);
    // Network errors end the stream (e.g. the backend restarted it): reconnect.
    if (type === mpegts.ErrorTypes.NETWORK_ERROR && state.current) {
      setTimeout(() => token === state.playToken && restartPlayer(url, token), 800);
    } else {
      showMessage("Playback error", `${type}: ${detail}`);
    }
  });
  p.load();
  const tryPlay = video.play();
  if (tryPlay) {
    tryPlay.catch(() => {
      // autoplay with sound refused: play muted and offer to unmute
      setMuted(true);
      $("unmute").classList.remove("hidden");
      video.play().catch(() => {});
    });
  }
}

function restartPlayer(url, token) {
  destroyPlayer();
  startPlayer(url, token);
}

async function stopPlayback() {
  state.playToken++;
  state.playing = false;
  destroyPlayer();
  updateView();
  try {
    await invoke("stop");
  } catch {}
}

video.addEventListener("playing", () => hideMessage());
video.addEventListener("waiting", () => {
  if (state.current) showMessage("Buffering…");
});

function setMuted(m) {
  state.muted = m;
  video.muted = m;
  if (!m) $("unmute").classList.add("hidden");
  $("mute-btn").textContent = m ? "🔇" : "🔊";
  if (state.mode === "mpv") invoke("mpv_mute", { muted: m }).catch(() => {});
}

$("unmute").onclick = () => setMuted(false);
$("mute-btn").onclick = () => setMuted(!state.muted);

/** Fullscreen is the window's (not the page element's), so mpv's native window can fill the screen too. */
function setFullscreen(on) {
  state.fullscreen = on;
  document.body.classList.toggle("fs", on);
  invoke("set_fullscreen", { on }).catch(() => {});
}

function toggleFullscreen() {
  setFullscreen(!state.fullscreen);
}

$("fs-btn").onclick = toggleFullscreen;
$("video-wrap").ondblclick = toggleFullscreen;

function zap(delta) {
  const list = orderedChannels();
  if (!list.length) return;
  let i = state.current ? list.findIndex((c) => key(c) === key(state.current)) : -1;
  i = (i + delta + list.length) % list.length;
  play(list[i]);
}

/** Keys from the page and from mpv's window (forwarded as "up", "down", "f", "m", "i", "esc"). */
function handleKey(k) {
  if (state.scanning) return false;
  if (k === "up") zap(-1);
  else if (k === "down") zap(1);
  else if (k === "f") toggleFullscreen();
  else if (k === "esc") {
    if (state.fullscreen) setFullscreen(false);
  } else if (k === "m") setMuted(!state.muted);
  else if (k === "i") {
    if (!state.current) return true;
    showOsd(state.current);
    if (state.mode === "mpv" && state.mpvPlaying) invoke("mpv_osd", { text: osdText(state.current) }).catch(() => {});
  } else return false;
  return true;
}

const KEYS = { ArrowUp: "up", PageUp: "up", ArrowDown: "down", PageDown: "down", f: "f", F: "f", m: "m", M: "m", i: "i", I: "i", Escape: "esc" };

document.addEventListener("keydown", (e) => {
  if (e.target.tagName === "SELECT") return;
  if (KEYS[e.key] && handleKey(KEYS[e.key])) e.preventDefault();
});

// ------------------------------------------------------------------ signal meters

function meter(barId, valId, pct) {
  const bar = $(barId);
  if (pct == null) {
    bar.style.width = "0";
    $(valId).textContent = "–";
    return;
  }
  bar.style.width = `${pct}%`;
  bar.style.background = pct >= 60 ? "var(--good)" : pct >= 30 ? "var(--warn)" : "var(--bad)";
  $(valId).textContent = `${pct}%`;
}

function resetMeters() {
  meter("strength-bar", "strength-val", null);
  meter("quality-bar", "quality-val", null);
  $("lock").textContent = "–";
  $("lock").className = "stat";
  $("rate").textContent = "–";
}

listen("signal", (ev) => {
  const s = ev.payload;
  if (!state.current || s.service_id !== state.current.service_id) return;
  const sig = s.signal || {};
  meter("strength-bar", "strength-val", sig.strength_percent);
  meter("quality-bar", "quality-val", sig.quality_percent);
  const lock = $("lock");
  lock.textContent = sig.locked ? (sig.dbm != null ? `Locked · ${sig.dbm} dBm` : "Locked") : "No lock";
  lock.className = `stat ${sig.locked ? "ok" : "bad"}`;
  updateRateLabel(s.mbit, s.dropped);
  if (!s.streaming && sig.locked && !$("message").classList.contains("hidden")) {
    showMessage("Waiting for the service…", "Looking for this channel's program table in the stream.");
  }
});

listen("player-error", (ev) => {
  state.playing = false;
  destroyPlayer();
  if (state.mode === "mpv") invoke("stop").catch(() => {});
  showMessage("The stick stopped streaming", `${ev.payload}\nIs it still plugged in?`);
  refreshDevice();
});

// ------------------------------------------------------------------ scan

async function startScan() {
  if (state.scanning) return;
  await stopPlayback();
  state.current = null;
  renderList();
  resetMeters();
  showMessage("Scanning…");
  state.scanning = true;
  $("scan-btn").disabled = true;
  $("scan-bar").style.width = "0";
  $("scan-now").textContent = "Opening the stick…";
  $("scan-found").innerHTML = "";
  $("scan-cancel").textContent = "Cancel";
  $("scan-dialog").classList.remove("hidden");
  updateView();
  await invoke("start_scan", {});
}

const STAGES = { tuning: "tuning", reading: "locked, reading channel names", locked: "done", empty: "no signal" };

listen("scan-progress", (ev) => {
  const p = ev.payload;
  const done = p.index + (p.stage === "locked" || p.stage === "empty" ? 1 : 0);
  $("scan-bar").style.width = `${(100 * done) / p.total}%`;
  const sig = p.signal ? ` · signal ${p.signal.strength_percent ?? "?"}% / quality ${p.signal.quality_percent ?? "?"}%` : "";
  $("scan-now").textContent = `UHF ${p.physical} · ${(p.freq_khz / 1000).toFixed(3)} MHz — ${STAGES[p.stage] || p.stage}${sig} · ${p.services_found} services found`;
});

listen("scan-done", (ev) => {
  const d = ev.payload;
  state.scanning = false;
  $("scan-btn").disabled = false;
  $("scan-bar").style.width = "100%";
  if (d.error) {
    $("scan-now").textContent = `Scan failed: ${d.error}`;
  } else if (d.cancelled) {
    $("scan-now").textContent = `Cancelled. ${d.channels.length} services found (not saved).`;
  } else {
    $("scan-now").textContent = `Done: ${d.channels.length} services found.`;
  }
  const byMux = new Map();
  for (const c of d.channels) {
    if (!byMux.has(c.physical)) byMux.set(c.physical, []);
    byMux.get(c.physical).push(c);
  }
  $("scan-found").innerHTML = [...byMux.entries()]
    .map(([ph, cs]) => `<div><span class="ch">UHF ${ph}</span>${esc(cs[0].network_name || "")} — ${cs.map((c) => esc(c.name)).join(", ")}</div>`)
    .join("");
  $("scan-cancel").textContent = "Close";
  if (!d.error && !d.cancelled) {
    setChannels(d.channels, Math.floor(Date.now() / 1000));
  }
  showMessage(state.channels.length ? "Pick a channel" : "No channels found", state.channels.length ? "" : "Check the antenna and scan again.");
  refreshDevice();
});

$("scan-cancel").onclick = async () => {
  if (state.scanning) {
    $("scan-now").textContent = "Cancelling…";
    await invoke("stop");
  } else {
    $("scan-dialog").classList.add("hidden");
    updateView();
  }
};

$("scan-btn").onclick = startScan;

// ------------------------------------------------------------------ startup

// ------------------------------------------------------------------ title bar (frameless window)

const appWindow = getCurrentWindow();
const MAX_ICON = `<svg viewBox="0 0 10 10"><rect x="0.5" y="0.5" width="9" height="9" /></svg>`;
const RESTORE_ICON = `<svg viewBox="0 0 10 10"><rect x="0.5" y="2.5" width="7" height="7" /><path d="M2.5 2.5V0.5h7v7h-2" /></svg>`;

$("win-min").onclick = () => appWindow.minimize();
$("win-max").onclick = () => appWindow.toggleMaximize();
$("win-close").onclick = () => appWindow.close();

async function updateMaxButton() {
  try {
    const max = await appWindow.isMaximized();
    $("win-max").innerHTML = max ? RESTORE_ICON : MAX_ICON;
    $("win-max").title = max ? "Restore" : "Maximize";
  } catch {}
}

appWindow.onResized(() => updateMaxButton());
updateMaxButton();

// ------------------------------------------------------------------ player selection

function setMode(mode) {
  state.mode = mode;
  $("mode").value = mode;
  video.classList.toggle("hidden", mode === "mpv");
  store("mode", mode);
}

$("mode").onchange = async () => {
  const mode = $("mode").value;
  const ch = state.current;
  await stopPlayback();
  setMode(mode);
  updateView(true); // hides the mpv window when leaving mpv
  if (ch) play(ch);
};

async function initModes() {
  try {
    state.info = await invoke("player_info");
  } catch {}
  const opt = (v) => $("mode").querySelector(`option[value="${v}"]`);
  opt("mpv").disabled = !state.info.mpv;
  opt("mpv").textContent = state.info.mpv ? "Player: mpv" : "Player: mpv (not found)";
  opt("web").disabled = !state.info.ffmpeg;
  opt("web").textContent = state.info.ffmpeg ? "Player: built-in (ffmpeg)" : "Player: built-in (ffmpeg not found)";
  const saved = recall("mode");
  const ok = (m) => (m === "mpv" ? state.info.mpv : m === "web" ? state.info.ffmpeg : null);
  setMode(ok(saved) ? saved : state.info.mpv ? "mpv" : "web");
}

async function init() {
  await initModes();
  const st = await refreshDevice();
  const file = await invoke("load_channels");
  setChannels(file.channels || [], file.scanned_at);
  if (state.channels.length) {
    showMessage("Pick a channel", "↑/↓ to zap · F fullscreen · M mute · I info");
    const last = recall("last");
    const ch = last && state.channels.find((c) => key(c) === last);
    if (ch && st && st.devices.length) play(ch);
  } else if (st && st.devices.length === 0) {
    showMessage(
      "Plug in the stick",
      "Supported: PT275 (048d:275a) and PT275C (048d:e275). On Windows, bind the WinUSB driver with Zadig first (see README).",
    );
  } else {
    showMessage("No channels yet", "Click “Scan channels” to search UHF 14–51.");
  }
  // keep the device pill current (plug / unplug); not while streaming (unplug then shows as a player error)
  setInterval(() => {
    if (!state.scanning && !state.playing) refreshDevice();
  }, 5000);
}

init();
