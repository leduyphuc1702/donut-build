// Self-hosted account/control plane for the Vietflow build of Donut.
//
// It answers the same small set of routes the desktop app asks of Donut cloud:
// device-code sign-in, the account object (entitlements), token refresh, the
// sync token, the profile locks a launch takes, and the team features that
// fit one self-hosted account: its one-seat team, the member list, and the
// activity log the desktop reports into (kept in DATA_DIR/team.json).
// Everything else (cloud automation, remote MCP) is
// deliberately not implemented: the app's entitlement object says so, and an
// unknown route answers 404 instead of pretending to work.
//
// No dependencies. Node 22+.

import { createHmac, randomBytes, randomUUID, timingSafeEqual } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:http";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const DEFAULT_PORT = 8787;
const ACCESS_TTL_SECS = 30 * 24 * 60 * 60;
const REFRESH_TTL_SECS = 365 * 24 * 60 * 60;
const CODE_TTL_MS = 10 * 60 * 1000;
const CHALLENGE_TTL_MS = 5 * 60 * 1000;
const MAX_BODY_BYTES = 64 * 1024;
/// Events kept on disk. The log is a record of local work, not an archive.
const MAX_ACTIVITY_EVENTS = 20_000;
/// Events one report may carry, matching the desktop's batch.
const ACTIVITY_BATCH = 50;
const ACTIVITY_DEFAULT_PAGE = 50;
const ACTIVITY_MAX_PAGE = 200;

/// The target types the desktop's filter accepts.
const TARGET_TYPES = new Set([
  "profile",
  "proxy",
  "group",
  "vpn",
  "extension",
  "template",
  "status",
  "member",
  "invitation",
]);

/// The actions this control plane itself records: a launch takes a profile
/// lock, a stop releases it. The cloud records these server-side; here the
/// control plane is the only party that sees the lock, so it writes them.
const LAUNCH_ACTION = "profile.launch";
const STOP_ACTION = "profile.stop";

function envString(env, name, fallback = "") {
  const value = env[name];
  return typeof value === "string" && value.trim() ? value.trim() : fallback;
}

export function loadConfig(env = process.env) {
  const controlToken = envString(env, "CONTROL_TOKEN");
  if (controlToken.length < 16) {
    throw new Error("CONTROL_TOKEN must be set to at least 16 characters");
  }
  return {
    port: Number(envString(env, "PORT", String(DEFAULT_PORT))),
    controlToken,
    // The key a distributed build presents on first launch to mint the shared
    // account without a device code. Falls back to CONTROL_TOKEN so a small
    // deployment needs one secret, not two.
    autoLoginKey: envString(env, "AUTO_LOGIN_KEY", controlToken),
    jwtSecret: envString(env, "JWT_SECRET", controlToken),
    accountId: envString(env, "ACCOUNT_ID", "selfhosted-owner"),
    email: envString(env, "ACCOUNT_EMAIL", "owner@selfhosted.local"),
    plan: envString(env, "ACCOUNT_PLAN", "enterprise"),
    syncServerUrl: envString(env, "SYNC_SERVER_URL"),
    // The account forms a one-seat team of its own. The id is stable and
    // local, so the desktop can hold team features (the activity log, access
    // rules, profile locks) without a cloud team to join.
    teamId: envString(env, "TEAM_ID", "local-team"),
    teamName: envString(env, "TEAM_NAME", "Vietflow"),
    // Where the local log lives. A relative default keeps a plain `node
    // server.mjs` self-contained; Docker sets an absolute path on a volume.
    dataDir: envString(env, "DATA_DIR", "data"),
  };
}

function account(config) {
  const limit = 1_000_000;
  return {
    id: config.accountId,
    email: config.email,
    plan: config.plan,
    planPeriod: "lifetime",
    subscriptionStatus: "active",
    profileLimit: limit,
    cloudProfilesUsed: 0,
    proxyBandwidthLimitMb: 0,
    proxyBandwidthUsedMb: 0,
    proxyBandwidthExtraMb: 0,
    teamId: config.teamId || "local-team",
    teamName: config.teamName || "Vietflow",
    teamRole: "owner",
    effectivePlan: config.plan,
    deviceOrdinal: 1,
    deviceCount: 1,
    isPrimaryDevice: true,
    // Read by this build to allow a local equivalent for services the cloud
    // runs remotely (remote sessions, cookie bot) without a sync server.
    selfHosted: true,
    // Capabilities this control plane actually serves. The services still
    // owned by Donut cloud stay false so the desktop does not offer a button
    // that can only fail.
    entitlements: {
      active: true,
      browserAutomation: true,
      crossOsFingerprints: true,
      cloudBackup: false,
      teamCollaboration: true,
      cookieBot: false,
      remoteInteractive: false,
      remoteControl: false,
      scheduledAutomation: false,
      agent: true,
      cookieBot: true,
      remoteInteractive: true,
      profileLimit: limit,
      requestsPerHour: 0,
      remoteBrowserHours: 0,
    },
  };
}

/// The owner's member row, as the desktop's access table reads it.
function ownerMember(config) {
  return {
    userId: config.accountId,
    email: config.email,
    role: "owner",
    groupAccess: null,
  };
}

