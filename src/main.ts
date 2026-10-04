import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
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
const flashAgainBtn     = $<HTMLButtonElement>("flash-again");
const progressEl        = $<HTMLDivElement>("progress");
const progressFill      = $<HTMLDivElement>("progress-fill");
const progressPhaseEl   = $<HTMLDivElement>("progress-phase");
const progressBytesEl   = $<HTMLSpanElement>("progress-bytes");
const progressSpeedEl   = $<HTMLSpanElement>("progress-speed");
const statusEl          = $<HTMLDivElement>("status");

// ─── Helpers ─────────────────────────────────────────────────────────────
function span(cls: string, text: string): HTMLSpanElement {
  const el = document.createElement("span");
  el.className = cls;
  el.textContent = text;
  return el;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < units.length - 1);
  return `${n.toFixed(n < 10 ? 1 : 0)} ${units[i]}`;
}

// Restart a one-shot CSS animation on an element.
function replay(el: HTMLElement, cls: string) {
  el.classList.remove(cls);
  void el.offsetWidth;
  el.classList.add(cls);
}

// Status line. Progress events arrive several times a second, so the text only
// animates when it actually changes — re-running an entrance on every event is
// what made it flicker. `busy` adds three dots that pulse on their own.
function setStatus(text: string, kind: "" | "ok" | "error" = "", busy = false) {
  const key = `${kind}|${busy}|${text}`;
  if (statusEl.dataset.key === key) return;
  statusEl.dataset.key = key;
  statusEl.className = `status ${kind}`;
  statusEl.replaceChildren(text);
  if (busy) {
    const dots = span("dots", "");
    dots.setAttribute("aria-hidden", "true");
    dots.append(span("", "."), span("", "."), span("", "."));
    statusEl.append(dots);
  }
  if (text) replay(statusEl, "text-enter");
}

// ─── Confirm dialog ──────────────────────────────────────────────────────
const askOverlay = $<HTMLDivElement>("ask-overlay");
const askYes     = $<HTMLButtonElement>("ask-yes");
const askNo      = $<HTMLButtonElement>("ask-no");
let askDone: ((ok: boolean) => void) | null = null;

function ask(title: string, body: string, yes: string, no: string, danger = false): Promise<boolean> {
  askDone?.(false);
  $("ask-title").textContent = title;
  $("ask-body").textContent  = body;
  askYes.textContent = yes;
  askNo.textContent  = no;
  askYes.classList.toggle("danger", danger);
  askOverlay.classList.remove("hidden");
  askNo.focus();   // the safe answer is the one Enter picks
  return new Promise((resolve) => {
    askDone = (ok) => { askDone = null; askOverlay.classList.add("hidden"); resolve(ok); };
  });
}
askYes.addEventListener("click", () => askDone?.(true));
askNo.addEventListener("click",  () => askDone?.(false));
askOverlay.addEventListener("click", (e) => { if (e.target === askOverlay) askDone?.(false); });
document.addEventListener("keydown", (e) => { if (e.key === "Escape") askDone?.(false); });

