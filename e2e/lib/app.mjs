import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import en from "../../src/i18n/locales/en.json" with { type: "json" };
import { seedEngineStandIn } from "./fixtures.mjs";
import { WebDriverClient } from "./webdriver.mjs";

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const MAX_DIAGNOSTIC_BYTES = 20 * 1024 * 1024;
const PNG_SIGNATURE = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);

function validatedPng(encoded) {
  assert.equal(typeof encoded, "string");
  assert.ok(encoded.length <= Math.ceil((MAX_DIAGNOSTIC_BYTES * 4) / 3) + 4);
  assert.match(encoded, /^[A-Za-z0-9+/]*={0,2}$/);
  const png = Buffer.from(encoded, "base64");
  assert.ok(png.length <= MAX_DIAGNOSTIC_BYTES);
  assert.deepEqual(png.subarray(0, PNG_SIGNATURE.length), PNG_SIGNATURE);
  return png;
}

function escapedDiagnosticHtml(html) {
  assert.equal(typeof html, "string");
  assert.ok(Buffer.byteLength(html, "utf8") <= MAX_DIAGNOSTIC_BYTES);
  return html
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;");
}

function isolatedEnvironment(root, extra = {}) {
  const home = path.join(root, "home");
  const temp = path.join(root, "tmp");
  return {
    DONUTBROWSER_DATA_ROOT: path.join(root, "donut"),
    HOME: home,
    USERPROFILE: home,
    ...(process.platform === "darwin" ? { CFFIXED_USER_HOME: home } : {}),
    TMPDIR: temp,
    TMP: temp,
    TEMP: temp,
    XDG_CONFIG_HOME: path.join(root, "xdg", "config"),
    XDG_CACHE_HOME: path.join(root, "xdg", "cache"),
    XDG_DATA_HOME: path.join(root, "xdg", "data"),
    APPDATA: path.join(root, "windows", "roaming"),
    LOCALAPPDATA: path.join(root, "windows", "local"),
    LANG: "en_US.UTF-8",
    LC_ALL: "en_US.UTF-8",
    NO_PROXY: "127.0.0.1,localhost",
    no_proxy: "127.0.0.1,localhost",
    HTTP_PROXY: "",
    HTTPS_PROXY: "",
    ALL_PROXY: "",
    http_proxy: "",
    https_proxy: "",
    all_proxy: "",
    RUST_BACKTRACE: "1",
    ...extra,
  };
}

export class AppSession {
  constructor({
    name,
    root,
    application,
    driverUrl,
    cwd,
    paidPlan,
    extraEnv = {},
    args = [],
    engine = "standin",
    onboardingCompleted = true,
    settings = {},
  }) {
    this.name = name;
    this.root = root;
    this.application = application;
    this.driver = new WebDriverClient(driverUrl);
    this.cwd = cwd;
    this.paidPlan = paidPlan;
    this.extraEnv = extraEnv;
    this.args = args;
    // "standin" seeds a stand-in engine executable so the app sees the engine
    // as installed without touching the network; "none" leaves the data root
    // without one, for a session that tests the first-run download.
    this.engine = engine;
    this.onboardingCompleted = onboardingCompleted;
    // Extra keys for the seeded app_settings.json, on top of the defaults.
    this.settings = settings;
    this.session = null;
  }

  get dataRoot() {
    return path.join(this.root, "donut");
  }

