import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import type { Drive, FlashProgress } from "./types";

document.documentElement.setAttribute("data-theme", localStorage.getItem("theme") ?? "dark");

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

// ─── FDA ─────────────────────────────────────────────────────────────────
const fdaOverlay = $<HTMLDivElement>("fda-overlay");
const fdaHint    = $<HTMLSpanElement>("fda-hint");

async function checkFda() {
  const ok: boolean = await invoke("check_fda");
  fdaOverlay.classList.toggle("hidden", ok);
  return ok;
}
$<HTMLButtonElement>("fda-open").addEventListener("click", () => invoke("open_fda_settings"));
$<HTMLButtonElement>("fda-check").addEventListener("click", async () => {
  fdaHint.textContent = "";
  if (!await checkFda()) fdaHint.textContent = "Still not granted — toggle dd-Etcher on in the list.";
});
checkFda();

// ─── Theme ───────────────────────────────────────────────────────────────
$<HTMLButtonElement>("theme-toggle").addEventListener("click", () => {
  const next = document.documentElement.getAttribute("data-theme") === "light" ? "dark" : "light";
  document.documentElement.setAttribute("data-theme", next);
  localStorage.setItem("theme", next);
});

// ─── About ───────────────────────────────────────────────────────────────
const aboutOverlay = $<HTMLDivElement>("about-overlay");
const aboutVersion = $<HTMLElement>("about-version");

$<HTMLButtonElement>("about-btn").addEventListener("click", async () => {
  aboutVersion.textContent = await invoke<string>("app_version");
  aboutOverlay.classList.remove("hidden");
});
$<HTMLButtonElement>("about-close").addEventListener("click", () => aboutOverlay.classList.add("hidden"));
aboutOverlay.addEventListener("click", (e) => { if (e.target === aboutOverlay) aboutOverlay.classList.add("hidden"); });
$<HTMLButtonElement>("about-repo").addEventListener("click", () => invoke("open_repo"));

// ─── State ───────────────────────────────────────────────────────────────
let selectedImage:     string | null = null;
let selectedImageSize: number | null = null;
let selectedDrive:     Drive  | null = null;
let drives:            Drive[]       = [];

// ─── DOM ─────────────────────────────────────────────────────────────────
const imageIdle         = $<HTMLDivElement>("image-idle");
const imageActive       = $<HTMLDivElement>("image-active");
const imageExtEl        = $<HTMLSpanElement>("image-ext");
const imageExtLine      = $<HTMLSpanElement>("image-ext-line");
const imageBasenameEl   = $<HTMLDivElement>("image-basename");
const sizeNumEl         = $<HTMLSpanElement>("size-num");
const sizeUnitEl        = $<HTMLSpanElement>("size-unit");
const imageCompactText  = $<HTMLSpanElement>("image-compact-text");

const driveSelector     = $<HTMLDivElement>("drive-selector");
const driveTrigger      = $<HTMLDivElement>("drive-trigger");
const driveTriggerLabel = $<HTMLSpanElement>("drive-trigger-label");
const driveTriggerMeta  = $<HTMLSpanElement>("drive-trigger-meta");
const driveOptionsEl    = $<HTMLUListElement>("drive-options");
const refreshBtn        = $<HTMLButtonElement>("refresh-drives");
const driveCompactText  = $<HTMLSpanElement>("drive-compact-text");

const flashBtn          = $<HTMLButtonElement>("flash");
const cancelFlashBtn    = $<HTMLButtonElement>("cancel-flash");
const progressEl        = $<HTMLDivElement>("progress");
const progressFill      = $<HTMLDivElement>("progress-fill");
const progressPhaseEl   = $<HTMLDivElement>("progress-phase");
const progressBytesEl   = $<HTMLSpanElement>("progress-bytes");
const progressSpeedEl   = $<HTMLSpanElement>("progress-speed");
const statusEl          = $<HTMLDivElement>("status");

// ─── Helpers ─────────────────────────────────────────────────────────────
function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < units.length - 1);
  return `${n.toFixed(n < 10 ? 1 : 0)} ${units[i]}`;
}

// Typewriter for status messages
let typewriterTimer: ReturnType<typeof setTimeout> | null = null;
function setStatus(text: string, kind: "" | "ok" | "error" = "") {
  if (typewriterTimer) clearTimeout(typewriterTimer);
  statusEl.className = `status ${kind}`;
  if (kind === "ok") { statusEl.textContent = text; return; }
  statusEl.textContent = "";
  let i = 0;
  (function type() {
    if (i < text.length) {
      statusEl.textContent = text.slice(0, ++i);
      typewriterTimer = setTimeout(type, 14);
    }
  })();
}