// Collapse/expand a step at its real content height. The height is measured
// immediately before the class flips, so the transition has a true target
// rather than a guessed ceiling.
function setStepCompact(stepId: string, compact: boolean) {
  const step  = $(stepId);
  const exp   = step.querySelector<HTMLElement>(".step-expanded");
  const inner = step.querySelector<HTMLElement>(".step-expanded-inner");
  // Only trust a measurement taken while the step is actually laid out — a
  // hidden or already-clamped step measures 0, which would poison the value
  // the expand direction reuses.
  if (exp && inner && inner.scrollHeight > 0) {
    exp.style.setProperty("--expanded-h", `${inner.scrollHeight}px`);
  }
  step.classList.toggle("step--compact", compact);
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

// Delay between the expanded image card and handing the window to step 02.
let settleTimer: ReturnType<typeof setTimeout> | null = null;

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
  const { basename, ext } = splitImageName(filename);
  const { val, unit } = getSizeProps(selectedImageSize);
  // e.g. "ubuntu-22.04  .ISO  · 6.8 GB"
  imageCompactText.replaceChildren(
    span("compact-chip", ext || "IMG"),
    span("compact-name", basename),
    span("compact-size", `${formatSizeNum(val)} ${unit}`),
  );
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
const IMAGE_EXTS = ["img", "iso", "dmg", "bin", "raw", "xz"];

function isImagePath(path: string): boolean {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  return IMAGE_EXTS.includes(ext);
}

// Split a filename into base and extension, keeping compound extensions whole.
// Raspberry Pi OS ships "...-lite.img.xz"; splitting on the last dot alone
// would label it ".XZ" and leave "...-lite.img" as the name.
function splitImageName(filename: string): { basename: string; ext: string } {
  const compound = /\.(img|iso|dmg|bin|raw)\.xz$/i.exec(filename);
  if (compound) {
    return { basename: filename.slice(0, compound.index), ext: compound[0].toUpperCase() };
  }
  const dot = filename.lastIndexOf(".");
  return dot === -1
    ? { basename: filename, ext: "" }
    : { basename: filename.slice(0, dot), ext: filename.slice(dot).toUpperCase() };
}

async function openImagePicker() {
  const picked = await open({
    multiple: false,
    directory: false,
    filters: [
      { name: "Disk images", extensions: ["img", "iso", "dmg", "bin", "raw", "img.xz", "iso.xz", "xz"] },
      { name: "All files", extensions: ["*"] },
    ],
  });
  if (typeof picked === "string") await applyImage(picked);
}

async function applyImage(picked: string) {
  selectedImage     = picked;
  selectedImageSize = null;

  const filename = picked.split("/").pop() ?? picked;
  const isFirstPick = imageActive.classList.contains("hidden");

  // Re-open the card so the choreography has somewhere to play. On a re-pick
  // the step is sitting minimised.
  if (!isFirstPick) setStepCompact("step-image", false);

  // Parse extension and base name
  const { basename, ext } = splitImageName(filename);

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
    // Prefetch drives now so step 02 is populated the moment it appears.
    refreshDrives(true);
  }
  pingNum("step-image");

  try {
    selectedImageSize = await invoke<number>("get_file_size", { path: picked });
    animateSize(selectedImageSize);
    updateCompactImage();

    // If step 03 is already visible (re-pick while flash ready), re-validate
    if (!$("step-flash").classList.contains("step--hidden")) {
      updateFlashEnabled();
    }
  } catch (e) {
    setStatus(`Could not read that image: ${e}`, "error");
  }

  // Hold the expanded card long enough for the count-up to land. The
  // magnitude-scaled size is the point of this moment; collapsing straight into
  // step 02 meant it never painted at all.
  if (settleTimer) clearTimeout(settleTimer);
  settleTimer = setTimeout(() => {
    setStepCompact("step-image", true);
    if (isFirstPick) revealStep("step-drive", 260);
  }, 2000);

  updateFlashEnabled();
}

$<HTMLButtonElement>("pick-image").addEventListener("click", openImagePicker);
$<HTMLButtonElement>("change-image").addEventListener("click", openImagePicker);
$<HTMLButtonElement>("change-image-compact").addEventListener("click", openImagePicker);

// ─── Drag and drop ────────────────────────────────────────────────────────
const dropOverlay = $<HTMLDivElement>("drop-overlay");
const dropGlyph   = $<HTMLDivElement>("drop-glyph");
const dropTitle   = $<HTMLDivElement>("drop-title");
const dropSub     = $<HTMLDivElement>("drop-sub");

let flashing = false;

function showDrop(paths: string[]) {
  const path = paths[0];
  const many = paths.length > 1;
  const ok   = !many && path !== undefined && isImagePath(path);

  dropOverlay.classList.toggle("drop-overlay--reject", !ok);
  dropGlyph.textContent = ok ? "↓" : "✕";
  if (ok) {
    dropTitle.textContent = "DROP TO LOAD";
    dropSub.textContent   = path.split("/").pop() ?? "";
  } else if (many) {
    dropTitle.textContent = "ONE IMAGE AT A TIME";
    dropSub.textContent   = `${paths.length} files`;
  } else {
    dropTitle.textContent = "NOT A DISK IMAGE";
    dropSub.textContent   = ".img  .iso  .dmg  .bin  .raw  .img.xz";
  }
  dropOverlay.classList.remove("hidden");
}

function hideDrop() {
  dropOverlay.classList.add("hidden");
}

