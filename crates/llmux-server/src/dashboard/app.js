"use strict";

const byId = (id) => document.getElementById(id);
const fmt = new Intl.NumberFormat("ko-KR", { maximumFractionDigits: 1 });
const clock = new Intl.DateTimeFormat("ko-KR", { hour: "numeric", minute: "2-digit", second: "2-digit" });
const WINDOW_MS = 120_000;
const POLL_MS = 2_000;
const total = ({ ok, rejected, unavailable, failed }) => ok + rejected + unavailable + failed;
const table = document.querySelector("table");
let session;

function text(id, value) {
  if (byId(id).textContent !== value) byId(id).textContent = value;
}

function status(label, tone) {
  text("connection", label);
  byId("connection").dataset.tone = tone;
}

function lock(message = "") {
  if (session) {
    clearTimeout(session.timer);
    if (session.request) session.request.abort();
    session.key = "";
    session = undefined;
  }
  byId("dashboard").hidden = true;
  byId("signin").hidden = false;
  byId("lock").hidden = true;
  byId("unlock").disabled = false;
  byId("key").value = "";
  text("signin-error", message);
  status("잠김", "neutral");
  byId("key").focus();
}

byId("lock").addEventListener("click", () => lock());
byId("key-form").addEventListener("submit", (event) => {
  event.preventDefault();
  if (session) return;
  session = {
    key: byId("key").value.trim(),
    points: [],
    previous: null,
  };
  byId("key").value = "";
  byId("unlock").disabled = true;
  text("signin-error", "");
  status("연결 중", "neutral");
  poll(session);
});

async function poll(current) {
  const request = new AbortController();
  current.request = request;
  const deadline = setTimeout(() => request.abort(), 5_000);
  try {
    const response = await fetch("/dashboard/stats", {
      cache: "no-store",
      credentials: "omit",
      redirect: "error",
      headers: { Authorization: "Bearer " + current.key },
      signal: request.signal,
    });
    if (current !== session) return;
    if (response.status === 401) return lock("대시보드 키가 올바르지 않습니다.");
    if (!response.ok) throw new Error("Snapshot unavailable");
    const snapshot = await response.json();
    if (current !== session) return;
    render(current, snapshot);
    if (byId("dashboard").hidden) {
      byId("signin").hidden = true;
      byId("dashboard").hidden = false;
      byId("lock").hidden = false;
      byId("lock").focus();
    }
    byId("connection-error").hidden = true;
    status("실시간", "good");
    text("updated", "마지막 갱신 " + clock.format(new Date()));
  } catch {
    if (current !== session) return;
    if (byId("dashboard").hidden) return lock("대시보드에 연결할 수 없습니다. 다시 시도해 주세요.");
    current.previous = null;
    status("연결 끊김", "warn");
    text("connection-error", "마지막으로 받은 데이터를 표시하고 있습니다. 자동으로 다시 연결합니다.");
    byId("connection-error").hidden = false;
  } finally {
    clearTimeout(deadline);
    current.request = undefined;
  }
  current.timer = setTimeout(() => poll(current), POLL_MS);
}

function render(current, { uptime_ms: now, responses, deployments }) {
  const previous = current.previous;
  const count = total(responses);
  let rate;
  if (previous) {
    const elapsed = (now - previous.at) / 1000;
    const ok = responses.ok - previous.ok;
    const errors = count - responses.ok - previous.errors;
    if (elapsed < 0 || ok < 0 || errors < 0) current.points = [];
    else if (elapsed > 0 && elapsed < 3 * POLL_MS / 1000) {
      current.points.push({ at: now, ok: ok / elapsed, errors: errors / elapsed });
      rate = (ok + errors) / elapsed;
    }
  }
  current.previous = { at: now, ok: responses.ok, errors: count - responses.ok };
  current.points = current.points.filter(({ at }) => at > now - WINDOW_MS && at <= now);
  draw(now, current.points);

  text("rps", rate === undefined ? "—" : fmt.format(rate));
  text("total", "누적 " + fmt.format(count) + "건");
  text("success", fmt.format(responses.ok));
  text("success-note", count ? "전체의 " + fmt.format(100 * responses.ok / count) + "%" : "아직 응답 없음");
  text("limited", fmt.format(responses.unavailable));
  text("errors-note", "그 외 4xx " + fmt.format(responses.rejected) + " · 5xx " + fmt.format(responses.failed));
  text("latency", count ? fmt.format(responses.ready_us / count / 1000) + " ms" : "—");
  text("uptime", uptime(now));
  renderDeployments(deployments);
}