// Step reveal — spring entrance
function revealStep(id: string, delay = 0) {
  const el = $(id);
  el.classList.remove("step--hidden");
  void el.offsetWidth;
  setTimeout(() => {
    el.classList.add("step--entering");
    el.addEventListener("animationend", () => el.classList.remove("step--entering"), { once: true });
  }, delay);
}

// Step hide — animate out then remove from layout
function hideStep(id: string) {
  const el = $(id);
  if (el.classList.contains("step--hidden")) return;
  el.classList.add("step--leaving");
  el.addEventListener("animationend", () => {
    el.classList.remove("step--leaving");
    el.classList.add("step--hidden");
  }, { once: true });
}

// Ping the step number (celebratory scale bounce)
function pingNum(stepId: string) {
  const numEl = $<HTMLElement>(stepId).querySelector(".step-num") as HTMLElement;
  numEl.classList.add("pinging");
  numEl.addEventListener("animationend", () => numEl.classList.remove("pinging"), { once: true });
}

// ─── Size display — the size IS the type ─────────────────────────────────
const GB = 1024 ** 3;
const MB = 1024 ** 2;

function getSizeProps(bytes: number): { val: number; unit: string; numPx: number } {
  if (bytes >= GB) {
    const v = bytes / GB;
    return { val: v, unit: "GB", numPx: bytes >= 32 * GB ? 68 : bytes >= 8 * GB ? 56 : 46 };
  }
  if (bytes >= MB) {
    const v = bytes / MB;
    return { val: v, unit: "MB", numPx: bytes >= 100 * MB ? 36 : 26 };
  }
  return { val: bytes / 1024, unit: "KB", numPx: 18 };
}

function formatSizeNum(val: number): string {
  return val < 10 ? val.toFixed(1) : Math.round(val).toString();
}

// Count up from 0 to target value using rAF — gives the satisfying "loading in" feel
let countRaf: number | null = null;
function animateSize(bytes: number) {
  if (countRaf) cancelAnimationFrame(countRaf);
  const { val: target, unit, numPx } = getSizeProps(bytes);
  const duration = 700;
  const start = performance.now();

  sizeUnitEl.textContent  = unit;
  sizeNumEl.style.fontSize  = `${numPx}px`;
  sizeUnitEl.style.fontSize = `${Math.round(numPx * 0.42)}px`;

  (function frame(now: number) {
    const t = Math.min((now - start) / duration, 1);
    const eased = 1 - Math.pow(1 - t, 3); // ease-out cubic
    const cur = target * eased;
    sizeNumEl.textContent = formatSizeNum(cur < 10 ? parseFloat(cur.toFixed(1)) : cur);
    if (t < 1) countRaf = requestAnimationFrame(frame);
    else sizeNumEl.textContent = formatSizeNum(target);
  })(start);
}

function updateCompactImage() {
  if (!selectedImage || selectedImageSize === null) return;
  const filename = selectedImage.split("/").pop() ?? selectedImage;
  const dotIdx   = filename.lastIndexOf(".");
  const ext      = dotIdx !== -1 ? filename.slice(dotIdx).toUpperCase() : "";
  const basename = dotIdx !== -1 ? filename.slice(0, dotIdx) : filename;
  const { val, unit } = getSizeProps(selectedImageSize);
  // e.g. "ubuntu-22.04  .ISO  · 6.8 GB"
  imageCompactText.innerHTML =
    `${basename}<span class="compact-accent">${ext}</span> <span class="compact-muted">· ${formatSizeNum(val)} ${unit}</span>`;
}

function updateFlashEnabled() {
  if (!selectedImage || !selectedDrive) { flashBtn.disabled = true; return; }
  if (selectedImageSize !== null && selectedImageSize > selectedDrive.size_bytes) {
    flashBtn.disabled = true;
    setStatus(`Image (${formatBytes(selectedImageSize)}) is larger than drive (${formatBytes(selectedDrive.size_bytes)}).`, "error");
    return;
  }
  flashBtn.disabled = false;
  if (statusEl.classList.contains("error")) setStatus("");
}