function activityError(res, message, code = "TEAM_ACTIVITY_INVALID") {
  return sendJson(res, 400, { message, code });
}

function clip(value, max) {
  return typeof value === "string" ? value.slice(0, max) : "";
}

/// One reported event, normalized to the shape the desktop reads back.
/// Anything this build cannot name is refused rather than stored blind: a
/// viewer would show an untranslated line for it forever.
function checkedEvent(config, raw, at) {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const action = clip(raw.action, 64);
  if (!/^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$/.test(action)) return null;
  const targetType = clip(raw.targetType, 64) || action.split(".")[0];
  if (!TARGET_TYPES.has(targetType)) return null;
  const targetId = raw.targetId == null ? null : clip(String(raw.targetId), 128);
  const targetName = raw.targetName == null ? null : clip(String(raw.targetName), 200);
  const detail =
    raw.detail && typeof raw.detail === "object" && !Array.isArray(raw.detail)
      ? raw.detail
      : null;
  return {
    at,
    actor: { userId: config.accountId, email: config.email, kind: "desktop" },
    action,
    target: { type: targetType, id: targetId || null, name: targetName || null },
    detail,
    source: "client",
  };
}

/// What a cut-down half of a page of JSON looks like in a spreadsheet cell.
function csvCell(value) {
  const text = value == null ? "" : typeof value === "string" ? value : JSON.stringify(value);
  return /[",\n\r]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
}

/// The team log, held in memory and written to one JSON file.
///
/// One writer (this Node process) and one file: a write per report is enough,
/// and a reload must never cost a person their record. A file this process
/// cannot read is started over rather than refusing the whole control plane.
function createTeamStore(config) {
  const dataDir = config.dataDir || "data";
  const file = join(dataDir, "team.json");
  let writable = true;
  let state;

  const fresh = () => ({
    version: 1,
    joinedAt: new Date().toISOString(),
    nextId: 1,
    events: [],
  });

  try {
    mkdirSync(dataDir, { recursive: true });
    if (existsSync(file)) {
      const stored = JSON.parse(readFileSync(file, "utf8"));
      const events = Array.isArray(stored.events) ? stored.events : [];
      state = {
        version: 1,
        joinedAt: typeof stored.joinedAt === "string" ? stored.joinedAt : fresh().joinedAt,
        nextId: Number.isSafeInteger(stored.nextId) && stored.nextId > 0 ? stored.nextId : events.length + 1,
        events: events.filter(
          (event) =>
            event &&
            Number.isSafeInteger(event.id) &&
            typeof event.at === "string" &&
            typeof event.action === "string",
        ),
      };
    }
  } catch (error) {
    console.error(`control-plane team log unreadable, starting over: ${error.message}`);
  }

  if (!state) {
    state = fresh();
    state.events.push({
      id: state.nextId++,
      at: state.joinedAt,
      actor: { userId: config.accountId, email: config.email, kind: "system" },
      action: "member.join",
      target: { type: "member", id: config.accountId, name: null },
      detail: { role: "owner" },
      source: "server",
    });
  }

  const persist = () => {
    if (!writable) return;
    try {
      const tmp = `${file}.${process.pid}.tmp`;
      writeFileSync(tmp, JSON.stringify(state));
      renameSync(tmp, file);
    } catch (error) {
      // Once is enough: a full disk would otherwise log per edit.
      writable = false;
      console.error(`control-plane team log not writable: ${error.message}`);
    }
  };

  persist();

  return {
    owner: () => ownerMember(config),
    /// Add the events a report carries, returning how many were accepted.
    report: (events) => {
      const at = new Date().toISOString();
      const accepted = [];
      for (const raw of events.slice(0, ACTIVITY_BATCH)) {
        const event = checkedEvent(config, raw, at);
        if (event) accepted.push(event);
      }
      if (accepted.length === 0) return 0;
      for (const event of accepted) {
        state.events.push({ id: state.nextId++, ...event });
      }
      const overflow = state.events.length - MAX_ACTIVITY_EVENTS;
      if (overflow > 0) state.events.splice(0, overflow);
      persist();
      return accepted.length;
    },
    /// Record something the control plane saw itself.
    note: (action, targetId, detail = null) => {
      if (!TARGET_TYPES.has(action.split(".")[0])) return;
      state.events.push({
        id: state.nextId++,
        at: new Date().toISOString(),
        actor: { userId: config.accountId, email: config.email, kind: "system" },
        action,
        target: { type: action.split(".")[0], id: targetId, name: null },
        detail,
        source: "server",
      });
      const overflow = state.events.length - MAX_ACTIVITY_EVENTS;
      if (overflow > 0) state.events.splice(0, overflow);
      persist();
    },
    /// A filtered page, newest first. `cursor` is the last id of the page
    /// before it, so ids keep paging stable while new events arrive.
    page: ({ cursor, limit, action, actorUserId, targetType, since }) => {
      const prefix = action?.endsWith(".*") ? action.slice(0, -1) : null;
      const sinceAt = since ? Date.parse(since) : null;
      const matches = state.events
        .filter((event) => {
          if (cursor != null && event.id >= cursor) return false;
          if (prefix ? !event.action.startsWith(prefix) : action && event.action !== action) {
            return false;
          }
          if (actorUserId && event.actor?.userId !== actorUserId) return false;
          if (targetType && event.target?.type !== targetType) return false;
          if (sinceAt != null && Date.parse(event.at) < sinceAt) return false;
          return true;
        })
        .sort((a, b) => b.id - a.id);
      const items = matches.slice(0, limit);
      return {
        items,
        nextCursor:
          matches.length > items.length && items.length > 0
            ? String(items[items.length - 1].id)
            : null,
      };
    },
    /// Every match, for the CSV export.
    all: ({ action, actorUserId, targetType, since }) => {
      const prefix = action?.endsWith(".*") ? action.slice(0, -1) : null;
      const sinceAt = since ? Date.parse(since) : null;
      return state.events
        .filter((event) => {
          if (prefix ? !event.action.startsWith(prefix) : action && event.action !== action) {
            return false;
          }
          if (actorUserId && event.actor?.userId !== actorUserId) return false;
          if (targetType && event.target?.type !== targetType) return false;
          if (sinceAt != null && Date.parse(event.at) < sinceAt) return false;
          return true;
        })
        .sort((a, b) => b.id - a.id);
    },
  };
}

/// Agent routines and their runs, in one local file.
///
/// The desktop still runs the task: this holds the routine list and the run
/// records it reports back, so the Agent page works without Donut cloud. The
/// model the engine talks to is its own; nothing here calls one.
function createAgentStore(config) {
  const dataDir = config.dataDir || "data";
  const file = join(dataDir, "agent.json");
  const fresh = () => ({
    version: 1,
    nextRoutine: 1,
    nextBatch: 1,
    nextRun: 1,
    routines: [],
    runs: [],
  });
  let writable = true;
  let state;

  try {
    mkdirSync(dataDir, { recursive: true });
    if (existsSync(file)) {
      const stored = JSON.parse(readFileSync(file, "utf8"));
      state = {
        ...fresh(),
        ...stored,
        routines: Array.isArray(stored.routines) ? stored.routines : [],
        runs: Array.isArray(stored.runs) ? stored.runs : [],
      };
    }
  } catch (error) {
    console.error(`control-plane agent store unreadable, starting over: ${error.message}`);
  }
  if (!state) {
    state = fresh();
    persistAgent();
  }

  function persistAgent() {
    if (!writable) return;
    try {
      const tmp = `${file}.${process.pid}.tmp`;
      writeFileSync(tmp, JSON.stringify(state));
      renameSync(tmp, file);
    } catch (error) {
      writable = false;
      console.error(`control-plane agent store not writable: ${error.message}`);
    }
  }

  const now = () => new Date().toISOString();

  /// The routine a request describes, or the reason it cannot be stored. The
  /// desktop validates too; this is the authority for a self-hosted server.
  const checkedRoutine = (body) => {
    if (!body || typeof body !== "object" || Array.isArray(body)) {
      return { error: "Invalid routine" };
    }
    const name = clip(String(body.name ?? "").trim(), 120);
    if (!name) return { error: "NAME_CANNOT_BE_EMPTY" };
    const prompt = clip(String(body.prompt ?? "").trim(), 8_000);
    if (!prompt) return { error: "AGENT_PROMPT_EMPTY" };
    let startUrl = body.startUrl == null ? null : clip(String(body.startUrl).trim(), 2_048);
    if (startUrl) {
      try {
        const parsed = new URL(startUrl);
        if (!["http:", "https:"].includes(parsed.protocol)) startUrl = null;
      } catch {
        return { error: "AGENT_START_URL_INVALID" };
      }
    }
    const profileIds = Array.isArray(body.profileIds)
      ? body.profileIds.map((id) => clip(String(id), 128)).filter(Boolean)
      : [];
    let schedule = null;
    if (body.schedule && typeof body.schedule === "object") {
      const cron = clip(String(body.schedule.cron ?? "").trim(), 128);
      const timezone = clip(String(body.schedule.timezone ?? "").trim(), 64);
      const fields = cron.split(/\s+/).filter(Boolean).length;
      if (!(fields >= 5 && fields <= 6) || !timezone) {
        return { error: "AGENT_SCHEDULE_INVALID" };
      }
      schedule = { cron, timezone };
    }
    const maxCredits = Number(body.maxCreditsPerRun);
    const concurrency = Number(body.concurrency);
    return {
      routine: {
        name,
        prompt,
        startUrl,
        profileIds,
        schedule,
        maxCreditsPerRun:
          Number.isFinite(maxCredits) && maxCredits > 0 ? Math.floor(maxCredits) : null,
        concurrency:
          Number.isFinite(concurrency) && concurrency > 0
            ? Math.min(Math.floor(concurrency), 50)
            : null,
        enabled: body.enabled !== false,
      },
    };
  };

  return {
    credits: () => {
      const used = state.runs.reduce((sum, run) => sum + (Number(run.creditsUsed) || 0), 0);
      return {
        allowance: 1_000_000,
        used,
        extra: 0,
        balance: Math.max(1_000_000 - used, 0),
        periodEnd: null,
        plan: config.plan,
        unlimited: true,
      };
    },
    mintToken: () => ({
      token: randomBytes(32).toString("base64url"),
      expiresAt: new Date(Date.now() + 15 * 60 * 1000).toISOString(),
    }),
    listRoutines: () => [...state.routines],
    routine: (id) => state.routines.find((routine) => routine.id === id) ?? null,
    createRoutine: (body) => {
      const checked = checkedRoutine(body);
      if (checked.error) return checked;
      const at = now();
      const routine = {
        id: `routine-${state.nextRoutine++}`,
        ...checked.routine,
        createdAt: at,
        updatedAt: at,
      };
      state.routines.push(routine);
      persistAgent();
      return { routine };
    },
    updateRoutine: (id, body) => {
      const index = state.routines.findIndex((routine) => routine.id === id);
      if (index < 0) return { error: "AGENT_ROUTINE_NOT_FOUND" };
      const checked = checkedRoutine(body);
      if (checked.error) return checked;
      const routine = {
        ...state.routines[index],
        ...checked.routine,
        updatedAt: now(),
      };
      state.routines[index] = routine;
      persistAgent();
      return { routine };
    },
    deleteRoutine: (id) => {
      const index = state.routines.findIndex((routine) => routine.id === id);
      if (index < 0) return false;
      state.routines.splice(index, 1);
      persistAgent();
      return true;
    },
    runs: (routineId) =>
      state.runs
        .filter((run) => run.routineId === routineId)
        .sort((a, b) => String(b.createdAt).localeCompare(String(a.createdAt))),
    startRuns: (routineId, profileIds, batchId) => {
      const at = now();
      const batch = batchId || `batch-${state.nextBatch++}`;
      const runs = profileIds.map((profileId) => {
        const run = {
          runId: `run-${state.nextRun++}`,
          routineId,
          batchId: batch,
          profileId,
          status: "running",
          summary: null,
          creditsUsed: 0,
          error: null,
          createdAt: at,
          finishedAt: null,
        };
        state.runs.push(run);
        return run;
      });
      persistAgent();
      return { batchId: batch, runs };
    },
    reportRun: (runId, patch) => {
      const run = state.runs.find((item) => item.runId === runId);
      if (!run) return null;
      if (typeof patch?.status === "string") run.status = clip(patch.status, 32);
      if (typeof patch?.summary === "string") run.summary = clip(patch.summary, 4_000);
      if (typeof patch?.error === "string") run.error = clip(patch.error, 500);
      const credits = Number(patch?.creditsUsed);
      if (Number.isFinite(credits) && credits >= 0) run.creditsUsed = Math.floor(credits);
      if (["done", "failed", "stopped", "no_credits", "budget"].includes(run.status)) {
        run.finishedAt = now();
      }
      persistAgent();
      return run;
    },
  };
}

function base64url(value) {
  return Buffer.from(value).toString("base64url");
}

function signToken(config, payload) {
  const now = Math.floor(Date.now() / 1000);
  const header = base64url(JSON.stringify({ alg: "HS256", typ: "JWT" }));
  const body = base64url(JSON.stringify({ iat: now, ...payload }));
  const signature = createHmac("sha256", config.jwtSecret)
    .update(`${header}.${body}`)
    .digest("base64url");
  return `${header}.${body}.${signature}`;
}

function verifyToken(config, token) {
  const parts = token.split(".");
  if (parts.length !== 3) return null;
  const expected = createHmac("sha256", config.jwtSecret)
    .update(`${parts[0]}.${parts[1]}`)
    .digest();
  const actual = Buffer.from(parts[2], "base64url");
  if (actual.length !== expected.length || !timingSafeEqual(actual, expected)) {
    return null;
  }
  try {
    const payload = JSON.parse(
      Buffer.from(parts[1], "base64url").toString("utf8"),
    );
    if (typeof payload.exp === "number" && payload.exp <= Date.now() / 1000) {
      return null;
    }
    return payload;
  } catch {
    return null;
  }
}

function safeEqual(left, right) {
  const a = Buffer.from(left);
  const b = Buffer.from(right);
  return a.length === b.length && timingSafeEqual(a, b);
}

function accessToken(config) {
  const now = Math.floor(Date.now() / 1000);
  return signToken(config, {
    sub: config.accountId,
    email: config.email,
    scope: "account",
    jti: randomUUID(),
    exp: now + ACCESS_TTL_SECS,
  });
}

// The body both sign-in routes return. One account, so the payload is the
// same whether it came from a one-time code or the build's provisioning key.
function loginPayload(config) {
  return {
    accessToken: accessToken(config),
    refreshToken: refreshToken(config),
    user: account(config),
  };
}

function refreshToken(config) {
  const now = Math.floor(Date.now() / 1000);
  return signToken(config, {
    sub: config.accountId,
    scope: "refresh",
    jti: randomUUID(),
    exp: now + REFRESH_TTL_SECS,
  });
}

function syncToken(config, server) {
  const now = Math.floor(Date.now() / 1000);
  const audience = server || config.syncServerUrl || "self-hosted";
  return signToken(config, {
    sub: config.accountId,
    scope: "server-sync",
    aud: audience,
    exp: now + ACCESS_TTL_SECS,
  });
}

function bearer(req) {
  const header = req.headers.authorization || "";
  return header.startsWith("Bearer ") ? header.slice("Bearer ".length) : "";
}

function authorized(config, req) {
  const token = bearer(req);
  if (!token) return null;
  if (safeEqual(token, config.controlToken)) {
    return { sub: config.accountId, scope: "account" };
  }
  const claims = verifyToken(config, token);
  return claims?.scope === "account" ? claims : null;
}

function sendJson(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    "Content-Type": "application/json; charset=utf-8",
    "Cache-Control": "no-store",
    "X-Content-Type-Options": "nosniff",
  });
  res.end(payload);
}

