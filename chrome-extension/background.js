const BRIDGE_ORIGIN = "http://127.0.0.1:27431";

async function callBridge(method, path, body) {
  const opts = {
    method: method || "GET",
    headers: {},
  };
  if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const resp = await fetch(`${BRIDGE_ORIGIN}${path}`, opts);
  const text = await resp.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = { raw: text };
  }
  if (!resp.ok) {
    return {
      ok: false,
      status: resp.status,
      error: (data && data.error) || `HTTP ${resp.status}`,
      data,
    };
  }
  return { ok: true, status: resp.status, data };
}

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg && msg.type === "bridge") {
    callBridge(msg.method, msg.path, msg.body)
      .then(sendResponse)
      .catch((err) => {
        sendResponse({
          ok: false,
          status: 0,
          offline: true,
          error: String(err && err.message ? err.message : err),
        });
      });
    return true;
  }
  if (msg && msg.type === "open-qxc") {
    chrome.tabs.create({ url: "https://qxc.hi.cn/#/" });
    sendResponse({ ok: true });
    return false;
  }
  return false;
});