// ─── Image picker ─────────────────────────────────────────────────────────
async function openImagePicker() {
  const picked = await open({
    multiple: false,
    directory: false,
    filters: [
      { name: "Disk images", extensions: ["img", "iso", "dmg", "bin", "raw"] },
      { name: "All files", extensions: ["*"] },
    ],
  });
  if (typeof picked !== "string") return;

  selectedImage     = picked;
  selectedImageSize = null;

  const filename = picked.split("/").pop() ?? picked;
  const isFirstPick = imageActive.classList.contains("hidden");

  // Parse extension and base name
  const dotIdx  = filename.lastIndexOf(".");
  const ext     = dotIdx !== -1 ? filename.slice(dotIdx).toUpperCase() : "";
  const basename = dotIdx !== -1 ? filename.slice(0, dotIdx) : filename;

  // Act 1: extension punches up from below
  imageExtEl.textContent = ext || filename.toUpperCase();
  imageExtEl.classList.remove("punch");
  void imageExtEl.offsetWidth;
  imageExtEl.classList.add("punch");

  // Act 2: line draws right (CSS delay handles timing)
  imageExtLine.classList.remove("draw");
  void imageExtLine.offsetWidth;
  imageExtLine.classList.add("draw");

  // Act 3: base name sweeps in from left (CSS delay handles timing)
  imageBasenameEl.textContent = basename;
  imageBasenameEl.classList.remove("sweep", "cursor");
  void imageBasenameEl.offsetWidth;
  imageBasenameEl.classList.add("sweep");
  // Cursor blinks twice after sweep completes, then vanishes
  setTimeout(() => {
    imageBasenameEl.classList.add("cursor");
    setTimeout(() => imageBasenameEl.classList.remove("cursor"), 900);
  }, 540);

  // Placeholder size while loading
  sizeNumEl.textContent     = "—";
  sizeUnitEl.textContent    = "";
  sizeNumEl.style.fontSize  = "28px";
  sizeUnitEl.style.fontSize = "12px";

  if (isFirstPick) {
    imageIdle.classList.add("hidden");
    imageActive.classList.remove("hidden", "entering");
    void imageActive.offsetWidth;
    imageActive.classList.add("entering");
    $("step-image").classList.add("step--done");
    pingNum("step-image");
    revealStep("step-drive", 100);
    // Refresh drives when step 02 first appears
    refreshDrives(true);
  }

  try {
    selectedImageSize = await invoke<number>("get_file_size", { path: picked });
    animateSize(selectedImageSize);
    updateCompactImage();

    // If step 03 is already visible (re-pick while flash ready), re-validate
    if (!$("step-flash").classList.contains("step--hidden")) {
      updateFlashEnabled();
    }
  } catch (_) {}

  updateFlashEnabled();
}

$<HTMLButtonElement>("pick-image").addEventListener("click", openImagePicker);
$<HTMLButtonElement>("change-image").addEventListener("click", openImagePicker);
$<HTMLButtonElement>("change-image-compact").addEventListener("click", openImagePicker);

// ─── Drives ───────────────────────────────────────────────────────────────
async function refreshDrives(silent = false) {
  try {
    drives = await invoke<Drive[]>("list_drives");
    driveOptionsEl.innerHTML = "";

    if (drives.length === 0) {
      const li = document.createElement("li");
      li.className = "drive-option drive-option--empty";
      li.textContent = "NO REMOVABLE DRIVES FOUND";
      driveOptionsEl.appendChild(li);
    } else {
      drives.forEach((d, i) => {
        const li = document.createElement("li");
        li.className = "drive-option";
        li.style.setProperty("--i", String(i));
        li.setAttribute("role", "option");
        li.innerHTML = `
          <span class="drive-opt-name">${d.name}</span>
          <span class="drive-opt-size">${formatBytes(d.size_bytes)}</span>
          <span class="drive-opt-meta">${d.device_path}${d.removable ? " · removable" : ""}</span>
        `;
        li.addEventListener("click", () => selectDrive(d));
        driveOptionsEl.appendChild(li);
      });
    }

    if (!silent) {
      driveTrigger.classList.add("trigger--loaded");
      driveTrigger.addEventListener("animationend", () => driveTrigger.classList.remove("trigger--loaded"), { once: true });
    }

    if (selectedDrive && !drives.find((d) => d.id === selectedDrive!.id)) {
      selectDrive(null);
    }
  } catch (e) {
    setStatus(`Failed to list drives: ${e}`, "error");
  }
}

function selectDrive(d: Drive | null) {
  selectedDrive = d;
  driveSelector.classList.remove("drive-selector--open");

  if (d) {
    driveTriggerLabel.textContent = d.name;
    driveTriggerMeta.textContent  = `${d.device_path} · ${formatBytes(d.size_bytes)}`;
    driveSelector.classList.add("drive-selector--selected");

    $("step-drive").classList.add("step--done");
    pingNum("step-drive");

    driveCompactText.innerHTML =
      `${d.name} <span class="compact-muted">· ${d.device_path} · ${formatBytes(d.size_bytes)}</span>`;

    $("step-image").classList.add("step--compact");
    $("step-drive").classList.add("step--compact");
    revealStep("step-flash", 180);
  } else {
    driveTriggerLabel.textContent = "SELECT TARGET DRIVE";
    driveTriggerMeta.textContent  = "";
    driveSelector.classList.remove("drive-selector--selected");

    $("step-drive").classList.remove("step--done");
    $("step-image").classList.remove("step--compact");
    $("step-drive").classList.remove("step--compact");
    hideStep("step-flash");
  }

  updateFlashEnabled();
}