function sendHtml(res, status, html) {
  res.writeHead(status, {
    "Content-Type": "text/html; charset=utf-8",
    "Cache-Control": "no-store",
    "X-Content-Type-Options": "nosniff",
    "X-Frame-Options": "DENY",
    "Referrer-Policy": "no-referrer",
    "Content-Security-Policy":
      "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'",
  });
  res.end(html);
}

async function readBody(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > MAX_BODY_BYTES) throw new Error("body too large");
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString("utf8");
}

async function readJson(req) {
  const raw = await readBody(req);
  if (!raw) return {};
  return JSON.parse(raw);
}

function parseForm(raw) {
  return new URLSearchParams(raw);
}

function htmlPage(title, body) {
  return `<!doctype html>
<html lang="vi">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${title}</title>
<style>
:root { color-scheme: light dark; }
body { font: 16px/1.5 system-ui, sans-serif; max-width: 40rem; margin: 3rem auto; padding: 0 1.25rem; }
h1 { font-size: 1.35rem; margin-bottom: .5rem; }
p { margin: .5rem 0 1rem; }
label { display: block; font-size: .85rem; margin-bottom: .35rem; }
input { width: 100%; box-sizing: border-box; padding: .7rem; border: 1px solid #8888; border-radius: .5rem; font: inherit; }
button { margin-top: 1rem; padding: .7rem 1.1rem; border: 0; border-radius: .5rem; font: inherit; cursor: pointer; background: #2563eb; color: white; }
code { display: block; margin: 1rem 0; padding: 1rem; font-size: 1.6rem; letter-spacing: .2em; text-align: center; border: 1px solid #8888; border-radius: .5rem; }
.error { color: #b91c1c; }
.muted { color: #666; font-size: .85rem; }
</style>
</head>
<body>${body}</body>
</html>`;
}