  async start() {
    await Promise.all([
      mkdir(path.join(this.root, "home"), { recursive: true }),
      mkdir(path.join(this.root, "tmp"), { recursive: true }),
      mkdir(path.join(this.root, "artifacts"), { recursive: true }),
      // Windows expands the known folders from %USERPROFILE%, which points at
      // the isolated home. A missing AppData\Roaming makes
      // SHGetKnownFolderPath fail, and the app cannot resolve its base
      // directories at startup.
      ...(process.platform === "win32"
        ? ["Roaming", "Local"].map((folder) =>
            mkdir(path.join(this.root, "home", "AppData", folder), {
              recursive: true,
            }),
          )
        : []),
    ]);
    if (this.onboardingCompleted) {
      const settingsFile = path.join(
        this.dataRoot,
        "data",
        "settings",
        "app_settings.json",
      );
      await mkdir(path.dirname(settingsFile), { recursive: true });
      await writeFile(
        settingsFile,
        `${JSON.stringify(
          {
            language: "en",
            onboarding_completed: true,
            commercial_trial_acknowledged: true,
            window_resize_warning_dismissed: true,
            disable_auto_updates: true,
            // A tip opening by itself mid-test is a modal nobody asked for;
            // the tips suite turns it back on for the one session that wants it.
            tips_auto_show: false,
            // Hundreds of scripted clicks must not play through the speakers
            // of the machine running the suite.
            ui_sounds: false,
            ...this.settings,
          },
          null,
          2,
        )}\n`,
        { flag: "wx" },
      ).catch((error) => {
        if (error.code !== "EEXIST") {
          throw error;
        }
      });
    }
    if (this.engine === "standin") {
      await seedEngineStandIn(this.dataRoot);
    }
    const env = isolatedEnvironment(this.root, {
      DONUT_E2E_DISABLE_STARTUP_NETWORK: "1",
      ...(process.env.DONUT_E2E_FIXTURE_URL
        ? {
            DONUT_E2E_DNS_BLOCKLIST_BASE_URL: `${process.env.DONUT_E2E_FIXTURE_URL}/dns`,
            ...(process.env.DONUT_E2E_GEOIP_FIXTURE_READY === "1"
              ? {
                  DONUT_E2E_GEOIP_DOWNLOAD_URL: `${process.env.DONUT_E2E_FIXTURE_URL}/geoip.mmdb`,
                }
              : {}),
            // The city database has no organisation for an address; the ASN
            // one does, and it is what a proxy check reports as the exit's
            // ISP. Seeded separately so the suite can assert a real value.
            ...(process.env.DONUT_E2E_GEOIP_ASN_FIXTURE_READY === "1"
              ? {
                  DONUT_E2E_GEOIP_ASN_DOWNLOAD_URL: `${process.env.DONUT_E2E_FIXTURE_URL}/geoip-asn.mmdb`,
                }
              : {}),
          }
        : {}),
      ...(this.paidPlan ? { DONUT_E2E_PAID_PLAN: this.paidPlan } : {}),
      ...this.extraEnv,
    });
    this.session = await this.driver.createSession({
      application: this.application,
      args: this.args,
      env,
      cwd: this.cwd,
      startupTimeout: 120_000,
      // Set by run.mjs on macOS and Windows. The driver keeps the Donut window
      // off the user's screen (on macOS transparent, click-through and never
      // key, with the app as an accessory; hidden on Windows), so a suite
      // never pops a window or steals focus. Linux runs show the window on a
      // private display instead, because a hidden GTK window has no size.
      headless: process.env.DONUT_E2E_HEADLESS === "1",
    });
    await this.session.setTimeouts();
    await this.waitFor(
      async () => {
        const ready = await this.execute(
          "return document.readyState === 'complete' && Boolean(window.__TAURI_INTERNALS__);",
        );
        return ready === true;
      },
      {
        description: `${this.name} frontend and Tauri bridge`,
        timeoutMs: 60_000,
      },
    );
    await this.dismissStartupPermissions();
    return this;
  }

  async dismissStartupPermissions() {
    if (
      process.platform !== "darwin" ||
      !(await this.invoke("get_onboarding_completed"))
    ) {
      return;
    }
    const [microphone, camera] = await Promise.all([
      this.invoke("plugin:macos-permissions|check_microphone_permission"),
      this.invoke("plugin:macos-permissions|check_camera_permission"),
    ]);
    if (microphone && camera) return;

    // The dialog names whichever permission the app finds missing first, which
    // can differ from the state read above if a grant lands in between.
    const titles = [
      en.permissionDialog.titleMicrophone,
      en.permissionDialog.titleCamera,
    ];
    await this.clickElement(
      () =>
        this.execute(
          `
            const dialogs = [...document.querySelectorAll("[role='dialog']")];
            const dialog = dialogs.find((node) =>
              arguments[0].includes(node.querySelector("h2")?.textContent)
            );
            if (!dialog) return null;
            return [...dialog.querySelectorAll("button")].find(
              (button) => button.textContent.trim() === arguments[1]
            ) ?? null;
          `,
          [titles, en.permissionDialog.cancelButton],
        ),
      "startup permission dialog Cancel button",
    );
    await this.waitFor(
      () =>
        this.execute(
          `return ![...document.querySelectorAll("[role='dialog']")].some(
            (node) => arguments[0].includes(node.querySelector("h2")?.textContent)
          );`,
          [titles],
        ),
      { description: "startup permission dialog to close" },
    );
  }

  async restart() {
    await this.close();
    return this.start();
  }

  async execute(script, args = []) {
    assert.ok(this.session, `${this.name} is not started`);
    return this.session.execute(script, args);
  }

  async invoke(command, args = {}, timeoutMs = 330_000) {
    assert.ok(this.session, `${this.name} is not started`);
    const result = await this.session.executeAsync(
      `
        const done = arguments[arguments.length - 1];
        const command = arguments[0];
        const args = arguments[1];
        window.__TAURI_INTERNALS__.invoke(command, args)
          .then((value) => done({ ok: true, value }))
          .catch((error) => done({
            ok: false,
            error: typeof error === "string" ? error : (error?.message ?? JSON.stringify(error))
          }));
      `,
      [command, args],
      timeoutMs,
    );
    if (!result?.ok) {
      throw new Error(
        `Tauri command ${command} failed: ${result?.error ?? "unknown error"}`,
      );
    }
    return result.value;
  }

