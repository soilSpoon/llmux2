"use strict";

const byId = (id) => document.getElementById(id);
const fmt = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });
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
    session.controller.abort();
    session.key = "";
    session = undefined;
  }
  byId("dashboard").hidden = true;
  byId("signin").hidden = false;
  byId("lock").hidden = true;
  byId("unlock").disabled = false;
  byId("key").value = "";
  text("signin-error", message);
  status("Locked", "neutral");
  byId("key").focus();
}

byId("lock").addEventListener("click", () => lock());
byId("key-form").addEventListener("submit", (event) => {
  event.preventDefault();
  if (session) return;
  session = {
    key: byId("key").value.trim(),
    controller: new AbortController(),
    points: [],
    previous: null,
  };
  byId("key").value = "";
  byId("unlock").disabled = true;
  text("signin-error", "");
  status("Connecting", "neutral");
  poll(session);
});

async function poll(current) {
  try {
    const response = await fetch("/dashboard/stats", {
      cache: "no-store",
      credentials: "omit",
      redirect: "error",
      headers: { Authorization: "Bearer " + current.key },
      signal: AbortSignal.any([current.controller.signal, AbortSignal.timeout(5_000)]),
    });
    if (current !== session) return;
    if (response.status === 401) return lock("The dashboard key was not accepted.");
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
    status("Live", "good");
    text("updated", "Updated " + new Date().toLocaleTimeString());
  } catch {
    if (current !== session) return;
    if (byId("dashboard").hidden) return lock("Cannot reach the dashboard. Try again.");
    current.previous = null;
    status("Stale", "warn");
    text("connection-error", "Live updates interrupted. Showing the last snapshot; retrying automatically.");
    byId("connection-error").hidden = false;
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
  text("total", "Since startup · " + fmt.format(count));
  text("success", fmt.format(responses.ok));
  text("success-note", count ? fmt.format(100 * responses.ok / count) + "% of responses" : "No responses yet");
  text("limited", fmt.format(responses.unavailable));
  text("errors-note", fmt.format(responses.rejected) + " other 4xx · " + fmt.format(responses.failed) + " 5xx");
  text("latency", count ? fmt.format(responses.ready_us / count / 1000) + " ms" : "—");
  text("uptime", uptime(now));
  renderDeployments(deployments);
}

function uptime(ms) {
  const s = Math.floor(ms / 1000);
  if (s < 60) return s + "s";
  if (s < 3600) return Math.floor(s / 60) + "m " + s % 60 + "s";
  if (s < 86400) return Math.floor(s / 3600) + "h " + Math.floor(s / 60) % 60 + "m";
  return Math.floor(s / 86400) + "d " + Math.floor(s / 3600) % 24 + "h";
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
    segments.at(-1).push(point);
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
    line(series, "ok") + "L" + x(series.at(-1)) + " 119 L" + x(series[0]) + " 119 Z").join(" "));
}

function cell(row, value, className = "") {
  const td = row.insertCell();
  td.className = className;
  td.textContent = value;
  return td;
}

function pill(label, tone) {
  const node = document.createElement("span");
  node.className = "pill";
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
    if (dep.disabled) state = pill("Auth disabled", "bad");
    else if (dep.cooldown_ms !== null) state = pill("Cooling · " + Math.ceil(dep.cooldown_ms / 1000) + "s", "warn");
    else if (dep.in_flight !== null && dep.in_flight >= dep.concurrency_limit) state = pill("At capacity", "warn");
    else state = pill("No cooldown", "good");
    cell(row, "").append(state);
    for (const value of [dep.routed, dep.failed_attempts, dep.stream_errors]) cell(row, fmt.format(value), "numeric");
    const limits = [];
    if (dep.rpm_limit !== null) limits.push(fmt.format(dep.rpm_limit) + " rpm max");
    if (dep.concurrency_limit !== null) limits.push(dep.in_flight + "/" + dep.concurrency_limit + " active");
    cell(row, limits.join(" · ") || "—", "limits");
  }
  text("deployment-count", deployments.length + (deployments.length === 1 ? " deployment" : " deployments"));
}