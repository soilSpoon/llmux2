const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { setImmediate: flush } = require("node:timers/promises");
const { test } = require("node:test");
const { createContext, runInContext } = require("node:vm");

const script = readFileSync(join(__dirname, "../src/dashboard/app.js"), "utf8");

const themeKey = "llmux2.dashboard.theme";

function dashboard(fetch, localStorage) {
  const nodes = new Map();
  const timers = new Map();
  let timer = 0;
  const table = {
    tBodies: [],
    createTBody() {
      const group = {
        rows: [],
        remove() {
          table.tBodies.splice(table.tBodies.indexOf(group), 1);
        },
        insertRow() {
          const children = [];
          group.rows.push(children);
          return {
            append: (node) => children.push(node),
            insertCell() {
              const node = makeNode();
              children.push(node);
              return node;
            },
          };
        },
      };
      table.tBodies.push(group);
      return group;
    },
  };
  function makeNode() {
    return {
      dataset: {},
      children: [],
      append(node) { this.children.push(node); },
      focus() {},
      setAttribute() {},
      addEventListener(type, callback) { this[type] = callback; },
    };
  }
  const element = (id) => {
    if (!nodes.has(id)) nodes.set(id, { ...makeNode(), hidden: id === "dashboard" });
    return nodes.get(id);
  };
  const root = makeNode();
  const context = createContext({
    AbortController,
    AbortSignal: {},
    clearTimeout: (id) => timers.delete(id),
    setTimeout(callback) { timers.set(++timer, callback); return timer; },
    document: { documentElement: root, getElementById: element, querySelector: () => table, createElement: makeNode },
    localStorage,
    fetch,
  });
  runInContext(script, context);
  return {
    element, timers, root,
    get rows() { return table.tBodies.flatMap((group) => group.rows); },
    submit() {
      element("key").value = "fake-key";
      element("key-form").submit({ preventDefault() {} });
    },
    tick() {
      const [id, callback] = timers.entries().next().value;
      timers.delete(id);
      callback();
    },
  };
}

test("the locked dashboard defaults to light and preserves the chosen theme across reloads", () => {
  const saved = new Map([[themeKey, "invalid"]]);
  const storage = { getItem: (key) => saved.get(key), setItem: (key, value) => saved.set(key, value) };
  const noNetwork = () => assert.fail("Changing the theme must not contact the gateway");
  const first = dashboard(noNetwork, storage);
  assert.equal(first.root.dataset.theme, "light");
  assert.equal(first.element("theme").textContent, "어둡게");
  first.element("theme").click();
  assert.equal(first.root.dataset.theme, "dark");
  assert.equal(first.element("dashboard").hidden, true);
  assert.deepEqual([...saved], [[themeKey, "dark"]]);
  const reopened = dashboard(noNetwork, storage);
  assert.equal(reopened.root.dataset.theme, "dark");
  assert.equal(reopened.element("theme").textContent, "밝게");
  reopened.element("theme").click();
  assert.equal(reopened.root.dataset.theme, "light");
  assert.deepEqual([...saved], [[themeKey, "light"]]);
});

test("the theme can be changed even when browser storage is unavailable", () => {
  const blocked = () => { throw new Error("storage denied"); };
  const ui = dashboard(undefined, { getItem: blocked, setItem: blocked });
  assert.equal(ui.root.dataset.theme, "light");
  ui.element("theme").click();
  assert.equal(ui.root.dataset.theme, "dark");
});

test("rejected sign-in is localized and stops polling without newer AbortSignal helpers", async () => {
  let request;
  const ui = dashboard(async (path, options) => {
    request = { path, options };
    return { status: 401 };
  });
  ui.submit();
  await flush();

  assert.equal(request?.path, "/dashboard/stats");
  assert.equal(request.options.headers.Authorization, "Bearer fake-key");
  assert.ok(request.options.signal instanceof AbortSignal);
  assert.equal(ui.element("signin-error").textContent, "대시보드 키가 올바르지 않습니다.");
  assert.equal(ui.element("connection").textContent, "잠김");
  assert.equal(ui.element("dashboard").hidden, true);
  assert.equal(ui.timers.size, 0);
});

test("live metrics, routing state, and disconnect recovery are displayed in Korean", async () => {
  const dep = {
    model: "gateway-model", id: "primary", provider: "openai_compat",
    priority: 0, cooldown_ms: 1_200, rpm_limit: 20, concurrency_limit: 2, in_flight: 0,
    routed: 1234, failed_attempts: 1, stream_errors: 0,
  };
  let offline = false;
  const ui = dashboard(async () => {
    if (offline) throw new Error("offline");
    return {
      ok: true,
      async json() {
        return {
          uptime_ms: 62_000,
          responses: { ok: 1234, rejected: 1, unavailable: 2, failed: 0, ready_us: 12_370_000 },
          deployments: [dep],
        };
      },
    };
  });
  ui.submit();
  await flush();
  assert.equal(ui.element("connection").textContent, "실시간");
  assert.equal(ui.element("uptime").textContent, "1분 2초");
  assert.equal(ui.element("success").textContent, "1,234");
  assert.equal(ui.element("limited").textContent, "2");
  assert.equal(ui.element("latency").textContent, "10 ms");
  assert.equal(ui.rows[0][0].textContent, "gateway-model");
  assert.equal(ui.rows[1][2].children[0].textContent, "재시도 대기 · 2초");
  assert.equal(ui.rows[1][6].textContent, "분당 최대 20건 · 처리 중 0/2");

  offline = true;
  ui.tick();
  await flush();
  assert.equal(ui.element("connection").textContent, "연결 끊김");
  assert.equal(ui.element("connection-error").hidden, false);
  assert.equal(ui.element("success").textContent, "1,234");
  offline = false;
  ui.tick();
  await flush();
  assert.equal(ui.element("connection").textContent, "실시간");
  assert.equal(ui.element("connection-error").hidden, true);
  ui.element("lock").click();
  assert.equal(ui.element("dashboard").hidden, true);
  assert.equal(ui.timers.size, 0);
});
