"use strict";
const $ = id => document.getElementById(id);
let token = sessionStorage.getItem("upm-token") || "";
let timer, methods = [], methodsKey = "", lastQr = "", qrObjectUrl, historyKey = "";
const errors = {
  400: "请检查填写的信息，或重新登录以刷新认证状态。",
  401: "访问密钥不正确，请从程序日志中获取正确的密钥。",
  403: "请求来源不匹配，请使用页面的原始地址访问。",
  409: "正在进行认证或采集，请稍候再试。",
  413: "输入内容过长。"
};
function showError(message) { $("error").textContent = message; $("error").hidden = !message; }
async function api(path, body) {
  const requestToken = token;
  const response = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    headers: {Authorization: `Bearer ${requestToken}`, ...(body === undefined ? {} : {"Content-Type": "application/json"})},
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store", signal: AbortSignal.timeout(10000)
  });
  if (!response.ok) {
    if (response.status === 401 && token === requestToken) disconnect();
    throw new Error(errors[response.status] || "请求失败，请稍后重试。");
  }
  return response.status === 202 ? null : response.json();
}
function disconnect() {
  clearTimeout(timer); token = ""; sessionStorage.removeItem("upm-token");
  $("workspace").hidden = true; $("access-panel").hidden = false;
  $("badge").textContent = "未连接";
  historyKey = ""; $("history-rows").replaceChildren(); $("history-table").hidden = true;
  $("history-message").hidden = false;
  $("history-message").textContent = "正在读取记录…";
  if (qrObjectUrl) URL.revokeObjectURL(qrObjectUrl);
  qrObjectUrl = undefined; lastQr = ""; $("qr").removeAttribute("src");
}
function updateMethod() {
  const method = methods.find(m => String(m.id) === $("method").value);
  $("code-fields").hidden = method?.kind !== "code";
  $("reauth-password-fields").hidden = method?.kind !== "password";
  $("code").required = method?.kind === "code";
  $("reauth-password").required = method?.kind === "password";
  $("verify").textContent = method?.kind === "wechat" ? "生成认证二维码" : "完成验证";
}
function render(status) {
  const busy = ["authenticating", "qr_pending"].includes(status.phase);
  const active = status.phase === "monitoring";
  $("refresh-data").disabled = !active || Boolean(status.refreshing);
  $("refresh-data").textContent = status.refreshing ? "正在采集…" : "立即刷新";
  const titles = {awaiting_login: "等待登录", authenticating: "正在认证", reauth_required: "需要二次认证", qr_pending: "等待扫码", monitoring: "监控已开启"};
  $("status-title").textContent = titles[status.phase] || "等待登录";
  $("badge").textContent = titles[status.phase] || "已连接";
  $("dot").className = `dot${busy ? " busy" : active ? " active" : ""}`;
  $("status-message").textContent = status.message;
  $("login-panel").hidden = active;
  $("cancel").hidden = !busy;
  $("reauth-panel").hidden = !status.methods.length;
  $("unsupported").hidden = status.phase !== "reauth_required" || status.methods.length > 0;
  if (!$("unsupported").hidden) $("reauth-panel").hidden = false;
  const key = JSON.stringify(status.methods);
  if (methodsKey !== key) {
    methods = status.methods; methodsKey = key;
    $("method").replaceChildren(...methods.map(m => {const option = document.createElement("option"); option.value = m.id; option.textContent = m.name; return option;}));
    $("trust").checked = status.trust_device;
    updateMethod();
  }
  document.querySelectorAll("#login-form input, #login-form select, #login-form button, #reauth-form input, #reauth-form select, #reauth-form button").forEach(el => {el.disabled = busy;});
  if (!methods.length) $("verify").disabled = true;
  $("qr-panel").hidden = !status.qr_svg;
  if ((status.qr_svg || "") !== lastQr) {
    if (qrObjectUrl) URL.revokeObjectURL(qrObjectUrl);
    lastQr = status.qr_svg || "";
    if (lastQr) {qrObjectUrl = URL.createObjectURL(new Blob([lastQr], {type: "image/svg+xml"})); $("qr").src = qrObjectUrl;}
    else {qrObjectUrl = undefined; $("qr").removeAttribute("src");}
  }
  $("reading-panel").hidden = !status.reading;
  if (status.reading) {
    $("money").textContent = status.reading.money.toFixed(2);
    $("energy").textContent = status.reading.energy.toFixed(2);
    $("reading-meta").textContent = `${status.reading.room} · 最近采集 ${new Date(status.reading.sampled_at).toLocaleString("zh-CN")}${active ? "" : " · 登录后继续更新"}`;
  }
}
function renderHistory(records) {
  $("history-message").textContent = records.length ? "" : "暂无采集记录，首次成功采集后会显示在这里。";
  $("history-message").hidden = records.length > 0;
  $("history-table").hidden = records.length === 0;
  const key = JSON.stringify(records);
  if (key === historyKey) return;
  historyKey = key;
  $("history-rows").replaceChildren(...records.map(record => {
    const row = document.createElement("tr");
    const sampled = new Date(record.created_at);
    const timestamp = document.createElement("td");
    if (Number.isNaN(sampled.getTime())) timestamp.textContent = record.created_at;
    else {
      for (const value of [sampled.toLocaleDateString("zh-CN"), sampled.toLocaleTimeString("zh-CN", {hour12: false})]) {
        const line = document.createElement("span"); line.textContent = value; timestamp.append(line);
      }
    }
    row.append(timestamp);
    for (const value of [record.room_display_name, record.remaining_money.toFixed(2), record.remaining_energy.toFixed(2)]) {
      const cell = document.createElement("td"); cell.textContent = value; row.append(cell);
    }
    return row;
  }));
}
async function refresh() {
  clearTimeout(timer);
  const requestToken = token;
  try {
    const status = await api("/api/status");
    if (requestToken !== token) return;
    render(status);
    $("workspace").hidden = false; $("access-panel").hidden = true;
    sessionStorage.setItem("upm-token", token);
    try {
      const records = await api("/api/history");
      if (requestToken === token) renderHistory(records);
    } catch (error) {
      if (requestToken !== token) return;
      $("history-message").hidden = false;
      $("history-message").textContent = "历史记录读取失败，稍后自动重试。";
    }
  } catch (error) {showError(error.name === "TimeoutError" ? "连接超时，请检查服务是否运行。" : error.message);}
  if (token) timer = setTimeout(refresh, 2000);
}
async function submit(path, body) {
  showError("");
  try {await api(path, body); await refresh();}
  catch (error) {showError(error.message || "连接失败，请检查服务是否运行。");}
}
$("access-form").addEventListener("submit", async event => {
  event.preventDefault(); token = $("token").value.trim(); $("token").value = ""; showError(""); await refresh();
});
$("login-type").addEventListener("change", () => {
  const password = $("login-type").value === "password";
  $("password-fields").hidden = !password;
  $("username").required = password; $("password").required = password;
  $("login-button").textContent = password ? "登录" : "生成登录二维码";
});
$("login-type").dispatchEvent(new Event("change"));
$("login-form").addEventListener("submit", async event => {
  event.preventDefault();
  const body = {login_type: $("login-type").value, username: $("username").value, password: $("password").value};
  $("password").value = ""; await submit("/api/login", body);
});
$("method").addEventListener("change", updateMethod);
$("send-code").addEventListener("click", () => submit("/api/reauth", {method: Number($("method").value), send_code: true}));
$("reauth-form").addEventListener("submit", async event => {
  event.preventDefault();
  const body = {method: Number($("method").value), code: $("code").value, password: $("reauth-password").value, trust_device: $("trust").checked};
  $("code").value = ""; $("reauth-password").value = "";
  await submit("/api/reauth", body);
});
$("cancel").addEventListener("click", () => submit("/api/cancel", {}));
$("refresh-data").addEventListener("click", async () => {
  $("refresh-data").disabled = true;
  await submit("/api/refresh", {});
});
$("disconnect").addEventListener("click", () => {disconnect(); showError("");});
if (token) refresh();
