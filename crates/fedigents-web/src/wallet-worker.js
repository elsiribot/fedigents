import init from "./fedigents-web.js";

await init();

self.postMessage("__ready__");
