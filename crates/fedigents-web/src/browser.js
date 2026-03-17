let currentStream = null;
let currentTimer = null;

export function createWalletWorker() {
  const url = new URL("wallet-worker.js", self.location.href);
  return new Worker(url, {
    type: "module",
    name: "fedigents-wallet"
  });
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