function linkForm(error = "") {
  return htmlPage(
    "Donut tự lưu trữ",
    `<h1>Đăng nhập Donut tự lưu trữ</h1>
<p>Nhập <strong>CONTROL_TOKEN</strong> của máy chủ này để lấy mã đăng nhập dùng một lần.</p>
${error ? `<p class="error">${error}</p>` : ""}
<form method="post" action="/auth/link">
<label for="token">CONTROL_TOKEN</label>
<input id="token" name="token" type="password" autocomplete="current-password" required autofocus>
<button type="submit">Lấy mã đăng nhập</button>
</form>
<p class="muted">Mã có hiệu lực 10 phút và chỉ dùng được một lần.</p>`,
  );
}

/// The filters a log read carries, or the reason it is not one this log can
/// answer. Mirrors the desktop's `ActivityFilter`: an unknown value is a 400,
/// never an empty page that looks like "nothing happened".
function activityFilters(url) {
  const value = (name) => {
    const raw = url.searchParams.get(name);
    return raw == null || raw === "" ? null : raw;
  };
  const filters = {
    cursor: null,
    limit: ACTIVITY_DEFAULT_PAGE,
    action: value("action"),
    actorUserId: value("actorUserId"),
    targetType: value("targetType"),
    since: value("since"),
  };

  const cursor = value("cursor");
  if (cursor != null) {
    if (cursor.length > 15 || !/^\d+$/.test(cursor)) {
      return { error: "Invalid cursor" };
    }
    filters.cursor = Number(cursor);
  }

  const limit = value("limit");
  if (limit != null) {
    if (!/^\d+$/.test(limit)) return { error: "Invalid limit" };
    filters.limit = Math.min(Math.max(Number(limit), 1), ACTIVITY_MAX_PAGE);
  }

  if (
    filters.action != null &&
    !/^[a-z][a-z0-9_]*\.([a-z][a-z0-9_]*|\*)$/.test(filters.action)
  ) {
    return { error: "Invalid action" };
  }
  if (
    filters.actorUserId != null &&
    !/^[A-Za-z0-9_-]{1,128}$/.test(filters.actorUserId)
  ) {
    return { error: "Invalid actor" };
  }
  if (filters.targetType != null && !TARGET_TYPES.has(filters.targetType)) {
    return { error: "Invalid target type" };
  }
  if (filters.since != null && Number.isNaN(Date.parse(filters.since))) {
    return { error: "Invalid since" };
  }
  return filters;
}