getCurrentWebview().onDragDropEvent(async ({ payload }) => {
  // Never swap the image out from under a running flash.
  if (flashing) return;
  if (payload.type === "enter") {
    showDrop(payload.paths);
  } else if (payload.type === "leave") {
    hideDrop();
  } else if (payload.type === "drop") {
    hideDrop();
    const path = payload.paths[0];
    if (payload.paths.length === 1 && path !== undefined && isImagePath(path)) {
      await applyImage(path);
    } else {
      setStatus("That is not a disk image — expected .img, .iso, .dmg, .bin, .raw or .img.xz.", "error");
    }
  }
});

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
        li.append(
          span("drive-opt-name", d.name),
          span("drive-opt-size", formatBytes(d.size_bytes)),
          span("drive-opt-meta", d.device_path),
        );
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
  resetFlashStep();
  driveSelector.classList.remove("drive-selector--open");

  if (d) {
    driveTriggerLabel.textContent = d.name;
    driveTriggerMeta.textContent  = `${formatBytes(d.size_bytes)} · ${d.device_path}`;
    driveSelector.classList.add("drive-selector--selected");

    $("step-drive").classList.add("step--done");
    pingNum("step-drive");

    driveCompactText.replaceChildren(
      span("compact-name", d.name),
      span("compact-faint", d.device_path),
      span("compact-size", formatBytes(d.size_bytes)),
    );

    setStepCompact("step-drive", true);
    revealStep("step-flash", 180);
  } else {
    driveTriggerLabel.textContent = "SELECT TARGET DRIVE";
    driveTriggerMeta.textContent  = "";
    driveSelector.classList.remove("drive-selector--selected");

    $("step-drive").classList.remove("step--done");
    setStepCompact("step-drive", false);
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
// Cancelling during the write corrupts the drive. Cancelling afterwards only
// skips verification — the bytes are already there. Say which one it is.
let currentPhase: FlashProgress["phase"] = "flashing";

cancelFlashBtn.addEventListener("click", async () => {
  const writing = currentPhase === "flashing";
  const ok = await ask(
    writing ? "Stop flashing?" : "Skip the check?",
    writing
      ? "The drive will be erased so a half-written image can't be mistaken for a working one. You can flash it again whenever you like."
      : "The image is already written and the drive will work. This only skips confirming it was written correctly.",
    writing ? "STOP" : "SKIP CHECK",
    writing ? "KEEP GOING" : "KEEP CHECKING",
    writing,
  );
  if (ok && flashing) await invoke("cancel_flash");
});
listen("flash-stalled", () => {
  setStatus("Drive stalled for 2 minutes — flash cancelled. Check the drive and try again.", "error");
});

// ─── Flash ────────────────────────────────────────────────────────────────
function resetFlashStep() {
  flashAgainBtn.classList.add("hidden");
  flashBtn.classList.remove("hidden");
  progressEl.classList.add("hidden");
  setStatus("");
}

// A finished drive is ejected, so macOS does not remount it and then warn when
// it is pulled. It then leaves the drive list, so the way on is choosing a
// drive again — never re-flashing the one that just left.
async function finishDrive(done: string, driveId: string) {
  try {
    await invoke("eject_drive", { driveId });
    setStatus(`${done}\nYou can remove the drive.`, "ok");
  } catch {
    setStatus(`${done}\nEject the drive before removing it.`, "ok");
  }
}

flashAgainBtn.addEventListener("click", () => {
  selectDrive(null);
  refreshDrives(true);
});

flashBtn.addEventListener("click", async () => {
  if (!selectedImage || !selectedDrive) return;
  const drive = selectedDrive;
  const ok = await ask(
    "Erase this drive?",
    `Everything on ${drive.name} (${drive.device_path}, ${formatBytes(drive.size_bytes)}) will be replaced by ${selectedImage.split("/").pop()}.`,
    "ERASE & FLASH",
    "CANCEL",
    true,
  );
  if (!ok) return;

  flashing = true;
  let finished = false;
  flashBtn.disabled   = true;
  flashBtn.classList.add("hidden");
  driveSelector.classList.add("drive-selector--disabled");
  $<HTMLButtonElement>("change-image-compact").disabled  = true;
  $<HTMLButtonElement>("change-drive-compact").disabled  = true;
  currentPhase = "flashing";
  resetRate();
  cancelFlashBtn.textContent = "CANCEL";
  cancelFlashBtn.classList.remove("hidden");
  // A previous run leaves the bar full and the label on DONE.
  progressFill.classList.remove("phase-done");
  progressFill.style.width = "0%";
  progressPhaseEl.className   = "progress-phase-label";
  progressPhaseEl.textContent = phaseLabels.flashing;
  progressBytesEl.textContent = "—";
  progressSpeedEl.textContent = "";
  progressEl.classList.remove("hidden");
  setStatus("Waiting for your password", "", true);

  try {
    await invoke("flash", { imagePath: selectedImage, driveId: drive.id });
    await finishDrive("Flash complete and verified ✓", drive.id);
    finished = true;
  } catch (e) {
    const msg = String(e);
    if (msg === "cancelled") {
      // The backend blanks the drive before reporting this, so it is a calm
      // outcome, not a warning the user has to act on.
      setStatus("Cancelled. The drive was erased and is ready to use again.", "ok");
    } else if (msg === "cancelled after write") {
      await finishDrive("Check skipped — the image is written.", drive.id);
      finished = true;
    } else if (msg.startsWith("wipe failed")) {
      setStatus(
        "Stopped, but the drive could not be erased. It holds a partial image — erase it in Disk Utility before using it.",
        "error"
      );
    } else if (msg === "Authentication cancelled.") {
      progressEl.classList.add("hidden");
      setStatus("");
    } else {
      setStatus(`Flash failed: ${e}`, "error");
    }
  } finally {
    flashing = false;
    askDone?.(false);
    flashBtn.disabled   = false;
    driveSelector.classList.remove("drive-selector--disabled");
    $<HTMLButtonElement>("change-image-compact").disabled  = false;
    $<HTMLButtonElement>("change-drive-compact").disabled  = false;
    cancelFlashBtn.classList.add("hidden");
    if (finished) {
      flashAgainBtn.classList.remove("hidden");
    } else {
      flashBtn.classList.remove("hidden");
      updateFlashEnabled();
    }
  }
});

// ─── Progress ─────────────────────────────────────────────────────────────
const phaseLabels: Record<string, string> = {
  flashing: "WRITING", wiping: "ERASING", hashing: "CHECKING", verifying: "VERIFYING", done: "DONE",
};
const phaseStatus: Record<string, string> = {
  flashing:  "Writing the image to the drive",
  wiping:    "Erasing the drive so it is safe to reuse",
  hashing:   "Finishing the image checksum",
  verifying: "Reading the drive back to check every byte",
};

// Share of the bar given to the write. It dominates the wall-clock time:
// reading back is usually 2–3× faster than writing to a USB stick, and the
// source hash runs alongside the write. The ETA uses the same assumption.
const WRITE_SHARE = 70;

// The raw rate is measured over half a second and jumps around; smooth it so
// neither the speed nor the ETA twitches.
let smoothedRate = 0;
let rateSamples  = 0;
function resetRate() { smoothedRate = 0; rateSamples = 0; }

function etaText(p: FlashProgress): string {
  // The first seconds of a write fill the OS cache and read far too fast.
  if (rateSamples < 6 || smoothedRate <= 0) return "";
  let secs = (p.total_bytes - p.bytes_written) / smoothedRate;
  if (p.phase === "flashing") secs += (p.total_bytes / smoothedRate) * (100 - WRITE_SHARE) / WRITE_SHARE;
  return secs < 60 ? "less than a minute left" : `about ${Math.round(secs / 60)} min left`;
}

listen<FlashProgress>("flash-progress", (event) => {
  const p = event.payload;
  if (p.phase !== currentPhase) {
    currentPhase = p.phase;
    resetRate();
    askDone?.(false);   // a question about the previous phase no longer applies
    if (p.phase === "wiping") cancelFlashBtn.classList.add("hidden");
    else if (p.phase !== "flashing") cancelFlashBtn.textContent = "SKIP CHECK";
  }
  const ratio = p.total_bytes > 0 ? Math.min(1, p.bytes_written / p.total_bytes) : 0;

  let pct = 0;
  if      (p.phase === "wiping")     pct = 100;   // indeterminate; keep the bar full rather than rewinding
  else if (p.phase === "flashing")   pct = ratio * WRITE_SHARE;
  else if (p.phase === "hashing")    pct = WRITE_SHARE;   // brief, and the label says what is happening
  else if (p.phase === "verifying")  pct = WRITE_SHARE + ratio * (100 - WRITE_SHARE);
  else if (p.phase === "done")       pct = 100;

  progressFill.style.width = `${pct}%`;

  const label = phaseLabels[p.phase] ?? p.phase.toUpperCase();
  if (progressPhaseEl.textContent !== label) {
    progressPhaseEl.textContent = label;
    progressPhaseEl.className = "progress-phase-label";
    replay(progressPhaseEl, p.phase === "done" ? "phase-done" : "text-enter");
  }
  if (p.phase === "done") progressFill.classList.add("phase-done");

  if (p.bytes_per_second > 0) {
    smoothedRate = smoothedRate ? smoothedRate * 0.8 + p.bytes_per_second * 0.2 : p.bytes_per_second;
    rateSamples++;
  }
  progressBytesEl.textContent = `${formatBytes(p.bytes_written)} / ${formatBytes(p.total_bytes)}`;
  const eta = p.phase === "flashing" || p.phase === "verifying" ? etaText(p) : "";
  progressSpeedEl.textContent = [smoothedRate > 0 ? `${formatBytes(smoothedRate)}/s` : "", eta]
    .filter(Boolean).join(" · ");

  // Until the first bytes land, the password dialog is what we are waiting on.
  if (p.phase === "flashing" && p.bytes_written === 0) return;
  if (phaseStatus[p.phase]) setStatus(phaseStatus[p.phase], "", true);
});

// Initial drive load (silent — no pulse on first load)
refreshDrives(true);
