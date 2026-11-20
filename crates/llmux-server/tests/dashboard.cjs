const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const { setImmediate: flush } = require("node:timers/promises");
const { test } = require("node:test");
const { createContext, runInContext } = require("node:vm");

const script = readFileSync(join(__dirname, "../src/dashboard/app.js"), "utf8");

test("sign-in sends a request even without AbortSignal.any and AbortSignal.timeout", async () => {
  const nodes = new Map();
  const element = (id) => {
    if (!nodes.has(id)) {
      nodes.set(id, {
        dataset: {},
        hidden: id === "dashboard",
        focus() {},
        addEventListener(type, callback) { this[type] = callback; },
      });
    }
    return nodes.get(id);
  };
  let request;
  const context = createContext({
    AbortController,
    AbortSignal: {},
    clearTimeout,
    setTimeout,
    document: { getElementById: element, querySelector() {} },
    async fetch(path, options) {
      request = { path, options };
      return { status: 401 };
    },
  });
  runInContext(script, context);
  element("key").value = "fake-key";
  element("key-form").submit({ preventDefault() {} });
  await flush();

  assert.equal(request?.path, "/dashboard/stats");
  assert.equal(request.options.headers.Authorization, "Bearer fake-key");
  assert.ok(request.options.signal instanceof AbortSignal);
  assert.equal(element("signin-error").textContent, "The dashboard key was not accepted.");
});
