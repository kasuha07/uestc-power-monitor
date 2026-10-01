"use strict";
const $ = id => document.getElementById(id);
let token = sessionStorage.getItem("upm-token") || "";
let timer, methods = [], methodsKey = "", lastQr = "", qrObjectUrl, historyKey = "", configView = null;
const errors = {
  400: "请检查填写的信息，或重新登录以刷新认证状态。",
  401: "访问密钥不正确，请从程序日志中获取正确的密钥。",
  403: "请求来源不匹配，请使用页面的原始地址访问。",
  409: "正在进行认证或采集，请稍候再试。",
  413: "输入内容过长。"
};
function showError(message) { $("error").textContent = message; $("error").hidden = !message; }
async function api(path, body, timeoutMs = 10000) {
  const requestToken = token;
  const response = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    headers: {Authorization: `Bearer ${requestToken}`, ...(body === undefined ? {} : {"Content-Type": "application/json"})},
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store", signal: AbortSignal.timeout(timeoutMs)
  });
  if (!response.ok) {
    if (response.status === 401 && token === requestToken) disconnect();
    let message = errors[response.status] || "请求失败，请稍后重试。";
    try {const data = await response.json(); if (data?.message) message = data.message;} catch {}
    throw new Error(message);
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
  trendState = {sampledAt: null, retryAt: 0};
  $("trend-panel").hidden = true; $("trend-chart").replaceChildren();
  $("trend-summary").textContent = ""; $("trend-estimate").hidden = true; $("trend-legend").hidden = true;
  $("notify-test-result").textContent = "";
  configView = null; $("config-card").open = false;
  $("config-form").replaceChildren();
  $("config-meta").textContent = "正在读取配置…";
  $("config-message").textContent = "";
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
    await refreshTrend(status);
  } catch (error) {showError(error.name === "TimeoutError" ? "连接超时，请检查服务是否运行。" : error.message);}
  if (token) timer = setTimeout(refresh, 2000);
}
async function submit(path, body) {
  showError("");
  try {await api(path, body); await refresh();}
  catch (error) {showError(error.message || "连接失败，请检查服务是否运行。");}
}

// ---------------- 用电趋势 ----------------
let trendState = {sampledAt: null, retryAt: 0};
async function refreshTrend(status) {
  const sampledAt = status.reading ? status.reading.sampled_at : null;
  if (Date.now() < trendState.retryAt) return;
  if (trendState.sampledAt !== null && sampledAt === trendState.sampledAt) return;
  trendState.sampledAt = sampledAt;
  try {
    renderTrend(await api("/api/trend"));
  } catch (error) {
    trendState.sampledAt = null; trendState.retryAt = Date.now() + 60000;
    if (token) $("trend-summary").textContent = "趋势数据读取失败，稍后自动重试。";
  }
}
function formatDays(value) {
  if (value >= 365) return "一年以上";
  if (value >= 30) return `${Math.round(value)} 天`;
  return `${Math.round(value * 10) / 10} 天`;
}
function renderTrend(trend) {
  const days = trend.days || [];
  $("trend-panel").hidden = days.length < 2;
  if (days.length < 2) return;
  const estimate = trend.estimate;
  $("trend-estimate").hidden = !estimate;
  $("trend-estimate").textContent = estimate ? `预计可用 ${formatDays(estimate.days_remaining)}` : "";
  $("trend-summary").textContent = estimate
    ? `按近几天日均 ${estimate.daily_money.toFixed(2)} 元 / ${estimate.daily_energy.toFixed(1)} kWh 估算`
    : "数据积累中：再运行几天后，这里会给出预计可用天数。";
  $("trend-legend").hidden = false;
  renderTrendChart(days);
}
function renderTrendChart(days) {
  const svgNS = "http://www.w3.org/2000/svg";
  const chart = $("trend-chart");
  chart.replaceChildren();
  const svg = document.createElementNS(svgNS, "svg");
  svg.setAttribute("viewBox", "0 0 560 150");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "最近 30 天的余额与剩余电量折线图");
  const width = 560, height = 150, padX = 6, padTop = 8, padBottom = 20;
  const plotW = width - padX * 2, plotH = height - padTop - padBottom;
  const step = days.length > 1 ? plotW / (days.length - 1) : 0;
  const series = [
    {key: "money", line: "trend-line-money", dot: "trend-dot-money", label: value => `${value.toFixed(2)} 元`},
    {key: "energy", line: "trend-line-energy", dot: "trend-dot-energy", label: value => `${value.toFixed(1)} kWh`}
  ];
  for (const item of series) {
    const values = days.map(day => day[item.key]);
    let min = Math.min(...values), max = Math.max(...values);
    if (max - min < 1e-9) { min -= 1; max += 1; }
    const points = values.map((value, index) => [
      padX + index * step,
      padTop + (1 - (value - min) / (max - min)) * plotH,
      value
    ]);
    const line = document.createElementNS(svgNS, "polyline");
    line.setAttribute("class", item.line);
    line.setAttribute("points", points.map(point => `${point[0]},${point[1]}`).join(" "));
    svg.append(line);
    points.forEach(([x, y, value], index) => {
      const dot = document.createElementNS(svgNS, "circle");
      dot.setAttribute("cx", x); dot.setAttribute("cy", y); dot.setAttribute("r", 2.5);
      dot.setAttribute("class", item.dot);
      const title = document.createElementNS(svgNS, "title");
      title.textContent = `${days[index].date} · ${item.label(value)}`;
      dot.append(title);
      svg.append(dot);
    });
  }
  for (const [index, anchor] of [[0, "start"], [days.length - 1, "end"]]) {
    const label = document.createElementNS(svgNS, "text");
    label.setAttribute("x", index === 0 ? padX : padX + plotW);
    label.setAttribute("y", height - 6);
    label.setAttribute("text-anchor", anchor);
    label.setAttribute("class", "trend-axis");
    label.textContent = days[index].date;
    svg.append(label);
  }
  chart.append(svg);
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