  async invokeError(command, args = {}) {
    try {
      await this.invoke(command, args);
    } catch (error) {
      return String(error);
    }
    throw new Error(`Expected Tauri command ${command} to fail`);
  }

  async bodyText() {
    return this.execute("return document.body?.innerText ?? '';");
  }

  async html() {
    return this.execute("return document.documentElement?.outerHTML ?? '';");
  }

  async visibleTextIncludes(text) {
    return this.execute(
      `
        const wanted = arguments[0];
        return [...document.querySelectorAll("body *")].some((node) => {
          const style = getComputedStyle(node);
          const rect = node.getBoundingClientRect();
          return style.visibility !== "hidden" && style.display !== "none" &&
            rect.width > 0 && rect.height > 0 &&
            (node.innerText ?? "").trim().includes(wanted);
        });
      `,
      [text],
    );
  }

  async waitFor(
    check,
    { timeoutMs = 20_000, intervalMs = 100, description = "condition" } = {},
  ) {
    const started = Date.now();
    let lastError;
    while (Date.now() - started < timeoutMs) {
      try {
        const value = await check();
        if (value) {
          return value;
        }
      } catch (error) {
        lastError = error;
      }
      await sleep(intervalMs);
    }
    throw new Error(
      `Timed out after ${timeoutMs}ms waiting for ${description}${lastError ? `: ${lastError}` : ""}`,
    );
  }

  async waitForText(text, timeoutMs = 20_000) {
    return this.waitFor(() => this.visibleTextIncludes(text), {
      timeoutMs,
      description: `visible text ${JSON.stringify(text)}`,
    });
  }

  async clickElement(target, description = "element") {
    let element;
    await this.waitFor(
      async () => {
        // Event-backed tables may replace a cell while its data is loading.
        // Resolve the current control on each attempt, as a browser locator does.
        element = typeof target === "function" ? await target() : target;
        if (!element) return false;
        const verdict = await this.execute(
          `
            const node = arguments[0];
            if (!(node instanceof Element) || !node.isConnected) return false;
            if (node.matches(":disabled") || node.getAttribute("aria-disabled") === "true") return false;
            node.scrollIntoView({ block: "center", inline: "center" });
            const rect = node.getBoundingClientRect();
            const x = Math.floor(rect.left + rect.width / 2);
            const y = Math.floor(rect.top + rect.height / 2);
            const hit = document.elementFromPoint(x, y);
            if (hit && (hit === node || node.contains(hit))) return true;
            const name = (el) => el
              ? el.tagName.toLowerCase() +
                (el.dataset?.slot ? '[data-slot="' + el.dataset.slot + '"]' : "") +
                (el.className && typeof el.className === "string" ? '.' + el.className.trim().split(/\\s+/).slice(0, 6).join(".") : "")
              : "nothing";
            return "its center (" + x + "," + y + ") hits " + name(hit) +
              (hit?.contains(node) ? " (an ancestor)" : "") +
              ", viewport " + innerWidth + "x" + innerHeight +
              ", opacity " + getComputedStyle(node).opacity;
          `,
          [element],
        );
        // The last miss names what covers the control, for the timeout message.
        if (typeof verdict === "string") throw new Error(verdict);
        return verdict;
      },
      { description: `pointer-interactable ${description}` },
    );
    await this.session.click(element);
  }

  async clickText(
    text,
    { exact = true, roles = ["button", "tab", "menuitem", "link"] } = {},
  ) {
    const findElement = () =>
      this.execute(
        `
        const wanted = arguments[0];
        const exact = arguments[1];
        const roles = new Set(arguments[2]);
        const candidates = [...document.querySelectorAll("button, a, [role], [data-slot='button']")];
        const visible = (node) => {
          const style = getComputedStyle(node);
          const rect = node.getBoundingClientRect();
          return style.visibility !== "hidden" && style.display !== "none" &&
            rect.width > 0 && rect.height > 0;
        };
        return candidates.find((node) => {
          const role = node.getAttribute("role") || (node.tagName === "A" ? "link" : "button");
          const label = (node.getAttribute("aria-label") || node.innerText || node.textContent || "").trim();
          return roles.has(role) && visible(node) && (exact ? label === wanted : label.includes(wanted));
        }) ?? null;
      `,
        [text, exact, roles],
      );
    await this.clickElement(findElement, JSON.stringify(text));
  }