function codePage(code) {
  return htmlPage(
    "Mã đăng nhập Donut",
    `<h1>Mã đăng nhập của bạn</h1>
<p>Dán mã này vào cửa sổ đăng nhập của Donut:</p>
<code>${code}</code>
<p class="muted">Mã có hiệu lực 10 phút và chỉ dùng được một lần. Không gửi mã cho người khác.</p>`,
  );
}

export function createControlPlane(config) {
  const challenges = new Map();
  const codes = new Map();
  const team = createTeamStore(config);
  const agent = createAgentStore(config);
  // Locks this account holds, so a launch is recorded once rather than on
  // every retry, and a stop only when there was something to stop.
  const heldLocks = new Set();

  const prune = () => {
    const now = Date.now();
    for (const [key, value] of challenges) {
      if (value.expiresAt <= now) challenges.delete(key);
    }
    for (const [key, value] of codes) {
      if (value.expiresAt <= now) codes.delete(key);
    }
  };

  const issueCode = () => {
    const code = randomBytes(8).toString("hex").toUpperCase();
    codes.set(code, {
      expiresAt: Date.now() + CODE_TTL_MS,
      accountId: config.accountId,
    });
    return code;
  };

  const exchangeCode = (code) => {
    prune();
    const normalized = code.trim().toUpperCase();
    const found = codes.get(normalized);
    if (!found) return false;
    codes.delete(normalized);
    return true;
  };

  return createServer(async (req, res) => {
    try {
      const url = new URL(req.url || "/", "http://control-plane.local");
      const path = url.pathname;
      const method = req.method || "GET";

      if (path === "/healthz" || path === "/health") {
        return sendJson(res, 200, { status: "ok" });
      }

      if (path === "/auth/link" && method === "GET") {
        return sendHtml(res, 200, linkForm());
      }

      if (path === "/auth/link" && method === "POST") {
        const form = parseForm(await readBody(req));
        const token = form.get("token") || "";
        if (!safeEqual(token, config.controlToken)) {
          return sendHtml(res, 401, linkForm("CONTROL_TOKEN không đúng."));
        }
        return sendHtml(res, 200, codePage(issueCode()));
      }

      if (path === "/api/auth/device-code/challenge" && method === "POST") {
        prune();
        const challengeId = randomUUID();
        challenges.set(challengeId, { expiresAt: Date.now() + CHALLENGE_TTL_MS });
        return sendJson(res, 200, {
          challengeId,
          // The desktop refuses difficulty 0 outright, so ask for one leading
          // zero bit. The one-time code and HTTPS are the real gates.
          prefix: "",
          difficulty: 1,
        });
      }

      if (path === "/api/auth/device-code/exchange" && method === "POST") {
        let body;
        try {
          body = await readJson(req);
        } catch {
          return sendJson(res, 400, { message: "Invalid JSON body" });
        }
        prune();
        const challengeId = String(body.challengeId || "");
        if (!challenges.has(challengeId)) {
          return sendJson(res, 401, { message: "Challenge expired" });
        }
        challenges.delete(challengeId);
        if (!exchangeCode(String(body.code || ""))) {
          return sendJson(res, 401, {
            message: "Mã đăng nhập không đúng hoặc đã hết hạn",
          });
        }
        return sendJson(res, 200, loginPayload(config));
      }

      if (path === "/api/auth/auto" && method === "POST") {
        let body;
        try {
          body = await readJson(req);
        } catch {
          return sendJson(res, 400, { message: "Invalid JSON body" });
        }
        const expected = config.autoLoginKey || config.controlToken;
        if (!safeEqual(String(body.key || ""), expected)) {
          return sendJson(res, 401, { message: "Auto-login key refused" });
        }
        return sendJson(res, 200, loginPayload(config));
      }

      if (path === "/api/auth/token/refresh" && method === "POST") {
        let body;
        try {
          body = await readJson(req);
        } catch {
          return sendJson(res, 400, { message: "Invalid JSON body" });
        }
        const claims = verifyToken(config, String(body.refreshToken || ""));
        if (claims?.scope !== "refresh" || claims.sub !== config.accountId) {
          return sendJson(res, 401, { message: "Refresh token refused" });
        }
        return sendJson(res, 200, {
          accessToken: accessToken(config),
          refreshToken: refreshToken(config),
        });
      }

      if (path === "/api/auth/logout" && method === "POST") {
        return sendJson(res, 200, { ok: true });
      }

      const claims = authorized(config, req);
      if (!claims) {
        return sendJson(res, 401, { message: "Unauthorized" });
      }

      if (path === "/api/auth/me" && method === "GET") {
        return sendJson(res, 200, account(config));
      }

      if (path === "/api/auth/sync-token" && method === "POST") {
        let body = {};
        try {
          body = await readJson(req);
        } catch {
          return sendJson(res, 400, { message: "Invalid JSON body" });
        }
        return sendJson(res, 200, { syncToken: syncToken(config, body.server) });
      }

      if (path === "/api/auth/sync-profile-usage" && method === "POST") {
        return sendJson(res, 200, { ok: true });
      }

      // --- Agent routines -------------------------------------------------
      // The browser runs the task; the account's credits, token, routines and
      // run records live here so the Agent page has a server without Donut
      // cloud. The engine is handed this base and the token to use.
      if (path === "/api/agent/credits" && method === "GET") {
        return sendJson(res, 200, agent.credits());
      }

      if (path === "/api/agent/token" && method === "POST") {
        return sendJson(res, 200, agent.mintToken());
      }

      if (path === "/api/agent/routines" && method === "GET") {
        return sendJson(res, 200, { routines: agent.listRoutines() });
      }

      if (path === "/api/agent/routines" && method === "POST") {
        let body;
        try {
          body = await readJson(req);
        } catch {
          return sendJson(res, 400, { code: "AGENT_ROUTINE_INVALID" });
        }
        const created = agent.createRoutine(body);
        if (created.error) return sendJson(res, 400, { code: created.error });
        return sendJson(res, 200, created.routine);
      }

      const agentRoutine = path.match(/^\/api\/agent\/routines\/([^/]+)$/);
      if (agentRoutine) {
        let id;
        try {
          id = decodeURIComponent(agentRoutine[1]);
        } catch {
          return sendJson(res, 400, { code: "AGENT_ROUTINE_INVALID" });
        }
        if (method === "GET") {
          const routine = agent.routine(id);
          if (!routine) return sendJson(res, 404, { code: "AGENT_ROUTINE_NOT_FOUND" });
          return sendJson(res, 200, routine);
        }
        if (method === "PATCH") {
          let body;
          try {
            body = await readJson(req);
          } catch {
            return sendJson(res, 400, { code: "AGENT_ROUTINE_INVALID" });
          }
          const updated = agent.updateRoutine(id, body);
          if (updated.error === "AGENT_ROUTINE_NOT_FOUND") {
            return sendJson(res, 404, { code: updated.error });
          }
          if (updated.error) return sendJson(res, 400, { code: updated.error });
          return sendJson(res, 200, updated.routine);
        }
        if (method === "DELETE") {
          if (!agent.deleteRoutine(id)) {
            return sendJson(res, 404, { code: "AGENT_ROUTINE_NOT_FOUND" });
          }
          return sendJson(res, 200, { ok: true });
        }
        return sendJson(res, 405, { code: "METHOD_NOT_ALLOWED" });
      }

      const agentRoutineRuns = path.match(/^\/api\/agent\/routines\/([^/]+)\/runs$/);
      if (agentRoutineRuns) {
        let id;
        try {
          id = decodeURIComponent(agentRoutineRuns[1]);
        } catch {
          return sendJson(res, 400, { code: "AGENT_ROUTINE_INVALID" });
        }
        if (!agent.routine(id)) {
          return sendJson(res, 404, { code: "AGENT_ROUTINE_NOT_FOUND" });
        }
        if (method === "GET") {
          return sendJson(res, 200, { runs: agent.runs(id) });
        }
        if (method === "POST") {
          let body;
          try {
            body = await readJson(req);
          } catch {
            return sendJson(res, 400, { code: "AGENT_ROUTINE_INVALID" });
          }
          const profileIds = Array.isArray(body.profileIds)
            ? body.profileIds.map((value) => clip(String(value), 128)).filter(Boolean)
            : [];
          if (profileIds.length === 0) {
            return sendJson(res, 400, { code: "AGENT_ROUTINE_INVALID" });
          }
          const batchId = typeof body.batchId === "string" ? clip(body.batchId, 128) : null;
          return sendJson(res, 200, agent.startRuns(id, profileIds, batchId));
        }
        return sendJson(res, 405, { code: "METHOD_NOT_ALLOWED" });
      }

      const agentRun = path.match(/^\/api\/agent\/runs\/([^/]+)$/);
      if (agentRun && method === "PATCH") {
        let id;
        try {
          id = decodeURIComponent(agentRun[1]);
        } catch {
          return sendJson(res, 400, { code: "AGENT_RUN_NOT_FOUND" });
        }
        let body;
        try {
          body = await readJson(req);
        } catch {
          return sendJson(res, 400, { code: "AGENT_RUN_NOT_FOUND" });
        }
        const run = agent.reportRun(id, body);
        if (!run) return sendJson(res, 404, { code: "AGENT_RUN_NOT_FOUND" });
        return sendJson(res, 200, run);
      }

      // The desktop reports the edits this machine made; the control plane
      // owns the log, exactly as the cloud does for a team.
      if (path === "/api/teams/activity" && method === "POST") {
        let body;
        try {
          body = await readJson(req);
        } catch {
          return activityError(res, "Invalid JSON body");
        }
        if (!Array.isArray(body.events) || body.events.length > ACTIVITY_BATCH) {
          return activityError(res, "Invalid events");
        }
        const accepted = team.report(body.events);
        return sendJson(res, 200, { ok: true, accepted });
      }

      const accessMatch = path.match(
        /^\/api\/teams\/([^/]+)\/members\/([^/]+)\/access$/,
      );
      if (accessMatch) {
        let teamId;
        let userId;
        try {
          teamId = decodeURIComponent(accessMatch[1]);
          userId = decodeURIComponent(accessMatch[2]);
        } catch {
          return sendJson(res, 400, { code: "TEAM_ACCESS_INVALID" });
        }
        if (teamId !== (config.teamId || "local-team")) {
          return sendJson(res, 404, { code: "TEAM_ACCESS_FORBIDDEN" });
        }
        if (method !== "PUT") {
          return sendJson(res, 405, { code: "METHOD_NOT_ALLOWED" });
        }
        if (userId !== config.accountId) {
          return sendJson(res, 404, { code: "TEAM_MEMBER_NOT_FOUND" });
        }
        // The owner is the account itself; limiting the only seat could lock
        // the person out of their own profiles, so the access table keeps it
        // fixed at every group.
        return sendJson(res, 400, { code: "TEAM_ACCESS_INVALID" });
      }

      const teamMatch = path.match(
        /^\/api\/teams\/([^/]+)\/(members|activity|activity\.csv)$/,
      );
      if (teamMatch) {
        let teamId;
        try {
          teamId = decodeURIComponent(teamMatch[1]);
        } catch {
          return sendJson(res, 400, { code: "TEAM_ACTIVITY_INVALID" });
        }
        if (teamId !== (config.teamId || "local-team")) {
          return sendJson(res, 404, { code: "TEAM_ACTIVITY_FORBIDDEN" });
        }
        const resource = teamMatch[2];

        if (resource === "members" && method === "GET") {
          return sendJson(res, 200, [team.owner()]);
        }

        if (method !== "GET") {
          return sendJson(res, 405, { code: "METHOD_NOT_ALLOWED" });
        }

        const filters = activityFilters(url);
        if (filters.error) return activityError(res, filters.error);

        if (resource === "activity.csv") {
          const rows = team.all(filters);
          const header =
            "at,actor_email,actor_kind,action,target_type,target_id,target_name,detail";
          const lines = rows.map((event) =>
            [
              event.at,
              event.actor?.email,
              event.actor?.kind,
              event.action,
              event.target?.type,
              event.target?.id,
              event.target?.name,
              event.detail,
            ]
              .map(csvCell)
              .join(","),
          );
          res.writeHead(200, {
            "Content-Type": "text/csv; charset=utf-8",
            "Cache-Control": "no-store",
            "X-Content-Type-Options": "nosniff",
          });
          return res.end([header, ...lines].join("\n"));
        }

        return sendJson(res, 200, team.page(filters));
      }

      if (path === "/api/proxy/locations/countries" && method === "GET") {
        return sendJson(res, 200, []);
      }

      if (path === "/api/mcp/status" && method === "GET") {
        return sendJson(res, 200, { entitled: false, enabled: false });
      }

      // Remote sessions are not served here, but the desktop keeps an SSE
      // subscriber open for any signed-in account. An empty snapshot plus a
      // heartbeat keeps that subscriber quiet instead of 404ing and backing
      // off forever; no session is ever invented.
      if (path === "/api/remote-sessions/events" && method === "GET") {
        res.writeHead(200, {
          "Content-Type": "text/event-stream",
          "Cache-Control": "no-cache, no-transform",
          Connection: "keep-alive",
          "X-Accel-Buffering": "no",
        });
        res.write(
          `data: ${JSON.stringify({
            type: "snapshot",
            at: new Date().toISOString(),
            sessions: [],
          })}\n\n`,
        );
        const heartbeat = setInterval(() => {
          if (!res.writableEnded) res.write(": keepalive\n\n");
        }, 25_000);
        const stop = () => clearInterval(heartbeat);
        req.on("close", stop);
        res.on("close", stop);
        return;
      }

      if (path === "/api/remote-sessions" && method === "GET") {
        return sendJson(res, 200, { sessions: [] });
      }

      if (path === "/api/profile-locks" && method === "GET") {
        // One account, no team: nothing is held by anyone else. The desktop
        // still takes and renews its own lock locally.
        return sendJson(res, 200, []);
      }

      const lockMatch = path.match(/^\/api\/profile-locks\/([^/]+)(\/heartbeat)?$/);
      if (lockMatch) {
        const profileId = decodeURIComponent(lockMatch[1]);
        if (lockMatch[2] && method === "POST") {
          return sendJson(res, 200, {
            refreshed: true,
            profileId,
          });
        }
        if (!lockMatch[2] && method === "POST") {
          // A launch takes this lock; a retry of the same launch does not
          // record a second one.
          if (!heldLocks.has(profileId)) {
            heldLocks.add(profileId);
            team.note(LAUNCH_ACTION, profileId);
          }
          return sendJson(res, 200, {
            success: true,
            lockedBy: config.accountId,
            lockedByEmail: config.email,
          });
        }
        if (!lockMatch[2] && method === "DELETE") {
          if (heldLocks.delete(profileId)) team.note(STOP_ACTION, profileId);
          return sendJson(res, 200, { success: true });
        }
      }

      if (path === "/api/feedback" && method === "POST") {
        return sendJson(res, 200, { ok: true });
      }

      // A path this server does not serve is worth one line: a desktop
      // service that assumes a cloud route shows up here instead of as an
      // unexplained failure in the app. The path is not a secret; bodies are
      // never logged.
      console.log(`control-plane unknown route ${method} ${path}`);
      return sendJson(res, 404, { code: "NOT_FOUND" });
    } catch (error) {
      // Never echo the request body: it can carry a token.
      console.error(`control-plane request failed: ${error.message}`);
      if (!res.headersSent) {
        sendJson(res, 500, { code: "INTERNAL_ERROR" });
      } else {
        res.end();
      }
    }
  });
}

export function start(config = loadConfig()) {
  const server = createControlPlane(config);
  server.listen(config.port, () => {
    console.log(
      `control-plane listening port=${config.port} account=${config.accountId}`,
    );
  });
  return server;
}

if (import.meta.url === pathToFileURL(process.argv[1] || "").href) {
  start();
}