// ---------------- 配置管理 ----------------
const SECRET_MASK = "********";
const CONFIG_SECTIONS = [
  {title: "基础", open: true, fields: [
    {path: "username", label: "学号 / 账号", type: "text", nullable: true, restart: true, hint: "仅用于启动时自动登录，页面登录不使用它"},
    {path: "password", label: "登录密码", type: "secret", restart: true},
    {path: "login_type", label: "登录方式", type: "select", options: [["password", "账号密码"], ["wechat", "微信扫码"]], restart: true},
    {path: "interval_seconds", label: "采集间隔（秒）", type: "number", min: 1},
    {path: "timezone", label: "时区（IANA）", type: "text", hint: "如 Asia/Shanghai"},
    {path: "reauth_trust_device", label: "“记住此设备”默认勾选", type: "bool"},
    {path: "service_url", label: "服务地址（可选）", type: "text", nullable: true, restart: true},
    {path: "database_url", label: "数据库地址", type: "text", restart: true},
    {path: "cookie_file", label: "Cookie 文件路径", type: "text", restart: true},
    {path: "cookie_encryption_key", label: "Cookie 加密密钥", type: "secret", restart: true, hint: "改动可能导致已保存的会话无法解密"}
  ]},
  {title: "通知", open: true, fields: [
    {path: "notify.enabled", label: "启用通知", type: "bool"},
    {path: "notify.threshold", label: "余额告警阈值（元）", type: "number"},
    {path: "notify.cooldown_minutes", label: "低余额告警冷却（分钟）", type: "number"},
    {path: "notify.startup_enabled", label: "启动通知", type: "bool"},
    {path: "notify.heartbeat_enabled", label: "每日心跳通知", type: "bool"},
    {path: "notify.heartbeat_hours", label: "心跳通知时刻", type: "csv", hint: "0–23 的整数，逗号分隔，如 9,21"},
    {path: "notify.login_failure_enabled", label: "登录失败通知", type: "bool"},
    {path: "notify.fetch_failure_enabled", label: "连续采集失败通知", type: "bool"},
    {path: "notify.fetch_failure_threshold", label: "连续失败次数阈值", type: "number"},
    {path: "notify.fetch_failure_cooldown_minutes", label: "失败通知冷却（分钟）", type: "number"},
    {path: "notify.login_retry_failure_enabled", label: "重登失败通知", type: "bool"},
    {path: "notify.login_retry_failure_threshold", label: "重登失败轮次阈值", type: "number"},
    {path: "notify.login_retry_failure_cooldown_minutes", label: "重登失败通知冷却（分钟）", type: "number"},
    {path: "notify.reauth_pending_enabled", label: "待人工二次认证提醒", type: "bool"},
    {path: "notify.reauth_pending_cooldown_minutes", label: "待人工提醒冷却（分钟）", type: "number"},
    {path: "notify.reauth_resolved_enabled", label: "会话恢复确认通知", type: "bool"}
  ]},
  {title: "通知通道", open: false, fields: [
    {path: "notify.notify_types", label: "启用的通道", type: "multi", options: [["console", "控制台"], ["webhook", "Webhook"], ["telegram", "Telegram"], ["pushover", "Pushover"], ["ntfy", "ntfy"], ["email", "邮件"]]},
    {path: "notify.retry_attempts", label: "每通道尝试次数", type: "number"},
    {path: "notify.retry_initial_delay_seconds", label: "重试初始等待（秒）", type: "number"},
    {path: "notify.retry_max_delay_seconds", label: "重试最大等待（秒）", type: "number"},
    {path: "notify.request_timeout_seconds", label: "单次请求超时（秒）", type: "number"},
    {path: "notify.webhook_url", label: "Webhook 地址", type: "text", hint: "必须 https"},
    {path: "notify.telegram_bot_token", label: "Telegram Bot Token", type: "secret"},
    {path: "notify.telegram_chat_id", label: "Telegram Chat ID", type: "text"},
    {path: "notify.pushover_api_token", label: "Pushover API Token", type: "secret"},
    {path: "notify.pushover_user_key", label: "Pushover User Key", type: "secret"},
    {path: "notify.pushover_priority", label: "Pushover 优先级", type: "number", hint: "-2 到 2；低余额告警固定为 2"},
    {path: "notify.pushover_url", label: "Pushover 跳转链接", type: "text"},
    {path: "notify.ntfy_topic_url", label: "ntfy Topic URL", type: "text"},
    {path: "notify.ntfy_token", label: "ntfy 访问令牌", type: "secret"},
    {path: "notify.ntfy_priority", label: "ntfy 优先级", type: "number", hint: "1 到 5；低余额告警固定为 5"},
    {path: "notify.ntfy_tags", label: "ntfy 标签", type: "csv", hint: "逗号分隔，如 warning,zap"},
    {path: "notify.ntfy_use_markdown", label: "ntfy 使用 Markdown", type: "bool"},
    {path: "notify.smtp_server", label: "SMTP 服务器", type: "text"},
    {path: "notify.smtp_port", label: "SMTP 端口", type: "number"},
    {path: "notify.smtp_username", label: "SMTP 用户名", type: "text"},
    {path: "notify.smtp_password", label: "SMTP 密码", type: "secret"},
    {path: "notify.smtp_from", label: "发件人地址", type: "text"},
    {path: "notify.smtp_to", label: "收件人地址", type: "text", hint: "多个用逗号分隔"},
    {path: "notify.smtp_encryption", label: "SMTP 加密方式", type: "select", options: [["starttls", "STARTTLS（587）"], ["tls", "TLS（465）"]]}
  ]},
  {title: "Web 服务", open: false, fields: [
    {path: "web.enabled", label: "启用 Web 页面", type: "bool", restart: true},
    {path: "web.bind", label: "监听地址", type: "text", restart: true, hint: "如 127.0.0.1:8080"},
    {path: "web.access_token", label: "访问密钥", type: "secret", restart: true, hint: "至少 32 字符；清除后重启会自动生成"}
  ]}
];
const cfgId = path => "cfg-" + path.replaceAll(".", "-");
function configMessage(message) { $("config-message").textContent = message; }
function configValue(source, path) { return path.split(".").reduce((node, key) => node == null ? undefined : node[key], source); }
function setConfigValue(target, path, value) {
  const keys = path.split("."); const last = keys.pop();
  let node = target;
  for (const key of keys) node = node[key] ??= {};
  node[last] = value;
}
function sameScalar(a, b) {
  if (typeof a === "number" || typeof b === "number") {
    const left = Number(a), right = Number(b);
    return !Number.isNaN(left) && !Number.isNaN(right) && left === right;
  }
  return String(a ?? "") === String(b ?? "");
}
function sameValue(a, b) {
  if (Array.isArray(a) || Array.isArray(b)) {
    return Array.isArray(a) && Array.isArray(b) && a.length === b.length && a.every((item, index) => sameValue(item, b[index]));
  }
  return sameScalar(a, b);
}
function restartTag(label) {
  const tag = document.createElement("span");
  tag.className = "restart-tag";
  tag.textContent = "重启生效";
  label.append(tag);
}
function renderConfigField(field) {
  const wrap = document.createElement("div");
  wrap.className = "config-field";
  const id = cfgId(field.path);
  const original = configValue(configView.config, field.path);
  if (field.type === "bool") {
    const label = document.createElement("label");
    label.className = "check";
    const box = document.createElement("input");
    box.type = "checkbox"; box.id = id; box.checked = Boolean(original);
    label.append(box, document.createTextNode(field.label));
    if (field.restart) restartTag(label);
    wrap.append(label);
    return wrap;
  }
  const label = document.createElement("label");
  label.htmlFor = id; label.textContent = field.label;
  if (field.restart) restartTag(label);
  let control;
  if (field.type === "select") {
    control = document.createElement("select");
    control.replaceChildren(...field.options.map(([value, text]) => {const option = document.createElement("option"); option.value = value; option.textContent = text; return option;}));
    control.value = original ?? field.options[0][0];
  } else if (field.type === "multi") {
    control = document.createElement("div");
    control.className = "config-multi";
    const selected = Array.isArray(original) ? original : [];
    control.replaceChildren(...field.options.map(([value, text]) => {
      const check = document.createElement("label"); check.className = "check";
      const box = document.createElement("input"); box.type = "checkbox"; box.value = value; box.checked = selected.includes(value);
      check.append(box, document.createTextNode(text));
      return check;
    }));
  } else {
    control = document.createElement("input");
    if (field.type === "number") {
      control.type = "number"; control.step = "any";
      if (field.min !== undefined) control.min = field.min;
      control.value = original ?? "";
    } else if (field.type === "secret") {
      control.type = "password"; control.autocomplete = "new-password";
      control.placeholder = original === SECRET_MASK ? "已设置，留空保持不变" : "";
    } else if (field.type === "csv") {
      control.value = Array.isArray(original) ? original.join(",") : String(original ?? "");
    } else {
      control.value = String(original ?? "");
    }
  }
  control.id = id;
  wrap.append(label);
  wrap.append(control);
  if (field.type === "secret" && original === SECRET_MASK) {
    const clear = document.createElement("label");
    clear.className = "check config-clear";
    const box = document.createElement("input"); box.type = "checkbox"; box.id = id + "-clear";
    clear.append(box, document.createTextNode("清除该值"));
    wrap.append(clear);
  }
  if (field.hint) {const hint = document.createElement("p"); hint.className = "hint"; hint.textContent = field.hint; wrap.append(hint);}
  return wrap;
}
function renderConfig(view) {
  configView = view;
  $("config-form").replaceChildren(...CONFIG_SECTIONS.map(section => {
    const details = document.createElement("details");
    details.className = "config-section";
    if (section.open) details.open = true;
    const summary = document.createElement("summary");
    summary.textContent = section.title;
    const grid = document.createElement("div");
    grid.className = "config-grid";
    grid.replaceChildren(...section.fields.map(renderConfigField));
    details.append(summary, grid);
    return details;
  }));
}
function readConfigField(field) {
  const id = cfgId(field.path);
  const original = configValue(configView.config, field.path);
  if (field.type === "bool") {
    const checked = $(id).checked;
    return checked === Boolean(original) ? undefined : checked;
  }
  if (field.type === "secret") {
    if ($(id + "-clear")?.checked) return null;
    const value = $(id).value;
    return !value || value === SECRET_MASK ? undefined : value;
  }
  if (field.type === "number") {
    if ($(id).value.trim() === "") return undefined;
    const value = Number($(id).value);
    if (Number.isNaN(value)) return undefined;
    return sameScalar(value, original) ? undefined : value;
  }
  if (field.type === "csv") {
    const items = $(id).value.split(",").map(item => item.trim()).filter(Boolean)
      .map(item => field.path === "notify.heartbeat_hours" ? Number(item) : item);
    if (field.path === "notify.heartbeat_hours" && items.some(item => !Number.isInteger(item) || item < 0 || item > 23)) {
      throw new Error("心跳通知时刻必须是 0–23 的整数，用逗号分隔。");
    }
    const before = Array.isArray(original) ? original : [];
    return sameValue(items, before) ? undefined : items;
  }
  if (field.type === "multi") {
    const items = [...$(id).querySelectorAll("input:checked")].map(box => box.value);
    const before = Array.isArray(original) ? original : [];
    return sameValue(items, before) ? undefined : items;
  }
  if (field.type === "select") {
    const value = $(id).value;
    return sameScalar(value, original) ? undefined : value;
  }
  const value = $(id).value.trim();
  if (field.nullable && value === "" && original != null) return null;
  return sameScalar(value, original ?? "") ? undefined : value;
}
function collectPatch() {
  const patch = {};
  for (const section of CONFIG_SECTIONS) {
    for (const field of section.fields) {
      const value = readConfigField(field);
      if (value !== undefined) setConfigValue(patch, field.path, value);
    }
  }
  return patch;
}
function describeConfigResult(view, prefix) {
  const notes = [prefix];
  if (view.warnings?.length) notes.push("以下配置项未按写入值生效（可能被环境变量或 Docker Secrets 覆盖）：" + view.warnings.map(item => item.path).join("、"));
  if (view.restart?.length) notes.push("以下配置项需重启程序后生效：" + view.restart.join("、"));
  return notes.join(" ");
}
async function loadConfigView() {
  try {
    renderConfig(await api("/api/config"));
    $("config-meta").textContent = configView.editable ? `配置文件：${configView.file}` : "未找到可编辑的 config.toml，页面修改已停用；可手动编辑配置文件后点击“重载配置”。";
    $("config-save").disabled = !configView.editable;
  } catch (error) {$("config-meta").textContent = error.message;}
}
$("config-card").addEventListener("toggle", () => {if ($("config-card").open && !configView) loadConfigView();});
$("config-form").addEventListener("submit", async event => {
  event.preventDefault();
  let patch;
  try {patch = collectPatch();} catch (error) {configMessage(error.message); return;}
  if (!Object.keys(patch).length) {configMessage("没有需要保存的修改。"); return;}
  $("config-save").disabled = true;
  try {
    renderConfig(await api("/api/config", patch));
    configMessage(describeConfigResult(configView, "已保存并应用。"));
  } catch (error) {configMessage(error.message);}
  $("config-save").disabled = false;
});
$("config-reload").addEventListener("click", async () => {
  $("config-reload").disabled = true;
  try {
    renderConfig(await api("/api/config/reload", {}));
    if (configView.editable) $("config-meta").textContent = `配置文件：${configView.file}`;
    configMessage(describeConfigResult(configView, "已从配置文件重新加载。"));
  } catch (error) {configMessage(error.message);}
  $("config-reload").disabled = false;
});
const NOTIFY_CHANNEL_NAMES = {console: "控制台", webhook: "Webhook", telegram: "Telegram", pushover: "Pushover", ntfy: "ntfy", email: "邮件"};
$("notify-test").addEventListener("click", async () => {
  $("notify-test").disabled = true;
  $("notify-test-result").textContent = "正在发送测试通知…";
  try {
    const view = await api("/api/notify/test", {}, 30000);
    if (view.message) $("notify-test-result").textContent = view.message;
    else if (!view.results.length) $("notify-test-result").textContent = "没有可测试的通知通道。";
    else {
      $("notify-test-result").textContent = view.results.map(result => {
        const name = NOTIFY_CHANNEL_NAMES[result.channel] || result.channel;
        if (result.ok) return `${name} ✓ 发送成功`;
        return result.reason === "not_configured" ? `${name} ✗ 未配置完整` : `${name} ✗ 发送失败（详情见服务日志）`;
      }).join("，");
    }
  } catch (error) {$("notify-test-result").textContent = error.message;}
  $("notify-test").disabled = false;
});
if (token) refresh();