driveTrigger.addEventListener("click", () => {
  if (driveSelector.classList.contains("drive-selector--disabled")) return;
  driveSelector.classList.toggle("drive-selector--open");
});

document.addEventListener("click", (e) => {
  if (!driveSelector.contains(e.target as Node)) {
    driveSelector.classList.remove("drive-selector--open");
  }
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") driveSelector.classList.remove("drive-selector--open");
});

$<HTMLButtonElement>("change-drive-compact").addEventListener("click", () => selectDrive(null));

refreshBtn.addEventListener("click", (e) => {
  e.stopPropagation();
  refreshBtn.classList.add("spinning");
  refreshBtn.addEventListener("animationend", () => refreshBtn.classList.remove("spinning"), { once: true });
  refreshDrives();
});

// ─── Cancel flash ─────────────────────────────────────────────────────────
cancelFlashBtn.addEventListener("click", async () => {
  const ok = confirm("Cancel the flash?\n\nThe drive will be left in a corrupted state and must be reflashed before use.");
  if (ok) await invoke("cancel_flash");
});
listen("flash-stalled", () => {
  setStatus("Drive stalled for 2 minutes — flash cancelled. Check the drive and try again.", "error");
});

// ─── Flash ────────────────────────────────────────────────────────────────
flashBtn.addEventListener("click", async () => {
  if (!selectedImage || !selectedDrive) return;
  const ok = confirm(
    `This will ERASE all data on "${selectedDrive.name}" (${selectedDrive.device_path}).\n\nContinue?`
  );
  if (!ok) return;

  flashBtn.disabled   = true;
  driveSelector.classList.add("drive-selector--disabled");
  $<HTMLButtonElement>("change-image-compact").disabled  = true;
  $<HTMLButtonElement>("change-drive-compact").disabled  = true;
  cancelFlashBtn.classList.remove("hidden");
  progressEl.classList.remove("hidden");
  setStatus("requesting admin permission…");

  try {
    await invoke("flash", { imagePath: selectedImage, driveId: selectedDrive.id });
    setStatus("Flash complete and verified ✓", "ok");
  } catch (e) {
    const msg = String(e);
    if (msg === "cancelled") {
      setStatus("Flash cancelled — the drive is not usable. Flash again before use.", "error");
    } else {
      setStatus(`Flash failed: ${e}`, "error");
    }
  } finally {
    flashBtn.disabled   = false;
    driveSelector.classList.remove("drive-selector--disabled");
    $<HTMLButtonElement>("change-image-compact").disabled  = false;
    $<HTMLButtonElement>("change-drive-compact").disabled  = false;
    cancelFlashBtn.classList.add("hidden");
    updateFlashEnabled();
  }
});

// ─── Progress ─────────────────────────────────────────────────────────────
const phaseLabels: Record<string, string> = {
  flashing: "WRITING", hashing: "HASHING", verifying: "VERIFYING", done: "DONE",
};

listen<FlashProgress>("flash-progress", (event) => {
  const p = event.payload;
  const ratio = p.total_bytes > 0 ? Math.min(1, p.bytes_written / p.total_bytes) : 0;

  let pct = 0;
  if      (p.phase === "flashing")   pct = ratio * 40;
  else if (p.phase === "hashing")    pct = 40 + ratio * 30;
  else if (p.phase === "verifying")  pct = 70 + ratio * 30;
  else if (p.phase === "done")       pct = 100;

  progressFill.style.width = `${pct}%`;

  const label = phaseLabels[p.phase] ?? p.phase.toUpperCase();
  if (progressPhaseEl.textContent !== label) {
    progressPhaseEl.textContent = label;
    progressPhaseEl.className = `progress-phase-label${p.phase === "done" ? " phase-done" : ""}`;
  }
  if (p.phase === "done") progressFill.classList.add("phase-done");

  progressBytesEl.textContent = `${formatBytes(p.bytes_written)} / ${formatBytes(p.total_bytes)}`;
  progressSpeedEl.textContent = p.bytes_per_second > 0 ? `${formatBytes(p.bytes_per_second)}/s` : "";

  if (p.phase === "flashing" && p.bytes_written > 0) setStatus("writing to drive…");
  else if (p.phase === "hashing")   setStatus("hashing source image…");
  else if (p.phase === "verifying") setStatus("verifying write integrity…");
});

// Initial drive load (silent — no pulse on first load)
refreshDrives(true);
