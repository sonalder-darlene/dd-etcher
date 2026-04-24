import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import type { Drive, FlashProgress } from "./types";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

// --- FDA onboarding ---
const fdaOverlay = $<HTMLDivElement>("fda-overlay");
const fdaOpenBtn = $<HTMLButtonElement>("fda-open");
const fdaCheckBtn = $<HTMLButtonElement>("fda-check");
const fdaHint = $<HTMLSpanElement>("fda-hint");

async function checkFda() {
  const granted: boolean = await invoke("check_fda");
  if (granted) {
    fdaOverlay.classList.add("hidden");
  } else {
    fdaOverlay.classList.remove("hidden");
  }
  return granted;
}

fdaOpenBtn.addEventListener("click", () => invoke("open_fda_settings"));

fdaCheckBtn.addEventListener("click", async () => {
  fdaHint.textContent = "";
  const granted = await checkFda();
  if (!granted) {
    fdaHint.textContent = "Still not granted — make sure to toggle dd-etcher on.";
  }
});

// Check FDA on load before anything else.
checkFda();
// ---

let selectedImage: string | null = null;
let selectedDrive: Drive | null = null;
let drives: Drive[] = [];

const pickImageBtn = $<HTMLButtonElement>("pick-image");
const imageInfo = $<HTMLDivElement>("image-info");
const driveSelect = $<HTMLSelectElement>("drive-select");
const refreshBtn = $<HTMLButtonElement>("refresh-drives");
const driveInfo = $<HTMLDivElement>("drive-info");
const flashBtn = $<HTMLButtonElement>("flash");
const progressEl = $<HTMLDivElement>("progress");
const progressFill = progressEl.querySelector(".progress-fill") as HTMLDivElement;
const progressText = progressEl.querySelector(".progress-text") as HTMLDivElement;
const statusEl = $<HTMLDivElement>("status");

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < units.length - 1);
  return `${n.toFixed(n < 10 ? 1 : 0)} ${units[i]}`;
}

function updateFlashEnabled() {
  flashBtn.disabled = !(selectedImage && selectedDrive);
}

function setStatus(text: string, kind: "" | "ok" | "error" = "") {
  statusEl.textContent = text;
  statusEl.className = `status ${kind}`;
}

async function refreshDrives() {
  try {
    drives = await invoke<Drive[]>("list_drives");
    driveSelect.innerHTML = '<option value="">Select a drive…</option>';
    for (const d of drives) {
      const opt = document.createElement("option");
      opt.value = d.id;
      opt.textContent = `${d.name} — ${formatBytes(d.size_bytes)}`;
      driveSelect.appendChild(opt);
    }
    if (selectedDrive && !drives.find((d) => d.id === selectedDrive!.id)) {
      selectedDrive = null;
      driveInfo.classList.add("hidden");
      updateFlashEnabled();
    }
  } catch (e) {
    setStatus(`Failed to list drives: ${e}`, "error");
  }
}

pickImageBtn.addEventListener("click", async () => {
  const picked = await open({
    multiple: false,
    directory: false,
    filters: [
      { name: "Disk images", extensions: ["img", "iso", "dmg", "bin", "raw"] },
      { name: "All files", extensions: ["*"] },
    ],
  });
  if (typeof picked === "string") {
    selectedImage = picked;
    const name = picked.split("/").pop() ?? picked;
    imageInfo.textContent = name;
    imageInfo.classList.remove("hidden");
    updateFlashEnabled();
  }
});

driveSelect.addEventListener("change", () => {
  const d = drives.find((d) => d.id === driveSelect.value) ?? null;
  selectedDrive = d;
  if (d) {
    driveInfo.textContent = `${d.device_path} • ${formatBytes(d.size_bytes)}`;
    driveInfo.classList.remove("hidden");
  } else {
    driveInfo.classList.add("hidden");
  }
  updateFlashEnabled();
});

refreshBtn.addEventListener("click", refreshDrives);

flashBtn.addEventListener("click", async () => {
  if (!selectedImage || !selectedDrive) return;
  const confirmed = confirm(
    `This will ERASE all data on "${selectedDrive.name}" (${selectedDrive.device_path}).\n\nContinue?`,
  );
  if (!confirmed) return;

  flashBtn.disabled = true;
  pickImageBtn.disabled = true;
  refreshBtn.disabled = true;
  driveSelect.disabled = true;
  progressEl.classList.remove("hidden");
  setStatus("Requesting admin permission…");

  try {
    await invoke("flash", {
      imagePath: selectedImage,
      driveId: selectedDrive.id,
    });
    setStatus("Flash complete and verified ✓", "ok");
  } catch (e) {
    setStatus(`Flash failed: ${e}`, "error");
  } finally {
    flashBtn.disabled = false;
    pickImageBtn.disabled = false;
    refreshBtn.disabled = false;
    driveSelect.disabled = false;
    updateFlashEnabled();
  }
});

listen<FlashProgress>("flash-progress", (event) => {
  const p = event.payload;
  const pct = p.total_bytes > 0 ? Math.min(100, (p.bytes_written / p.total_bytes) * 100) : 0;
  progressFill.style.width = `${pct}%`;
  const rate = p.bytes_per_second > 0 ? ` • ${formatBytes(p.bytes_per_second)}/s` : "";
  progressText.textContent = `${p.phase} — ${formatBytes(p.bytes_written)} / ${formatBytes(p.total_bytes)}${rate}`;
  if (p.phase === "flashing" && p.bytes_written > 0) setStatus("Flashing…");
  else if (p.phase === "verifying") setStatus("Verifying…");
});

refreshDrives();