function uptime(ms) {
  const s = Math.floor(ms / 1000);
  if (s < 60) return s + "초";
  if (s < 3600) return Math.floor(s / 60) + "분 " + s % 60 + "초";
  if (s < 86400) return Math.floor(s / 3600) + "시간 " + Math.floor(s / 60) % 60 + "분";
  return fmt.format(Math.floor(s / 86400)) + "일 " + Math.floor(s / 3600) % 24 + "시간";
}

function draw(now, points) {
  const max = Math.max(1, ...points.map(({ ok, errors }) => Math.ceil(Math.max(ok, errors))));
  text("axis-max", fmt.format(max));
  text("axis-mid", fmt.format(max / 2));
  byId("chart-empty").hidden = points.length > 0;
  const segments = [];
  let last = -Infinity;
  for (const point of points) {
    if (point.at - last >= 3 * POLL_MS) segments.push([]);
    segments[segments.length - 1].push(point);
    last = point.at;
  }
  const x = ({ at }) => Math.max(1, Math.min(599, (at - now + WINDOW_MS) / WINDOW_MS * 600));
  const y = (rate) => 119 - rate / max * 118;
  const line = (series, key) => series.map((point, index) =>
    (index ? "L" : "M") + x(point) + " " + y(point[key])).join(" ") +
    (series.length === 1 ? "h.01" : "");
  byId("traffic-ok").setAttribute("d", segments.map((series) => line(series, "ok")).join(" "));
  byId("traffic-errors").setAttribute("d", segments.map((series) => line(series, "errors")).join(" "));
  byId("traffic-fill").setAttribute("d", segments.map((series) =>
    line(series, "ok") + "L" + x(series[series.length - 1]) + " 119 L" + x(series[0]) + " 119 Z").join(" "));
  const latest = points[points.length - 1];
  byId("traffic-latest").setAttribute("d", latest ? "M" + x(latest) + " " + y(latest.ok) + "h.01" : "");
}

function cell(row, value, className = "") {
  const td = row.insertCell();
  td.className = className;
  td.textContent = value;
  return td;
}

function stateLabel(label, tone) {
  const node = document.createElement("span");
  node.className = "route-state";
  node.dataset.tone = tone;
  node.textContent = label;
  return node;
}

function renderDeployments(deployments) {
  for (const group of Array.from(table.tBodies)) group.remove();
  let model;
  let group;
  for (const dep of deployments) {
    if (dep.model !== model) {
      model = dep.model;
      group = table.createTBody();
      const heading = group.insertRow();
      heading.className = "model-row";
      const label = document.createElement("th");
      label.scope = "rowgroup";
      label.colSpan = 7;
      label.textContent = model;
      heading.append(label);
    }
    const row = group.insertRow();
    const name = cell(row, dep.id, "deployment");
    const provider = document.createElement("small");
    provider.textContent = dep.provider;
    name.append(provider);
    cell(row, dep.priority, "priority");
    let state;
    if (dep.disabled) state = stateLabel("인증 비활성", "bad");
    else if (dep.cooldown_ms !== null) state = stateLabel("재시도 대기 · " + fmt.format(Math.ceil(dep.cooldown_ms / 1000)) + "초", "warn");
    else if (dep.in_flight !== null && dep.in_flight >= dep.concurrency_limit) state = stateLabel("동시 처리 한도 도달", "warn");
    else state = stateLabel("대기 없음", "neutral");
    cell(row, "").append(state);
    for (const value of [dep.routed, dep.failed_attempts, dep.stream_errors]) cell(row, fmt.format(value), "numeric");
    const limits = [];
    if (dep.rpm_limit !== null) limits.push("분당 최대 " + fmt.format(dep.rpm_limit) + "건");
    if (dep.concurrency_limit !== null) limits.push("처리 중 " + fmt.format(dep.in_flight) + "/" + fmt.format(dep.concurrency_limit));
    cell(row, limits.join(" · ") || "—", "limits");
  }
  text("deployment-count", fmt.format(deployments.length) + "개 배포");
}