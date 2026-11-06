import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import {
  createCipheriv,
  createHash,
  pbkdf2Sync,
  randomBytes,
} from "node:crypto";
import { existsSync, statSync } from "node:fs";
import { chmod, cp, mkdir, rename, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { crc32 } from "node:zlib";
import {
  ENGINE_DOWNLOAD_CLIENT_TIMEOUT_MS,
  ENGINE_DOWNLOAD_TIMEOUT_MS,
} from "./limits.mjs";

/**
 * The engine the app pins (`apostate.rs`): its own release, and the Chromium
 * every profile stores as its `version`. The smoke suite compares both with
 * `get_engine_install_status`, so a bump in one place cannot pass unnoticed.
 */
export const ENGINE_VERSION = "0.5.1";
export const ENGINE_CHROMIUM = "155.0.8059.31";

/** The build target and executable the app resolves for this machine, or null. */
export function engineTarget() {
  const key = `${process.platform}-${process.arch}`;
  const targets = {
    "darwin-arm64": {
      target: "macos-arm64",
      executable: "Chromium.app/Contents/MacOS/Chromium",
    },
    "win32-x64": { target: "windows-x64", executable: "chrome.exe" },
    "linux-x64": { target: "linux-x64", executable: "chrome" },
    "linux-arm64": { target: "linux-arm64", executable: "chrome" },
  };
  return targets[key] ?? null;
}

/** Where the app looks for the engine under a session's data root. */
export function engineInstallDir(dataRoot) {
  const engine = engineTarget();
  if (!engine) return null;
  return path.join(
    dataRoot,
    "data",
    "binaries",
    "apostate",
    ENGINE_VERSION,
    engine.target,
  );
}

function engineExecutable(dataRoot) {
  const dir = engineInstallDir(dataRoot);
  return dir ? path.join(dir, engineTarget().executable) : null;
}

/** Whether the engine in a data root is only the stand-in. */
function isStandIn(file) {
  return existsSync(file) && statSync(file).size < 4096;
}

/**
 * Put a stand-in engine executable where the app looks for it, so a session
 * that never opens a browser sees the engine as installed and the app does not
 * reach for the network (the session also sets
 * `DONUT_E2E_DISABLE_STARTUP_NETWORK`). It exits at once if it is ever run.
 * A real install already in place is left alone.
 */
export async function seedEngineStandIn(dataRoot) {
  const executable = engineExecutable(dataRoot);
  if (!executable || existsSync(executable)) return;
  await mkdir(path.dirname(executable), { recursive: true });
  await writeFile(
    executable,
    process.platform === "win32" ? "" : "#!/bin/sh\nexit 1\n",
  );
  if (process.platform !== "win32") await chmod(executable, 0o755);
}

/**
 * Where the cache fixture keeps a real install of the engine: the directory the
 * app itself installs into, copied whole. `DONUT_E2E_APOSTATE_PATH` pins an
 * explicit install directory (a locally built engine) and is used as given.
 */
export function defaultEnginePath(projectRoot) {
  if (process.env.DONUT_E2E_APOSTATE_PATH) {
    return path.resolve(process.env.DONUT_E2E_APOSTATE_PATH);
  }
  const engine = engineTarget();
  return path.join(
    projectRoot,
    ".cache",
    "e2e-apostate-fixture",
    ENGINE_VERSION,
    engine?.target ?? "unsupported",
  );
}

function cachedEngineExists(projectRoot) {
  const engine = engineTarget();
  if (!engine) return false;
  const executable = path.join(
    defaultEnginePath(projectRoot),
    engine.executable,
  );
  return existsSync(executable) && !isStandIn(executable);
}

async function cloneInstall(source, destination) {
  await mkdir(path.dirname(destination), { recursive: true });
  if (process.platform === "darwin") {
    try {
      execFileSync("/bin/cp", ["-cR", source, destination]);
      return;
    } catch (_error) {
      // Not on a filesystem that clones; copy the bytes.
    }
  }
  await cp(source, destination, {
    recursive: true,
    preserveTimestamps: true,
    errorOnExist: true,
  });
}

/** Seed a real engine install into a data root, before the app starts. */
export async function seedEngine(dataRoot, projectRoot) {
  const destination = engineInstallDir(dataRoot);
  assert.ok(destination, "this machine has no Apostate build");
  await rm(destination, { recursive: true, force: true });
  await cloneInstall(defaultEnginePath(projectRoot), destination);
  return destination;
}

async function downloadEngine(app) {
  await app.session.setTimeouts({ script: ENGINE_DOWNLOAD_TIMEOUT_MS });
  try {
    await app.invoke(
      "download_browser",
      { browserStr: "wayfern", version: "" },
      ENGINE_DOWNLOAD_CLIENT_TIMEOUT_MS,
    );
  } finally {
    await app.session.setTimeouts();
  }
}

/**
 * Put the install this session just downloaded into the cache fixture. The swap
 * goes through a staging copy and renames, so a suite that dies mid-copy leaves
 * the old fixture or the new one on disk, never a half-written install.
 */
async function cacheDownloadedEngine(app, projectRoot) {
  if (process.env.DONUT_E2E_APOSTATE_PATH) return;
  const destination = defaultEnginePath(projectRoot);
  const staging = `${destination}.tmp-${process.pid}`;
  await rm(staging, { recursive: true, force: true });
  try {
    await cloneInstall(engineInstallDir(app.dataRoot), staging);
    await rm(destination, { recursive: true, force: true });
    await rename(staging, destination);
  } catch (error) {
    await rm(staging, { recursive: true, force: true });
    console.warn(
      `[donut-e2e] Could not refresh the engine fixture cache: ${error}`,
    );
  }
}

/**
 * Make the real Apostate engine available to `app` and report what it runs.
 *
 * A cached or pinned install is copied in before the app starts. Without one,
 * the app downloads and verifies the pinned release itself (the same path a
 * first launch takes), and the install is kept for the next run. The cache is
 * keyed by the pinned engine release, so a new pin never reuses an old build.
 */
export async function prepareEngine(app, projectRoot) {
  assert.ok(engineTarget(), "this machine has no Apostate build");
  if (process.env.DONUT_E2E_APOSTATE_PATH || cachedEngineExists(projectRoot)) {
    await seedEngine(app.dataRoot, projectRoot);
    return {
      version: ENGINE_CHROMIUM,
      engine: ENGINE_VERSION,
      source: process.env.DONUT_E2E_APOSTATE_PATH
        ? "pinned fixture"
        : "cached fixture",
    };
  }

  if (!app.session) await app.start();
  const executable = engineExecutable(app.dataRoot);
  if (isStandIn(executable)) await rm(executable, { force: true });
  await downloadEngine(app);
  await cacheDownloadedEngine(app, projectRoot);
  return {
    version: ENGINE_CHROMIUM,
    engine: ENGINE_VERSION,
    source: "published download",
  };
}

export function wireGuardFixture() {
  return [
    "[Interface]",
    "PrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    "Address = 10.88.0.2/32",
    "DNS = 1.1.1.1",
    "",
    "[Peer]",
    "PublicKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=",
    "Endpoint = 127.0.0.1:51820",
    "AllowedIPs = 0.0.0.0/0",
    "PersistentKeepalive = 25",
    "",
  ].join("\n");
}

export function extensionZipBase64() {
  // A deterministic Manifest V3 ZIP containing only manifest.json. Generated
  // once and kept inline so the suite has no archiver dependency.
  return "UEsDBBQAAAAAAE8K9Fxo1IfNawAAAGsAAAANAAAAbWFuaWZlc3QuanNvbnsibWFuaWZlc3RfdmVyc2lvbiI6MywibmFtZSI6IkRvbnV0IEUyRSBGaXh0dXJlIiwidmVyc2lvbiI6IjEuMC4wIiwiZGVzY3JpcHRpb24iOiJJc29sYXRlZCB0ZXN0IGV4dGVuc2lvbiJ9UEsBAhQDFAAAAAAATwr0XGjUh81rAAAAawAAAA0AAAAAAAAAAAAAAIABAAAAAG1hbmlmZXN0Lmpzb25QSwUGAAAAAAEAAQA7AAAAlgAAAAAA";
}

// 1980-01-01 00:00, the earliest timestamp the ZIP format can carry. Fixed so
// two calls with the same entries produce byte-identical archives.
const DOS_TIME = 0;
const DOS_DATE = 0x0021;

/**
 * Build a ZIP archive from `entries` (`{ name, data }`) with every member
 * stored, not deflated.
 *
 * Stored is what the inline fixture above already is, and it is load-bearing
 * for the oversized fixture below: the assertion is about a request body that
 * has to stay over the limit under test, so nothing in the archive may shrink
 * the padding back under it.
 */
export function buildStoredZip(entries) {
  const locals = [];
  const central = [];
  let offset = 0;

  for (const { name, data } of entries) {
    const nameBytes = Buffer.from(name, "utf8");
    const body = Buffer.isBuffer(data) ? data : Buffer.from(data);
    const checksum = crc32(body);

    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(DOS_TIME, 10);
    local.writeUInt16LE(DOS_DATE, 12);
    local.writeUInt32LE(checksum, 14);
    local.writeUInt32LE(body.length, 18);
    local.writeUInt32LE(body.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    locals.push(local, nameBytes, body);

    const entry = Buffer.alloc(46);
    entry.writeUInt32LE(0x02014b50, 0);
    entry.writeUInt16LE(20, 4);
    entry.writeUInt16LE(20, 6);
    entry.writeUInt16LE(DOS_TIME, 12);
    entry.writeUInt16LE(DOS_DATE, 14);
    entry.writeUInt32LE(checksum, 16);
    entry.writeUInt32LE(body.length, 20);
    entry.writeUInt32LE(body.length, 24);
    entry.writeUInt16LE(nameBytes.length, 28);
    entry.writeUInt32LE(offset, 42);
    central.push(entry, nameBytes);

    offset += local.length + nameBytes.length + body.length;
  }

  const directory = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);

  return Buffer.concat([...locals, directory, end]);
}

export const OVERSIZED_EXTENSION_NAME = "Donut E2E Oversized Fixture";

/**
 * A valid Manifest V3 ZIP padded past the 2 MiB body limit axum applies by
 * default, so the raised limit on the extension routes is the only reason a
 * request carrying it can succeed.
 *
 * The padding is random bytes, and the archive stores rather than deflates
 * them, so neither the fixture nor the transport can quietly shrink the body
 * back under the limit and turn the assertion into a tautology.
 */
export function oversizedExtensionZipBase64(paddingBytes = 3 * 1024 * 1024) {
  return buildStoredZip([
    {
      name: "manifest.json",
      data: `${JSON.stringify(
        {
          manifest_version: 3,
          name: OVERSIZED_EXTENSION_NAME,
          version: "1.0.0",
          description: "Isolated oversized test extension",
        },
        null,
        2,
      )}\n`,
    },
    { name: "payload.bin", data: randomBytes(paddingBytes) },
  ]).toString("base64");
}

// What `_locales/<default_locale>/messages.json` resolves the manifest's
// placeholders to. Deliberately free of the `__MSG_` marker so a test can
// assert the stored record carries no placeholder anywhere.
export const LOCALIZED_EXTENSION_MESSAGES = {
  extName: "Donut E2E Localized Blocker",
  extDescription: "Resolved from the default locale, not the manifest",
  extAuthor: "Donut E2E Localization",
};

/**
 * A Manifest V3 ZIP shaped the way Chrome Web Store extensions actually ship:
 * `name`, `description` and `author` are `__MSG_key__` placeholders and the
 * real strings live in `_locales/<default_locale>/messages.json`. uBlock Origin
 * Lite is exactly this, which is why an importer that stores the manifest
 * verbatim shows users `__MSG_extName__`.
 *
 * Pass `messages: {}` for a locale file that resolves none of the placeholders,
 * or `messages: null` to omit the locale file entirely.
 */
export function localizedExtensionZipBase64({
  defaultLocale = "en",
  messages = LOCALIZED_EXTENSION_MESSAGES,
} = {}) {
  const entries = [
    {
      name: "manifest.json",
      data: `${JSON.stringify(
        {
          manifest_version: 3,
          name: "__MSG_extName__",
          version: "2.4.0",
          description: "__MSG_extDescription__",
          author: "__MSG_extAuthor__",
          default_locale: defaultLocale,
        },
        null,
        2,
      )}\n`,
    },
  ];
  if (messages) {
    entries.push({
      name: `_locales/${defaultLocale}/messages.json`,
      data: `${JSON.stringify(
        Object.fromEntries(
          Object.entries(messages).map(([key, message]) => [key, { message }]),
        ),
        null,
        2,
      )}\n`,
    });
  }
  return buildStoredZip(entries).toString("base64");
}

// A 1x1 PNG, inline for the same reason the ZIP above is: no encoder
// dependency, and the exact bytes are what the icon assertions compare.
const EXTENSION_ICON_PNG_BASE64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

export function extensionIconPngBase64() {
  return EXTENSION_ICON_PNG_BASE64;
}

/**
 * Write a real unpacked Manifest V3 extension at `directory` and return its
 * absolute path.
 *
 * Unlike the ZIP fixture this one declares `icons` and ships the file they
 * point at, so importing the folder exercises icon extraction for both import
 * modes: linking reads the icon straight out of the folder, copying reads it
 * back out of the ZIP the importer builds. The background service worker is
 * what makes a loaded copy observable over CDP, which registers a
 * `chrome-extension://<id>/background.js` target.
 */
export async function writeUnpackedExtension(
  directory,
  { name = "Donut E2E Unpacked", version = "1.0.0" } = {},
) {
  const absolute = path.resolve(directory);
  await mkdir(path.join(absolute, "icons"), { recursive: true });
  await writeFile(
    path.join(absolute, "manifest.json"),
    `${JSON.stringify(
      {
        manifest_version: 3,
        name,
        version,
        description: "Isolated unpacked test extension",
        icons: { 16: "icons/icon-16.png", 48: "icons/icon-48.png" },
        background: { service_worker: "background.js" },
      },
      null,
      2,
    )}\n`,
  );
  await writeFile(
    path.join(absolute, "background.js"),
    [
      "globalThis.__donutE2eExtension = chrome.runtime.id;",
      "chrome.runtime.onInstalled.addListener(() => {",
      "  console.log('donut e2e extension installed');",
      "});",
      "",
    ].join("\n"),
  );
  const icon = Buffer.from(EXTENSION_ICON_PNG_BASE64, "base64");
  for (const size of [16, 48]) {
    await writeFile(path.join(absolute, "icons", `icon-${size}.png`), icon);
  }
  return absolute;
}

export function currentHostOs() {
  return os.platform() === "darwin"
    ? "macos"
    : os.platform() === "win32"
      ? "windows"
      : "linux";
}

/**
 * Write a Chromium cookie store at schema version 24 with plaintext values.
 *
 * Plaintext is deliberate: it is what a store looks like when the source
 * browser could not reach its keyring, and it lets the suite assert that
 * import seals every row with the target profile's key. Chromium reads a row
 * whose `encrypted_value` is empty, and drops any row where both columns are
 * set, so "value cleared and encrypted_value populated" is the only shape that
 * actually loads.
 */
export function writeChromiumCookies(dbPath, cookies) {
  const db = new DatabaseSync(dbPath);
  db.exec(`
    CREATE TABLE cookies(
      creation_utc INTEGER NOT NULL,
      host_key TEXT NOT NULL,
      top_frame_site_key TEXT NOT NULL,
      name TEXT NOT NULL,
      value TEXT NOT NULL,
      encrypted_value BLOB NOT NULL DEFAULT '',
      path TEXT NOT NULL,
      expires_utc INTEGER NOT NULL,
      is_secure INTEGER NOT NULL,
      is_httponly INTEGER NOT NULL,
      last_access_utc INTEGER NOT NULL,
      has_expires INTEGER NOT NULL DEFAULT 1,
      is_persistent INTEGER NOT NULL DEFAULT 1,
      priority INTEGER NOT NULL DEFAULT 1,
      samesite INTEGER NOT NULL DEFAULT -1,
      source_scheme INTEGER NOT NULL DEFAULT 0,
      source_port INTEGER NOT NULL DEFAULT -1,
      last_update_utc INTEGER NOT NULL DEFAULT 0,
      source_type INTEGER NOT NULL DEFAULT 0,
      has_cross_site_ancestor INTEGER NOT NULL DEFAULT 0
    );
    CREATE UNIQUE INDEX cookies_unique_index
      ON cookies(host_key, top_frame_site_key, name, path);
    CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
    INSERT INTO meta VALUES('version', '24');
    INSERT INTO meta VALUES('last_compatible_version', '24');
  `);
  const insert = db.prepare(
    `INSERT INTO cookies(creation_utc, host_key, top_frame_site_key, name, value,
       encrypted_value, path, expires_utc, is_secure, is_httponly, last_access_utc)
     VALUES(?, ?, '', ?, ?, ?, '/', 0, 0, 0, 0)`,
  );
  // `encrypted` cookies are written the way Chromium's v23->v24 migration
  // does: BindString into a BLOB column, which leaves the storage class as
  // TEXT. Reading that as a strict blob returns empty and silently blanks the
  // cookie, so the suite has to reproduce it rather than only binding blobs.
  const insertAsText = db.prepare(
    `INSERT INTO cookies(creation_utc, host_key, top_frame_site_key, name, value,
       encrypted_value, path, expires_utc, is_secure, is_httponly, last_access_utc)
     VALUES(?, ?, '', ?, '', CAST(? AS TEXT), '/', 0, 0, 0, 0)`,
  );
  // A row that is already sealed (`encryptedValue`, bytes) and still alive when
  // a browser loads it (`expires`, microseconds since 1601).
  const insertSealed = db.prepare(
    `INSERT INTO cookies(creation_utc, host_key, top_frame_site_key, name, value,
       encrypted_value, path, expires_utc, is_secure, is_httponly, last_access_utc)
     VALUES(?, ?, '', ?, '', ?, '/', ?, 0, 0, 0)`,
  );
  let creation = 13000000000000000;
  for (const cookie of cookies) {
    if (cookie.encryptedValue !== undefined) {
      insertSealed.run(
        creation++,
        cookie.host,
        cookie.name,
        cookie.encryptedValue,
        cookie.expires ?? 0,
      );
    } else if (cookie.encryptedValueText === undefined) {
      insert.run(creation++, cookie.host, cookie.name, cookie.value, "");
    } else {
      insertAsText.run(
        creation++,
        cookie.host,
        cookie.name,
        cookie.encryptedValueText,
      );
    }
  }
  db.close();
}

/**
 * A cookie value sealed the way the closed engine did: with a password kept in
 * the profile's own `os_crypt_key` file (PBKDF2, 1003 rounds on macOS under
 * `v10`, one round on Linux under `v11`), behind the SHA-256 of its host.
 */
export function sealLikeTheClosedEngine(password, host, value) {
  const linux = process.platform === "linux";
  const key = pbkdf2Sync(password, "saltysalt", linux ? 1 : 1003, 16, "sha1");
  const cipher = createCipheriv("aes-128-cbc", key, Buffer.alloc(16, 0x20));
  const plain = Buffer.concat([
    createHash("sha256").update(host).digest(),
    Buffer.from(value),
  ]);
  return Buffer.concat([
    Buffer.from(linux ? "v11" : "v10"),
    cipher.update(plain),
    cipher.final(),
  ]);
}

/** Write a Chromium History database holding the given URLs. */
export function writeChromiumHistory(dbPath, urls) {
  const db = new DatabaseSync(dbPath);
  db.exec(`
    CREATE TABLE urls(
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      url LONGVARCHAR,
      title LONGVARCHAR,
      visit_count INTEGER DEFAULT 0 NOT NULL,
      typed_count INTEGER DEFAULT 0 NOT NULL,
      last_visit_time INTEGER NOT NULL,
      hidden INTEGER DEFAULT 0 NOT NULL
    );
    CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
    INSERT INTO meta VALUES('version', '69');
    INSERT INTO meta VALUES('last_compatible_version', '16');
  `);
  const insert = db.prepare(
    "INSERT INTO urls(url, title, visit_count, typed_count, last_visit_time, hidden) VALUES(?, ?, 1, 0, ?, 0)",
  );
  let visit = 13000000000000000;
  for (const url of urls) {
    insert.run(url, url, visit++);
  }
  db.close();
}

/** The name and version the CRX fixture's own manifest declares. */
export const CRX_EXTENSION_NAME = "Donut E2E Web Extension";
export const CRX_EXTENSION_VERSION = "3.2.1";

/**
 * Wrap `zip` in a CRX3 container, the shape the Chrome Web Store actually
 * serves: `Cr24`, a little-endian format version of 3, a little-endian header
 * length, that many bytes of signature header, and only then the ZIP.
 *
 * The header bytes are filler — nothing in Donut verifies the signature, and a
 * real one would need a packing key. What a test built on this proves is that
 * the importer reads the ZIP at the offset the header declares instead of
 * scanning the file for a `PK` marker, which is the bug the format invites.
 */
export function buildCrx3(zip, headerBytes = 137) {
  const prefix = Buffer.alloc(12);
  prefix.write("Cr24", 0, "ascii");
  prefix.writeUInt32LE(3, 4);
  prefix.writeUInt32LE(headerBytes, 8);
  return Buffer.concat([prefix, Buffer.alloc(headerBytes, 0x42), zip]);
}

/** A CRX3 whose payload is a real Manifest V3 archive. */
export function extensionCrx3({
  name = CRX_EXTENSION_NAME,
  version = CRX_EXTENSION_VERSION,
} = {}) {
  return buildCrx3(
    buildStoredZip([
      {
        name: "manifest.json",
        data: `${JSON.stringify(
          {
            manifest_version: 3,
            name,
            version,
            description: "Isolated test extension served over a link",
          },
          null,
          2,
        )}\n`,
      },
    ]),
  );
}

/**
 * The form page the MCP locator and typing checks run against. Each part
 * stands for a kind of field agents reported trouble with: a search input and
 * a search textarea named by id, a controlled input that keeps only what real
 * input events gave it, an autocomplete whose input is transparent and one
 * pixel wide, two fields whose names differ by a suffix, radios drawn by their
 * labels, a grid of day cells, a field the page empties after every key, and
 * a textarea far down the page beside a fixed banner that covers the bottom of
 * the window.
 */
export function mcpFormsPage() {
  const days = Array.from(
    { length: 31 },
    (_, i) =>
      `<div role="option" aria-label="Choose October ${i + 1}, 2026" data-day="${i + 1}">${i + 1}</div>`,
  ).join("");
  return `<!doctype html>
<html>
  <head>
    <title>Donut E2E MCP Forms</title>
    <style>
      body { font: 14px sans-serif; margin: 0; padding: 16px 16px 160px; }
      .row { margin: 10px 0; }
      .control { position: relative; width: 320px; min-height: 36px; border: 1px solid #888; border-radius: 4px; display: flex; align-items: center; padding: 0 8px; cursor: text; }
      .control .placeholder { position: absolute; left: 8px; color: #777; pointer-events: none; }
      .control input { width: 1px; opacity: 0; border: 0; padding: 0; outline: 0; font: inherit; background: none; }
      .choice { position: relative; display: inline-block; padding-left: 24px; margin-right: 16px; }
      .choice input { position: absolute; z-index: -1; opacity: 0; left: 0; top: 2px; width: 16px; height: 16px; margin: 0; }
      .choice label::before { content: ""; position: absolute; left: 0; top: 2px; width: 14px; height: 14px; border: 1px solid #333; border-radius: 50%; }
      .choice input:checked + label::before { background: #333; }
      .days { display: grid; grid-template-columns: repeat(7, 36px); gap: 2px; }
      .days div { height: 28px; display: flex; align-items: center; justify-content: center; border: 1px solid #ccc; cursor: pointer; }
      .spacer { height: 900px; }
      #fixedban { position: fixed; left: 0; right: 0; bottom: 0; height: 110px; z-index: 10; background: #333; color: #fff; }
    </style>
  </head>
  <body>
    <h1>MCP forms</h1>
    <div class="row"><input id="searchInput" name="search" placeholder="Search Wikipedia"></div>
    <div class="row"><textarea id="sb_form_q" name="q" rows="1" placeholder="Ask me anything"></textarea></div>
    <div class="row"><label for="react-name">Full name</label> <input id="react-name" name="fullName"></div>
    <div class="row">
      <label for="first-name">First Name</label> <input id="first-name" name="firstName">
      <label for="first-name-optional">First Name (optional)</label> <input id="first-name-optional" name="firstNameOptional">
    </div>
    <div class="row">
      <div id="subjects" class="control"><span class="placeholder">Subjects</span><input id="subjectsInput" aria-label="Subjects" autocomplete="off"></div>
      <ul id="subjects-menu"></ul>
    </div>
    <div class="row">
      <div class="choice"><input type="radio" name="gender" id="gender-radio-1" value="Male"><label for="gender-radio-1">Male</label></div>
      <div class="choice"><input type="radio" name="gender" id="gender-radio-2" value="Female"><label for="gender-radio-2">Female</label></div>
    </div>
    <div class="row"><div id="days" class="days" role="listbox" aria-label="October 2026">${days}</div></div>
    <div class="row"><input id="locked" name="locked" aria-label="Locked"></div>
    <div class="spacer"></div>
    <div class="row"><label for="currentAddress">Current Address</label><br><textarea id="currentAddress" name="currentAddress" rows="5" cols="40"></textarea></div>
    <div class="spacer"></div>
    <div id="fixedban">Advertisement</div>
    <script>
      (() => {
        const banner = document.getElementById("fixedban");
        banner.addEventListener("mousedown", () => {
          banner.dataset.clicks = String(Number(banner.dataset.clicks || 0) + 1);
        });

        // A controlled input as React keeps one: a value a script assigns moves
        // the tracker, so the input event after it changes nothing, and every
        // render puts the state back into the field.
        const named = document.getElementById("react-name");
        const native = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value");
        let state = "";
        let tracked = "";
        Object.defineProperty(named, "value", {
          configurable: true,
          get() { return native.get.call(this); },
          set(value) { tracked = String(value); native.set.call(this, value); },
        });
        named.addEventListener("input", () => {
          const now = native.get.call(named);
          if (now !== tracked) {
            tracked = now;
            state = now;
            named.dataset.state = state;
          }
        });
        setInterval(() => {
          if (native.get.call(named) !== state) {
            native.set.call(named, state);
            tracked = state;
          }
        }, 50);

        // An autocomplete: a click anywhere on the control focuses its input,
        // which grows and shows itself only once it holds text.
        const control = document.getElementById("subjects");
        const subject = document.getElementById("subjectsInput");
        const placeholder = control.querySelector(".placeholder");
        const menu = document.getElementById("subjects-menu");
        control.addEventListener("mousedown", (event) => {
          if (event.target !== subject) {
            event.preventDefault();
            subject.focus();
          }
        });
        subject.addEventListener("input", () => {
          const typed = subject.value;
          subject.style.width = typed ? typed.length + 1 + "ch" : "1px";
          subject.style.opacity = typed ? "1" : "0";
          placeholder.style.display = typed ? "none" : "";
          menu.innerHTML = ["Maths", "Physics", "Chemistry"]
            .filter((option) => typed && option.toLowerCase().startsWith(typed.toLowerCase()))
            .map((option) => "<li>" + option + "</li>")
            .join("");
        });

        // A field the page empties after every key.
        const locked = document.getElementById("locked");
        locked.addEventListener("input", () => {
          locked.value = "";
        });

        const daysGrid = document.getElementById("days");
        daysGrid.addEventListener("click", (event) => {
          const day = event.target.closest("[data-day]");
          if (day) daysGrid.dataset.picked = day.dataset.day;
        });
        window.__formsReady = true;
      })();
    </script>
  </body>
</html>`;
}