  async clickTextIn(
    containerSelector,
    text,
    { exact = true, roles = ["button", "tab", "menuitem", "link"] } = {},
  ) {
    const findElement = () =>
      this.execute(
        `
        const containers = [...document.querySelectorAll(arguments[0])];
        const wanted = arguments[1];
        const exact = arguments[2];
        const roles = new Set(arguments[3]);
        const visible = (node) => {
          const style = getComputedStyle(node);
          const rect = node.getBoundingClientRect();
          return style.visibility !== "hidden" && style.display !== "none" &&
            rect.width > 0 && rect.height > 0;
        };
        for (const container of containers.reverse()) {
          if (!visible(container)) continue;
          const candidates = [...container.querySelectorAll("button, a, [role], [data-slot='button']")];
          const match = candidates.find((node) => {
            const role = node.getAttribute("role") || (node.tagName === "A" ? "link" : "button");
            const label = (node.getAttribute("aria-label") || node.innerText || node.textContent || "").trim();
            return roles.has(role) && visible(node) && (exact ? label === wanted : label.includes(wanted));
          });
          if (match) return match;
        }
        return null;
      `,
        [containerSelector, text, exact, roles],
      );
    await this.clickElement(
      findElement,
      `${JSON.stringify(text)} inside ${containerSelector}`,
    );
  }

  async clickSelector(selector) {
    await this.clickElement(
      () =>
        this.execute(
          `
            const node = document.querySelector(arguments[0]);
            if (!node) return null;
            const style = getComputedStyle(node);
            const rect = node.getBoundingClientRect();
            return style.visibility !== "hidden" && style.display !== "none" &&
              rect.width > 0 && rect.height > 0 ? node : null;
          `,
          [selector],
        ),
      selector,
    );
  }

  async fillSelector(selector, value) {
    const element = await this.waitFor(
      () =>
        this.execute("return document.querySelector(arguments[0]);", [
          selector,
        ]),
      { description: `selector ${selector}` },
    );
    await this.session.clear(element);
    await this.session.sendKeys(element, value);
  }

  async pressShortcut({
    key,
    meta = false,
    ctrl = false,
    alt = false,
    shift = false,
  }) {
    const modifiers = [
      ...(meta ? ["\uE03D"] : []),
      ...(ctrl ? ["\uE009"] : []),
      ...(alt ? ["\uE00A"] : []),
      ...(shift ? ["\uE008"] : []),
    ];
    const value = key === "Escape" ? "\uE00C" : key;
    const actions = [
      ...modifiers.map((modifier) => ({ type: "keyDown", value: modifier })),
      { type: "keyDown", value },
      { type: "keyUp", value },
      ...modifiers
        .toReversed()
        .map((modifier) => ({ type: "keyUp", value: modifier })),
    ];
    try {
      await this.session.command("POST", "/actions", {
        actions: [{ type: "key", id: "keyboard", actions }],
      });
    } finally {
      await this.session.command("DELETE", "/actions");
    }
  }

  async capture(label) {
    if (!this.session) {
      return;
    }
    const safe = label.replace(/[^a-z0-9_.-]+/gi, "-");
    try {
      const png = await this.session.screenshot();
      const artifact = validatedPng(png);
      // The validated response is intentionally persisted in an isolated test directory.
      await writeFile(
        path.join(this.root, "artifacts", `${safe}.png`),
        artifact,
      );
    } catch {
      // Best-effort diagnostics must never hide the original test failure.
    }
    try {
      const artifact = escapedDiagnosticHtml(await this.html());
      // Escaping makes the saved HTML inert while preserving it for diagnostics.
      await writeFile(
        path.join(this.root, "artifacts", `${safe}.html`),
        artifact,
      );
    } catch {
      // Best-effort diagnostics must never hide the original test failure.
    }
  }

  async close() {
    if (!this.session) {
      return;
    }
    const session = this.session;
    this.session = null;
    await session.close();
  }
}

export function appFromEnvironment(name, options = {}) {
  const runRoot = process.env.DONUT_E2E_RUN_ROOT;
  assert.ok(runRoot, "DONUT_E2E_RUN_ROOT is required");
  return new AppSession({
    name,
    root: options.root ?? path.join(runRoot, "sessions", name),
    application: process.env.DONUT_E2E_APP,
    driverUrl: process.env.DONUT_E2E_DRIVER_URL,
    cwd: process.env.DONUT_E2E_PROJECT_ROOT,
    paidPlan: process.env.DONUT_E2E_PAID_PLAN,
    extraEnv: options.extraEnv,
    args: options.args,
    engine: options.engine,
    onboardingCompleted: options.onboardingCompleted,
    settings: options.settings,
  });
}

export async function withApp(name, callback, options = {}) {
  const app = appFromEnvironment(name, options);
  try {
    await app.start();
    return await callback(app);
  } catch (error) {
    await app.capture("failure");
    throw error;
  } finally {
    await app.close();
  }
}
