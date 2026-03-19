let currentStream = null;
let currentTimer = null;

export async function createWalletWorker() {
  const url = new URL("wallet-worker.js", self.location.href);
  const worker = new Worker(url, {
    type: "module",
    name: "fedigents-wallet"
  });
  await new Promise((resolve) => {
    const handler = (event) => {
      if (event.data === "__ready__") {
        worker.removeEventListener("message", handler);
        resolve();
      }
    };
    worker.addEventListener("message", handler);
  });
  return worker;
}

export function supportsSyncAccessHandles() {
  return (
    typeof FileSystemFileHandle !== "undefined" &&
    typeof FileSystemFileHandle.prototype?.createSyncAccessHandle === "function"
  );
}

export async function openWalletDb(fileName) {
  const root = await navigator.storage.getDirectory();
  const handle = await root.getFileHandle(fileName, { create: true });
  if (typeof handle.createSyncAccessHandle !== "function") {
    throw new Error(
      "This browser does not support OPFS Sync Access Handles. Use a recent Chromium-based browser for wallet storage."
    );
  }
  return await handle.createSyncAccessHandle();
}

export async function registerServiceWorker() {
  if ("serviceWorker" in navigator) {
    const url = new URL("sw.js", window.location.href);
    await navigator.serviceWorker.register(url.pathname);
  }
}

export async function copyText(value) {
  if (navigator.clipboard) {
    await navigator.clipboard.writeText(value);
  }
}

async function stopTracks() {
  if (currentTimer !== null) {
    clearTimeout(currentTimer);
    currentTimer = null;
  }
  if (currentStream !== null) {
    for (const track of currentStream.getTracks()) {
      track.stop();
    }
    currentStream = null;
  }
}

export async function stopQrScanner(video) {
  await stopTracks();
  if (video) {
    video.srcObject = null;
  }
}

let mediaRecorder = null;
let audioChunks = [];

export async function startRecording() {
  const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  audioChunks = [];
  mediaRecorder = new MediaRecorder(stream, { mimeType: "audio/webm" });
  mediaRecorder.addEventListener("dataavailable", (e) => {
    if (e.data.size > 0) audioChunks.push(e.data);
  });
  mediaRecorder.start();
}

export async function stopRecording() {
  if (!mediaRecorder || mediaRecorder.state === "inactive") return null;
  return new Promise((resolve) => {
    mediaRecorder.addEventListener("stop", () => {
      const blob = new Blob(audioChunks, { type: "audio/webm" });
      for (const track of mediaRecorder.stream.getTracks()) track.stop();
      mediaRecorder = null;
      audioChunks = [];
      resolve(blob);
    });
    mediaRecorder.stop();
  });
}

export async function transcribeAudio(blob, apiKey) {
  const form = new FormData();
  form.append("file", blob, "recording.webm");
  form.append("model", "nova-3");
  form.append("response_format", "json");
  const resp = await fetch("https://api.ppq.sirion.io/api/v1/audio/transcriptions", {
    method: "POST",
    headers: { "Authorization": `Bearer ${apiKey}` },
    body: form,
  });
  if (!resp.ok) {
    const text = await resp.text();
    throw new Error(`STT error ${resp.status}: ${text}`);
  }
  const data = await resp.json();
  return data.text || "";
}

export async function startQrScanner(video, callback) {
  if (!("BarcodeDetector" in globalThis)) {
    throw new Error("BarcodeDetector is not available in this browser.");
  }

  await stopTracks();

  const detector = new BarcodeDetector({ formats: ["qr_code"] });
  currentStream = await navigator.mediaDevices.getUserMedia({
    audio: false,
    video: {
      facingMode: { ideal: "environment" }
    }
  });

  video.srcObject = currentStream;
  await video.play();

  const tick = async () => {
    try {
      const codes = await detector.detect(video);
      if (codes.length > 0 && codes[0].rawValue) {
        callback(codes[0].rawValue);
        await stopQrScanner(video);
        return;
      }
    } catch (_err) {
      // Ignore transient decoder failures.
    }
    currentTimer = setTimeout(tick, 250);
  };

  currentTimer = setTimeout(tick, 250);
}
