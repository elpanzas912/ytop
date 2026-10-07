const URL = "http://127.0.0.1:9334/cookies";
let timer;

async function sync() {
  try {
    const [a, b] = await Promise.all([
      chrome.cookies.getAll({ domain: "youtube.com" }),
      chrome.cookies.getAll({ domain: "google.com" }),
    ]);
    await fetch(URL, {
      method: "POST",
      headers: { "X-Ytop": "1", "Content-Type": "text/plain" },
      body: JSON.stringify([...a, ...b]),
    });
  } catch (e) {
    // la app no esta abierta: se reintenta en la proxima alarma
  }
}

function arm() {
  chrome.alarms.create("sync", { periodInMinutes: 5 });
  sync();
}

chrome.runtime.onInstalled.addListener(arm);
chrome.runtime.onStartup.addListener(arm);
chrome.alarms.onAlarm.addListener(sync);
chrome.cookies.onChanged.addListener((c) => {
  if (/(youtube|google)\.com$/.test(c.cookie.domain)) {
    clearTimeout(timer);
    timer = setTimeout(sync, 5000);
  }
});
