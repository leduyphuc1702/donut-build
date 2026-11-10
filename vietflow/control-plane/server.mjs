// Self-hosted account/control plane for the Vietflow build of Donut.
//
// It answers the same small set of routes the desktop app asks of Donut cloud:
// device-code sign-in, the account object (entitlements), token refresh, the
// sync token, the profile locks a launch takes, and the team features: the
// env account is the team's owner, the owner adds staff on /admin (members
// kept in DATA_DIR/members.json), and the activity log the desktop reports
// into names who did what from which computer (DATA_DIR/team.json).
// Everything else (cloud automation, remote MCP) is
// deliberately not implemented: the app's entitlement object says so, and an
// unknown route answers 404 instead of pretending to work.
//
// No dependencies. Node 22+.

import {
  createHash,
  createHmac,
  randomBytes,
  randomUUID,
  timingSafeEqual,
} from "node:crypto";
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
/// A code the owner mints on /auth/link for their own sign-in.
const CODE_TTL_MS = 10 * 60 * 1000;
/// A code the owner hands to a staff member: it has to survive the message
/// that carries it, so it lives a week.
const MEMBER_CODE_TTL_MS = 7 * 24 * 60 * 60 * 1000;
const MAX_CODES = 2_000;
const CHALLENGE_TTL_MS = 5 * 60 * 1000;
const MAX_BODY_BYTES = 64 * 1024;
const MAX_MEMBERS = 500;
const MAX_DEVICES = 20;
const MAX_GROUP_ACCESS = 500;
/// The roles the owner can give. "owner" is the env account and nobody else.
const MEMBER_ROLES = new Set(["admin", "member", "launcher"]);
/// A profile lock is the holder's promise that the browser is open; it lapses
/// unless the desktop renews it (every 30 s) so a crashed computer frees it.
const LOCK_TTL_MS = 90 * 1000;
const MAX_LOCKS = 10_000;
const ADMIN_COOKIE = "donut_admin";
const ADMIN_SESSION_TTL_MS = 30 * 60 * 1000;
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

/// Actions only this server writes (membership). A desktop that reports one
/// would let a member put words in the owner's mouth, so a report is refused.
const SERVER_ONLY_PREFIXES = ["member.", "invitation."];

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
    // donut-sync's shared `SYNC_TOKEN`. Set together with SYNC_SERVER_URL, it
    // is handed to every signed-in device, so signing in is all a device
    // needs to sync. It opens the whole bucket, like typing it in did.
    syncSharedToken: envString(env, "SYNC_SHARED_TOKEN"),
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

/// The account object one signed-in person gets. The plan and entitlements
/// are the owner's, shared by every member; the id, email, role and device
/// count are the person's own.
function account(config, user) {
  const limit = 1_000_000;
  return {
    id: user.userId,
    email: user.email,
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
    teamRole: user.role,
    effectivePlan: config.plan,
    // Which computer is asking is not known here; every one counts as the
    // primary so no one is refused browser automation for being "second".
    deviceOrdinal: 1,
    deviceCount: Math.max(1, user.devices.length),
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

function activityError(res, message, code = "TEAM_ACTIVITY_INVALID") {
  return sendJson(res, 400, { message, code });
}

function clip(value, max) {
  return typeof value === "string" ? value.slice(0, max) : "";
}

/// Control characters, line breaks and the invisible direction marks that
/// make one string read as another. A name from a desktop reaches the owner's
/// admin page and spreadsheet, so none of them is kept.
const INVISIBLE = /[\u0000-\u001f\u007f-\u009f\u200b-\u200f\u2028\u2029\u202a-\u202e\u2066-\u2069\ufeff]/g;

/// The computer a report came from, or null when it names none this log can
/// hold: an id of 1-128 of [A-Za-z0-9_-] and a name of at most 64 characters.
function checkedDevice(raw) {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const id = typeof raw.id === "string" ? raw.id : "";
  if (!/^[A-Za-z0-9_-]{1,128}$/.test(id)) return null;
  const name =
    typeof raw.name === "string"
      ? Array.from(raw.name.replace(INVISIBLE, "").trim()).slice(0, 64).join("").trim()
      : "";
  return { id, name: name || id.slice(0, 8) };
}

/// One reported event, normalized to the shape the desktop reads back, by the
/// person who sent it. Anything this build cannot name is refused rather than
/// stored blind: a viewer would show an untranslated line for it forever.
function checkedEvent(user, raw, at, batchDevice) {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const action = clip(raw.action, 64);
  if (!/^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$/.test(action)) return null;
  if (SERVER_ONLY_PREFIXES.some((prefix) => action.startsWith(prefix))) return null;
  const targetType = clip(raw.targetType, 64) || action.split(".")[0];
  if (!TARGET_TYPES.has(targetType)) return null;
  const targetId = raw.targetId == null ? null : clip(String(raw.targetId), 128);
  const targetName = raw.targetName == null ? null : clip(String(raw.targetName), 200);
  const detail =
    raw.detail && typeof raw.detail === "object" && !Array.isArray(raw.detail)
      ? raw.detail
      : null;
  const device = checkedDevice(raw.device) ?? batchDevice;
  return {
    at,
    actor: {
      userId: user.userId,
      email: user.email,
      kind: "desktop",
      deviceId: device?.id ?? null,
      deviceName: device?.name ?? null,
    },
    action,
    target: { type: targetType, id: targetId || null, name: targetName || null },
    detail,
    source: "client",
  };
}

/// What a cut-down half of a page of JSON looks like in a spreadsheet cell.
/// A cell a spreadsheet would read as a formula gets a leading apostrophe: a
/// profile or computer name is typed by whoever owns it, not by the reader.
function csvCell(value) {
  let text = value == null ? "" : typeof value === "string" ? value : JSON.stringify(value);
  if (/^[=+\-@\t\r]/.test(text)) text = `'${text}`;
  return /[",\n\r]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
}

function stamp(date = new Date()) {
  return date.toISOString();
}

function validEmail(email) {
  return email.length <= 254 && /^[^\s@<>"',;]+@[^\s@<>"',;]+\.[^\s@<>"',;]+$/.test(email);
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
    /// Add the events `user` reported, returning how many were accepted and
    /// the computers they came from. `batchDevice` names the sender when an
    /// event does not name its own.
    report: (user, events, batchDevice = null) => {
      const at = stamp();
      const accepted = [];
      for (const raw of events.slice(0, ACTIVITY_BATCH)) {
        const event = checkedEvent(user, raw, at, batchDevice);
        if (event) accepted.push(event);
      }
      if (accepted.length === 0) return { accepted: 0, devices: [] };
      const devices = new Map();
      for (const event of accepted) {
        state.events.push({ id: state.nextId++, ...event });
        if (event.actor.deviceId) {
          devices.set(event.actor.deviceId, {
            id: event.actor.deviceId,
            name: event.actor.deviceName,
          });
        }
      }
      const overflow = state.events.length - MAX_ACTIVITY_EVENTS;
      if (overflow > 0) state.events.splice(0, overflow);
      persist();
      return { accepted: accepted.length, devices: [...devices.values()] };
    },
    /// Record something the control plane did or saw itself: a membership
    /// change by the owner on /admin, or by an admin from the desktop. The
    /// actor is the person who did it; with none given it is the server.
    note: (action, { actor = null, targetId = null, targetName = null, detail = null }) => {
      const targetType = action.split(".")[0];
      if (!TARGET_TYPES.has(targetType)) return;
      state.events.push({
        id: state.nextId++,
        at: stamp(),
        actor: actor
          ? {
              userId: actor.userId,
              email: actor.email,
              kind: actor.kind ?? "desktop",
              deviceId: null,
              deviceName: null,
            }
          : { userId: config.accountId, email: config.email, kind: "system" },
        action,
        target: { type: targetType, id: targetId, name: targetName },
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

const USER_ID = /^[A-Za-z0-9_-]{1,128}$/;

function codeKey(code) {
  return createHash("sha256").update(String(code).trim().toUpperCase()).digest("hex");
}

/// The people on the team and the sign-in codes that let them in, in one
/// file (DATA_DIR/members.json, written atomically).
///
/// The env account is the owner and is never listed in the file: only its
/// token version, last-seen time and computers are kept, so the owner can be
/// signed out everywhere like anyone else. A staff member has a random
/// userId, a role, the groups they may reach, and a token version that a JWT
/// must match: remove or "sign out everywhere" bumps it and every token the
/// person holds stops working at once. Codes are kept as SHA-256 so the file
/// does not hold a usable sign-in. A file that cannot be read is moved aside,
/// never overwritten: starting over would lock the whole team out.
function createMemberStore(config) {
  const dataDir = config.dataDir || "data";
  const file = join(dataDir, "members.json");
  const ownerId = config.accountId;
  const codes = new Map();
  let state;
  let failing = false;
  let lastPersistAt = 0;

  const freshOwner = () => ({ tokenVersion: 0, lastSeenAt: null, devices: [] });

  const cleanDevices = (value) => {
    if (!Array.isArray(value)) return [];
    const devices = [];
    for (const raw of value) {
      const device = checkedDevice(raw);
      if (device && !devices.some((known) => known.id === device.id)) {
        devices.push({
          ...device,
          lastSeenAt: typeof raw.lastSeenAt === "string" ? raw.lastSeenAt : null,
        });
      }
    }
    return devices.slice(0, MAX_DEVICES);
  };

  const cleanAccess = (value) => {
    if (value == null) return null;
    if (!Array.isArray(value)) return [];
    return value.filter((id) => typeof id === "string" && USER_ID.test(id)).slice(0, MAX_GROUP_ACCESS);
  };

  const cleanMember = (raw) => {
    if (!raw || typeof raw !== "object") return null;
    if (typeof raw.userId !== "string" || !USER_ID.test(raw.userId)) return null;
    const email = typeof raw.email === "string" ? raw.email.trim().toLowerCase() : "";
    if (!validEmail(email) || !MEMBER_ROLES.has(raw.role)) return null;
    return {
      userId: raw.userId,
      email,
      role: raw.role,
      groupAccess: cleanAccess(raw.groupAccess),
      createdAt: typeof raw.createdAt === "string" ? raw.createdAt : stamp(),
      joinedAt: typeof raw.joinedAt === "string" ? raw.joinedAt : null,
      tokenVersion: Number.isSafeInteger(raw.tokenVersion) && raw.tokenVersion >= 0 ? raw.tokenVersion : 0,
      lastSeenAt: typeof raw.lastSeenAt === "string" ? raw.lastSeenAt : null,
      devices: cleanDevices(raw.devices),
    };
  };

  try {
    mkdirSync(dataDir, { recursive: true });
    if (existsSync(file)) {
      const stored = JSON.parse(readFileSync(file, "utf8"));
      const seen = new Set();
      state = {
        version: 1,
        createdAt: typeof stored.createdAt === "string" ? stored.createdAt : stamp(),
        owner: {
          tokenVersion:
            Number.isSafeInteger(stored.owner?.tokenVersion) && stored.owner.tokenVersion >= 0
              ? stored.owner.tokenVersion
              : 0,
          lastSeenAt: typeof stored.owner?.lastSeenAt === "string" ? stored.owner.lastSeenAt : null,
          devices: cleanDevices(stored.owner?.devices),
        },
        members: (Array.isArray(stored.members) ? stored.members : [])
          .map(cleanMember)
          .filter((member) => {
            if (!member || member.userId === ownerId || seen.has(member.userId)) return false;
            seen.add(member.userId);
            return true;
          }),
      };
      for (const entry of Array.isArray(stored.codes) ? stored.codes : []) {
        if (
          entry &&
          typeof entry.hash === "string" &&
          typeof entry.userId === "string" &&
          Number.isFinite(entry.expiresAt) &&
          entry.expiresAt > Date.now()
        ) {
          codes.set(entry.hash, { userId: entry.userId, expiresAt: entry.expiresAt });
        }
      }
    }
  } catch (error) {
    console.error(`control-plane members unreadable, moved aside: ${error.message}`);
    try {
      renameSync(file, `${file}.corrupt-${Date.now()}`);
    } catch {
      // The new file below replaces it; the error above says why.
    }
    state = undefined;
    codes.clear();
  }

  if (!state) {
    state = { version: 1, createdAt: stamp(), owner: freshOwner(), members: [] };
  }

  const persist = () => {
    try {
      const tmp = `${file}.${process.pid}.tmp`;
      writeFileSync(
        tmp,
        JSON.stringify({
          ...state,
          codes: [...codes].map(([hash, entry]) => ({ hash, ...entry })),
        }),
        { mode: 0o600 },
      );
      renameSync(tmp, file);
      failing = false;
      lastPersistAt = Date.now();
    } catch (error) {
      // Once per outage: a full disk would otherwise log per request.
      if (!failing) console.error(`control-plane members not writable: ${error.message}`);
      failing = true;
    }
  };

  persist();

  const ownerUser = () => ({
    userId: ownerId,
    email: config.email,
    role: "owner",
    groupAccess: null,
    createdAt: state.createdAt,
    joinedAt: state.createdAt,
    tokenVersion: state.owner.tokenVersion,
    lastSeenAt: state.owner.lastSeenAt,
    devices: state.owner.devices,
  });

  const memberOf = (userId) => state.members.find((member) => member.userId === userId) ?? null;
  const recordOf = (userId) => (userId === ownerId ? state.owner : memberOf(userId));

  const pruneCodes = () => {
    const now = Date.now();
    for (const [hash, entry] of codes) {
      if (entry.expiresAt <= now) codes.delete(hash);
    }
    while (codes.size > MAX_CODES) codes.delete(codes.keys().next().value);
  };

  return {
    /// False while the file cannot be written: changes then die with the process.
    durable: () => !failing,
    owner: ownerUser,
    get: (userId) => (userId === ownerId ? ownerUser() : memberOf(userId)),
    /// Everyone, the owner first, then staff in the order they were added.
    list: () => [ownerUser(), ...state.members],
    add: (emailRaw, role) => {
      const email = String(emailRaw ?? "").trim().toLowerCase();
      if (!validEmail(email)) return { error: "EMAIL_INVALID" };
      if (!MEMBER_ROLES.has(role)) return { error: "ROLE_INVALID" };
      if (email === config.email.toLowerCase() || state.members.some((member) => member.email === email)) {
        return { error: "EMAIL_TAKEN" };
      }
      if (state.members.length >= MAX_MEMBERS) return { error: "TOO_MANY_MEMBERS" };
      const member = {
        userId: `u-${randomBytes(12).toString("base64url")}`,
        email,
        role,
        groupAccess: null,
        createdAt: stamp(),
        joinedAt: null,
        tokenVersion: 0,
        lastSeenAt: null,
        devices: [],
      };
      state.members.push(member);
      persist();
      return { member };
    },
    setRole: (userId, role) => {
      const member = memberOf(userId);
      if (!member) return { error: "NOT_FOUND" };
      if (!MEMBER_ROLES.has(role)) return { error: "ROLE_INVALID" };
      const from = member.role;
      member.role = role;
      if (from !== role) persist();
      return { member, from };
    },
    /// `groups` is null for every group, else a list of group ids.
    setAccess: (userId, groups) => {
      const member = memberOf(userId);
      if (!member) return { error: "NOT_FOUND" };
      const next = groups === null ? null : [...new Set(groups)];
      const before = JSON.stringify(member.groupAccess);
      member.groupAccess = next;
      const changed = before !== JSON.stringify(next);
      if (changed) persist();
      return { member, changed };
    },
    /// Delete a member. The token version goes up first, so a copy of this
    /// record that outlives the call (a request in flight) already fails.
    remove: (userId) => {
      const index = state.members.findIndex((member) => member.userId === userId);
      if (index < 0) return null;
      const [member] = state.members.splice(index, 1);
      member.tokenVersion += 1;
      for (const [hash, entry] of codes) {
        if (entry.userId === userId) codes.delete(hash);
      }
      persist();
      return member;
    },
    /// Sign a person out of every computer: their tokens carry the old version.
    revoke: (userId) => {
      const record = recordOf(userId);
      if (!record) return false;
      record.tokenVersion += 1;
      persist();
      return true;
    },
    /// Note that `userId` was seen now, on `devices`. The file is written when
    /// a computer is new or renamed, else at most once a minute.
    touch: (userId, devices = []) => {
      const record = recordOf(userId);
      if (!record) return;
      const now = stamp();
      let structural = false;
      for (const device of devices) {
        const known = record.devices.find((item) => item.id === device.id);
        if (known) {
          if (known.name !== device.name) {
            known.name = device.name;
            structural = true;
          }
          known.lastSeenAt = now;
        } else {
          record.devices.push({ id: device.id, name: device.name, lastSeenAt: now });
          structural = true;
        }
      }
      if (record.devices.length > MAX_DEVICES) {
        record.devices.sort((a, b) => String(b.lastSeenAt).localeCompare(String(a.lastSeenAt)));
        record.devices.length = MAX_DEVICES;
      }
      record.lastSeenAt = now;
      if (structural || Date.now() - lastPersistAt > 60_000) persist();
    },
    /// True the first time a person signs in, which is when they join.
    markJoined: (userId) => {
      const member = memberOf(userId);
      if (!member || member.joinedAt) return false;
      member.joinedAt = stamp();
      persist();
      return true;
    },
    /// A one-time code for `userId`. `replace` voids the person's earlier codes.
    issueCode: (userId, ttlMs, replace = false) => {
      pruneCodes();
      if (replace) {
        for (const [hash, entry] of codes) {
          if (entry.userId === userId) codes.delete(hash);
        }
      }
      const code = randomBytes(8).toString("hex").toUpperCase();
      codes.set(codeKey(code), { userId, expiresAt: Date.now() + ttlMs });
      persist();
      return code;
    },
    /// The person a code signs in, once: the code is gone after this call.
    takeCode: (code) => {
      pruneCodes();
      const key = codeKey(code);
      const entry = codes.get(key);
      if (!entry) return null;
      codes.delete(key);
      persist();
      return entry.userId === ownerId || memberOf(entry.userId) ? entry.userId : null;
    },
    hasCode: (userId) => {
      pruneCodes();
      return [...codes.values()].some((entry) => entry.userId === userId);
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

// Every token names its person (`sub`) and the version of their sign-in
// (`tv`). A token with no `tv` was minted before members existed and counts
// as version 0, so the owner's existing sessions keep working.
function accessToken(config, user) {
  const now = Math.floor(Date.now() / 1000);
  return signToken(config, {
    sub: user.userId,
    email: user.email,
    scope: "account",
    tv: user.tokenVersion,
    jti: randomUUID(),
    exp: now + ACCESS_TTL_SECS,
  });
}

// The body both sign-in routes return: the person's own tokens and account,
// whether they came from a one-time code or the build's provisioning key.
function loginPayload(config, user) {
  return {
    accessToken: accessToken(config, user),
    refreshToken: refreshToken(config, user),
    user: account(config, user),
  };
}

function refreshToken(config, user) {
  const now = Math.floor(Date.now() / 1000);
  return signToken(config, {
    sub: user.userId,
    scope: "refresh",
    tv: user.tokenVersion,
    jti: randomUUID(),
    exp: now + REFRESH_TTL_SECS,
  });
}

function syncToken(config, user, server) {
  const now = Math.floor(Date.now() / 1000);
  const audience = server || config.syncServerUrl || "self-hosted";
  return signToken(config, {
    sub: user.userId,
    scope: "server-sync",
    aud: audience,
    exp: now + ACCESS_TTL_SECS,
  });
}

/// Whether a token's version is the one its person is on now.
function currentVersion(claims, user) {
  const version = Number.isSafeInteger(claims.tv) ? claims.tv : 0;
  return version === user.tokenVersion;
}

function bearer(req) {
  const header = req.headers.authorization || "";
  return header.startsWith("Bearer ") ? header.slice("Bearer ".length) : "";
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

function sendHtml(res, status, html, headers = {}) {
  res.writeHead(status, {
    "Content-Type": "text/html; charset=utf-8",
    "Cache-Control": "no-store",
    "X-Content-Type-Options": "nosniff",
    "X-Frame-Options": "DENY",
    "Referrer-Policy": "no-referrer",
    "Content-Security-Policy":
      "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'",
    ...headers,
  });
  res.end(html);
}

function redirect(res, location, headers = {}) {
  res.writeHead(303, { Location: location, "Cache-Control": "no-store", ...headers });
  res.end();
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

function htmlPage(title, body, wide = false) {
  return `<!doctype html>
<html lang="vi">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${title}</title>
<style>
:root { color-scheme: light dark; }
body { font: 16px/1.5 system-ui, sans-serif; max-width: ${wide ? "64rem" : "40rem"}; margin: 3rem auto; padding: 0 1.25rem; }
h1 { font-size: 1.35rem; margin-bottom: .5rem; }
h2 { font-size: 1.1rem; margin: 2rem 0 .5rem; }
p { margin: .5rem 0 1rem; }
label { display: block; font-size: .85rem; margin-bottom: .35rem; }
input, select { width: 100%; box-sizing: border-box; padding: .7rem; border: 1px solid #8888; border-radius: .5rem; font: inherit; }
button { margin-top: 1rem; padding: .7rem 1.1rem; border: 0; border-radius: .5rem; font: inherit; cursor: pointer; background: #2563eb; color: white; }
button.quiet { margin-top: 0; padding: .35rem .7rem; font-size: .85rem; background: #8884; color: inherit; }
button.danger { margin-top: 0; padding: .35rem .7rem; font-size: .85rem; background: #b91c1c; }
code { display: block; margin: 1rem 0; padding: 1rem; font-size: 1.6rem; letter-spacing: .2em; text-align: center; border: 1px solid #8888; border-radius: .5rem; }
table { width: 100%; border-collapse: collapse; font-size: .9rem; }
th, td { text-align: left; vertical-align: top; padding: .55rem .5rem; border-bottom: 1px solid #8884; }
td form { display: inline-block; margin: 0 .25rem .25rem 0; }
td select { width: auto; padding: .3rem; }
td label { display: inline; font-size: .8rem; }
td input[type=checkbox] { width: auto; }
.row { display: grid; grid-template-columns: 1fr 12rem auto; gap: .75rem; align-items: end; }
.row button { margin-top: 0; }
.notice { padding: .75rem 1rem; border: 1px solid #8888; border-radius: .5rem; }
.error { color: #b91c1c; }
.muted { color: #666; font-size: .85rem; }
</style>
</head>
<body>${body}</body>
</html>`;
}

function esc(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
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
<p class="muted">Mã có hiệu lực 10 phút và chỉ dùng được một lần. Trang này dành cho chủ sở hữu. Nhân viên dùng mã do chủ sở hữu cấp trong <a href="/admin">trang quản lý</a> và dán thẳng vào Donut.</p>`,
  );
}

const ROLE_LABELS = {
  owner: "Chủ sở hữu",
  admin: "Quản trị viên",
  member: "Thành viên",
  launcher: "Người khởi chạy",
};

function adminLoginForm(error = "") {
  return htmlPage(
    "Quản lý thành viên Donut",
    `<h1>Quản lý thành viên</h1>
<p>Nhập <strong>CONTROL_TOKEN</strong> để thêm nhân viên và xem ai đang dùng Donut.</p>
${error ? `<p class="error">${esc(error)}</p>` : ""}
<form method="post" action="/admin/login">
<label for="token">CONTROL_TOKEN</label>
<input id="token" name="token" type="password" autocomplete="current-password" required autofocus>
<button type="submit">Đăng nhập</button>
</form>
<p class="muted">Phiên quản lý tự hết hạn sau 30 phút không dùng.</p>`,
  );
}

function formatSeen(value) {
  const time = value ? Date.parse(value) : NaN;
  return Number.isNaN(time)
    ? "Chưa có"
    : `${new Date(time).toISOString().replace("T", " ").slice(0, 16)} UTC`;
}

/// The owner's page: everyone on the team, what each person can be asked to
/// do, and (once, right after it is made) the code a new person signs in with.
function adminDashboard(config, members, session, { error = "", flash = null } = {}) {
  const csrf = `<input type="hidden" name="csrf" value="${esc(session.csrf)}">`;
  const rows = members
    .list()
    .map((user) => {
      if (user.role === "owner") {
        return `<tr>
<td>${esc(user.email)}</td>
<td>${esc(ROLE_LABELS.owner)}</td>
<td>${esc(formatSeen(user.lastSeenAt))}</td>
<td>${deviceList(user)}</td>
<td class="muted">Tài khoản chủ của máy chủ này.</td>
</tr>`;
      }
      const id = esc(user.userId);
      const roleOptions = ["admin", "member", "launcher"]
        .map(
          (role) =>
            `<option value="${role}"${role === user.role ? " selected" : ""}>${esc(ROLE_LABELS[role])}</option>`,
        )
        .join("");
      const waiting = user.joinedAt ? "" : `<br><span class="muted">${members.hasCode(user.userId) ? "Chờ đăng nhập lần đầu" : "Chưa có mã còn hạn"}</span>`;
      return `<tr>
<td>${esc(user.email)}${waiting}</td>
<td><form method="post" action="/admin/members/role">${csrf}<input type="hidden" name="userId" value="${id}"><select name="role" aria-label="Vai trò">${roleOptions}</select> <button class="quiet" type="submit">Đổi</button></form></td>
<td>${esc(formatSeen(user.lastSeenAt))}</td>
<td>${deviceList(user)}</td>
<td>
<form method="post" action="/admin/members/code">${csrf}<input type="hidden" name="userId" value="${id}"><button class="quiet" type="submit">Cấp lại mã</button></form>
<form method="post" action="/admin/members/revoke">${csrf}<input type="hidden" name="userId" value="${id}"><button class="quiet" type="submit">Đăng xuất các máy</button></form>
<form method="post" action="/admin/members/remove">${csrf}<input type="hidden" name="userId" value="${id}"><label><input type="checkbox" name="confirm" value="1" required> Xác nhận</label> <button class="danger" type="submit">Xóa</button></form>
</td>
</tr>`;
    })
    .join("\n");

  return htmlPage(
    "Quản lý thành viên Donut",
    `<h1>Quản lý thành viên</h1>
<p>${esc(config.teamName || "Vietflow")}: ${members.list().length} người.</p>
${members.durable() ? "" : `<p class="error">Không ghi được members.json. Thay đổi sẽ mất khi máy chủ khởi động lại; kiểm tra ổ đĩa của DATA_DIR.</p>`}
${error ? `<p class="error">${esc(error)}</p>` : ""}
${
  flash
    ? `<div class="notice"><p>Mã đăng nhập cho <strong>${esc(flash.email)}</strong>:</p>
<code>${esc(flash.code)}</code>
<p class="muted">Có hiệu lực 7 ngày, dùng được một lần, và chỉ hiện ở đây một lần. Gửi mã cho đúng người đó: họ mở Donut, vào Tài khoản, bấm Đăng nhập rồi dán mã vào ô nhập.</p></div>`
    : ""
}
<h2>Thêm nhân viên</h2>
<form method="post" action="/admin/members/add">
${csrf}
<div class="row">
<div><label for="email">Email</label><input id="email" name="email" type="email" maxlength="254" required></div>
<div><label for="role">Vai trò</label><select id="role" name="role"><option value="member">${esc(ROLE_LABELS.member)}</option><option value="launcher">${esc(ROLE_LABELS.launcher)}</option><option value="admin">${esc(ROLE_LABELS.admin)}</option></select></div>
<button type="submit">Thêm và tạo mã</button>
</div>
</form>
<p class="muted">Quản trị viên xem được nhật ký và phân quyền nhóm cho người khác. Thành viên làm việc với hồ sơ. Người khởi chạy chỉ mở hồ sơ, không sửa cài đặt của nhóm.</p>
<h2>Thành viên</h2>
<table>
<thead><tr><th>Email</th><th>Vai trò</th><th>Lần cuối</th><th>Máy</th><th></th></tr></thead>
<tbody>
${rows}
</tbody>
</table>
<form method="post" action="/admin/logout">${csrf}<button class="quiet" type="submit">Thoát</button></form>`,
    true,
  );
}

function deviceList(user) {
  if (user.devices.length === 0) return `<span class="muted">Chưa có</span>`;
  return user.devices
    .map((device) => `${esc(device.name)} <span class="muted">${esc(formatSeen(device.lastSeenAt))}</span>`)
    .join("<br>");
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

/// A profile id as the desktop names it (a UUID); also what a lock is keyed by.
const PROFILE_ID = /^[A-Za-z0-9_-]{1,128}$/;

const ADMIN_ERRORS = {
  EMAIL_INVALID: "Email không hợp lệ.",
  ROLE_INVALID: "Vai trò không hợp lệ.",
  EMAIL_TAKEN: "Email này đã có trong nhóm.",
  TOO_MANY_MEMBERS: "Nhóm đã đủ số người tối đa.",
  NOT_FOUND: "Không tìm thấy thành viên.",
  CONFIRM: "Tick ô Xác nhận trước khi xóa.",
};

function parseCookies(req) {
  const out = {};
  for (const part of (req.headers.cookie || "").split(";")) {
    const index = part.indexOf("=");
    if (index > 0) out[part.slice(0, index).trim()] = part.slice(index + 1).trim();
  }
  return out;
}

/// The member row the desktop's access table reads.
function memberRow(user) {
  return {
    userId: user.userId,
    email: user.email,
    role: user.role,
    groupAccess: user.groupAccess,
  };
}

/// `{ groups }` for an access body's `groups` (null is every group), or
/// `{ error: true }` for anything that is not a list of group ids.
function checkedGroups(value) {
  if (value === null) return { groups: null };
  if (!Array.isArray(value) || value.length > MAX_GROUP_ACCESS) return { error: true };
  const groups = [];
  for (const raw of value) {
    const id = typeof raw === "string" ? raw.trim() : "";
    if (!USER_ID.test(id)) return { error: true };
    if (!groups.includes(id)) groups.push(id);
  }
  return { groups };
}

const canSeeTeam = (user) => user.role === "owner" || user.role === "admin";

export function createControlPlane(config) {
  const challenges = new Map();
  const adminSessions = new Map();
  const locks = new Map();
  const lockTtl =
    Number.isFinite(config.lockTtlMs) && config.lockTtlMs > 0 ? config.lockTtlMs : LOCK_TTL_MS;
  const members = createMemberStore(config);
  const team = createTeamStore(config);
  const agent = createAgentStore(config);

  const prune = () => {
    const now = Date.now();
    for (const [key, value] of challenges) {
      if (value.expiresAt <= now) challenges.delete(key);
    }
    for (const [key, value] of adminSessions) {
      if (value.expiresAt <= now) adminSessions.delete(key);
    }
  };

  const sweepLocks = () => {
    const now = Date.now();
    for (const [profileId, lock] of locks) {
      if (lock.expiresAt <= now) locks.delete(profileId);
    }
  };

  const lockInfo = (profileId, lock) => ({
    profileId,
    lockedBy: lock.userId,
    lockedByEmail: lock.email,
    lockedAt: new Date(lock.lockedAt).toISOString(),
    expiresAt: new Date(lock.expiresAt).toISOString(),
  });

  const dropLocksOf = (userId) => {
    for (const [profileId, lock] of locks) {
      if (lock.userId === userId) locks.delete(profileId);
    }
  };

  const actorOf = (user, kind = "desktop") => ({
    userId: user.userId,
    email: user.email,
    kind,
  });

  /// The person a request speaks for, or null. The raw CONTROL_TOKEN is the
  /// owner; a JWT is its person only while that person is still on the team
  /// and has not been signed out since the token was made.
  const authenticate = (req) => {
    const token = bearer(req);
    if (!token) return null;
    if (safeEqual(token, config.controlToken)) return members.owner();
    const claims = verifyToken(config, token);
    if (claims?.scope !== "account" || typeof claims.sub !== "string") return null;
    const user = members.get(claims.sub);
    return user && currentVersion(claims, user) ? user : null;
  };

  /// Sign `userId` in. A staff member's first sign-in is when they join.
  const signIn = (userId) => {
    const user = members.get(userId);
    if (!user) return null;
    if (members.markJoined(userId)) {
      team.note("member.join", {
        actor: actorOf(user),
        targetId: user.userId,
        targetName: user.email,
        detail: { role: user.role },
      });
    }
    members.touch(user.userId);
    return user;
  };

  // --- /admin: the owner's page ---------------------------------------------
  // Protected by CONTROL_TOKEN, entered once into a form; the page then runs on
  // a short-lived cookie the script on no other site can read (HttpOnly) or
  // send (SameSite=Strict). Every change is a POST that must also carry the
  // session's own token and, when the browser names one, come from this origin.

  const adminSession = (req) => {
    const sid = parseCookies(req)[ADMIN_COOKIE];
    const session = sid ? adminSessions.get(sid) : null;
    if (!session || session.expiresAt <= Date.now()) {
      if (sid) adminSessions.delete(sid);
      return null;
    }
    session.expiresAt = Date.now() + ADMIN_SESSION_TTL_MS;
    return session;
  };

  const sameOrigin = (req) => {
    const origin = req.headers.origin;
    if (origin === undefined) return true;
    try {
      return new URL(origin).host === req.headers.host;
    } catch {
      return false;
    }
  };

  const cookieFlags = (req) =>
    `HttpOnly; SameSite=Strict; Path=/admin${req.headers["x-forwarded-proto"] === "https" ? "; Secure" : ""}`;

  /// Handle `/admin...`. Returns false for a path that is not the admin page.
  const handleAdmin = async (req, res, path, method) => {
    if (path !== "/admin" && !path.startsWith("/admin/")) return false;

    if (path === "/admin" && method === "GET") {
      const session = adminSession(req);
      sendHtml(
        res,
        200,
        session ? adminDashboard(config, members, session) : adminLoginForm(),
      );
      return true;
    }
    if (method !== "POST") {
      sendJson(res, 405, { code: "METHOD_NOT_ALLOWED" });
      return true;
    }

    const form = parseForm(await readBody(req));

    if (path === "/admin/login") {
      if (!sameOrigin(req)) {
        sendHtml(res, 403, adminLoginForm("Yêu cầu bị từ chối."));
        return true;
      }
      if (!safeEqual(form.get("token") || "", config.controlToken)) {
        sendHtml(res, 401, adminLoginForm("CONTROL_TOKEN không đúng."));
        return true;
      }
      prune();
      while (adminSessions.size >= 20) adminSessions.delete(adminSessions.keys().next().value);
      const sid = randomBytes(32).toString("base64url");
      adminSessions.set(sid, {
        sid,
        csrf: randomBytes(24).toString("base64url"),
        expiresAt: Date.now() + ADMIN_SESSION_TTL_MS,
      });
      redirect(res, "/admin", {
        "Set-Cookie": `${ADMIN_COOKIE}=${sid}; Max-Age=${ADMIN_SESSION_TTL_MS / 1000}; ${cookieFlags(req)}`,
      });
      return true;
    }

    const session = adminSession(req);
    if (!session) {
      sendHtml(res, 401, adminLoginForm("Phiên đã hết hạn. Đăng nhập lại."));
      return true;
    }
    if (!sameOrigin(req) || !safeEqual(form.get("csrf") || "", session.csrf)) {
      sendHtml(
        res,
        403,
        htmlPage(
          "Bị từ chối",
          `<h1>Yêu cầu bị từ chối</h1><p>Biểu mẫu không đến từ trang quản lý này. <a href="/admin">Quay lại</a>.</p>`,
        ),
      );
      return true;
    }

    const owner = members.owner();
    const actor = actorOf(owner, "web");
    const show = (status, options) =>
      sendHtml(res, status, adminDashboard(config, members, session, options));
    const staffId = form.get("userId") || "";
    const staff = staffId === owner.userId ? null : members.get(staffId);

    switch (path) {
      case "/admin/logout":
        adminSessions.delete(session.sid);
        redirect(res, "/admin", {
          "Set-Cookie": `${ADMIN_COOKIE}=; Max-Age=0; ${cookieFlags(req)}`,
        });
        return true;

      case "/admin/members/add": {
        const added = members.add(form.get("email"), form.get("role"));
        if (added.error) {
          show(added.error === "EMAIL_TAKEN" ? 409 : 400, { error: ADMIN_ERRORS[added.error] });
          return true;
        }
        const { member } = added;
        const code = members.issueCode(member.userId, MEMBER_CODE_TTL_MS, true);
        team.note("member.invite", {
          actor,
          targetId: member.userId,
          targetName: member.email,
          detail: { role: member.role },
        });
        show(200, { flash: { email: member.email, code } });
        return true;
      }

      case "/admin/members/code": {
        if (!staff) {
          show(404, { error: ADMIN_ERRORS.NOT_FOUND });
          return true;
        }
        show(200, {
          flash: { email: staff.email, code: members.issueCode(staff.userId, MEMBER_CODE_TTL_MS, true) },
        });
        return true;
      }

      case "/admin/members/role": {
        if (!staff) {
          show(404, { error: ADMIN_ERRORS.NOT_FOUND });
          return true;
        }
        const changed = members.setRole(staff.userId, form.get("role"));
        if (changed.error) {
          show(400, { error: ADMIN_ERRORS[changed.error] });
          return true;
        }
        if (changed.from !== changed.member.role) {
          team.note("member.role_change", {
            actor,
            targetId: staff.userId,
            targetName: staff.email,
            detail: { from: changed.from, to: changed.member.role },
          });
        }
        redirect(res, "/admin");
        return true;
      }

      case "/admin/members/revoke": {
        if (!staff) {
          show(404, { error: ADMIN_ERRORS.NOT_FOUND });
          return true;
        }
        members.revoke(staff.userId);
        dropLocksOf(staff.userId);
        redirect(res, "/admin");
        return true;
      }

      case "/admin/members/remove": {
        if (!staff) {
          show(404, { error: ADMIN_ERRORS.NOT_FOUND });
          return true;
        }
        if (form.get("confirm") !== "1") {
          show(400, { error: ADMIN_ERRORS.CONFIRM });
          return true;
        }
        members.remove(staff.userId);
        dropLocksOf(staff.userId);
        team.note("member.remove", {
          actor,
          targetId: staff.userId,
          targetName: staff.email,
          detail: { role: staff.role },
        });
        redirect(res, "/admin");
        return true;
      }

      default:
        sendJson(res, 404, { code: "NOT_FOUND" });
        return true;
    }
  };

  return createServer(async (req, res) => {
    try {
      const url = new URL(req.url || "/", "http://control-plane.local");
      const path = url.pathname;
      const method = req.method || "GET";

      if (path === "/healthz" || path === "/health") {
        return sendJson(res, 200, { status: "ok" });
      }

      if (await handleAdmin(req, res, path, method)) return;

      if (path === "/auth/link" && method === "GET") {
        return sendHtml(res, 200, linkForm());
      }

      if (path === "/auth/link" && method === "POST") {
        const form = parseForm(await readBody(req));
        const token = form.get("token") || "";
        if (!safeEqual(token, config.controlToken)) {
          return sendHtml(res, 401, linkForm("CONTROL_TOKEN không đúng."));
        }
        return sendHtml(
          res,
          200,
          codePage(members.issueCode(config.accountId, CODE_TTL_MS)),
        );
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
        const user = signIn(members.takeCode(String(body.code || "")));
        if (!user) {
          return sendJson(res, 401, {
            message: "Mã đăng nhập không đúng hoặc đã hết hạn",
          });
        }
        return sendJson(res, 200, loginPayload(config, user));
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
        // The provisioning key ships inside a build and names no one: it is
        // the owner's, never a staff member's.
        return sendJson(res, 200, loginPayload(config, signIn(config.accountId)));
      }

      if (path === "/api/auth/token/refresh" && method === "POST") {
        let body;
        try {
          body = await readJson(req);
        } catch {
          return sendJson(res, 400, { message: "Invalid JSON body" });
        }
        const claims = verifyToken(config, String(body.refreshToken || ""));
        const user =
          claims?.scope === "refresh" && typeof claims.sub === "string"
            ? members.get(claims.sub)
            : null;
        if (!user || !currentVersion(claims, user)) {
          return sendJson(res, 401, { message: "Refresh token refused" });
        }
        members.touch(user.userId);
        return sendJson(res, 200, {
          accessToken: accessToken(config, user),
          refreshToken: refreshToken(config, user),
        });
      }

      if (path === "/api/auth/logout" && method === "POST") {
        return sendJson(res, 200, { ok: true });
      }

      const user = authenticate(req);
      if (!user) {
        return sendJson(res, 401, { message: "Unauthorized" });
      }
      members.touch(user.userId);

      if (path === "/api/auth/me" && method === "GET") {
        return sendJson(res, 200, account(config, user));
      }

      if (path === "/api/auth/sync-token" && method === "POST") {
        let body = {};
        try {
          body = await readJson(req);
        } catch {
          return sendJson(res, 400, { message: "Invalid JSON body" });
        }
        return sendJson(res, 200, {
          syncToken: syncToken(config, user, body.server),
        });
      }

      if (path === "/api/auth/sync-config" && method === "GET") {
        if (!config.syncServerUrl || !config.syncSharedToken) {
          return sendJson(res, 404, { code: "SYNC_NOT_PROVIDED" });
        }
        return sendJson(res, 200, {
          server: config.syncServerUrl,
          token: config.syncSharedToken,
        });
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
        // The computer is named once for the batch, or on each event; a
        // build that names neither is still logged, without a computer.
        const { accepted, devices } = team.report(
          user,
          body.events,
          checkedDevice(body.device),
        );
        if (devices.length > 0) members.touch(user.userId, devices);
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
        // The owner may change anyone's access but their own, an admin only a
        // non-admin's, and no one else any. Checked before the member is looked
        // up so a plain member learns nothing about who is on the team.
        if (!canSeeTeam(user)) {
          return sendJson(res, 403, { code: "TEAM_ACCESS_FORBIDDEN" });
        }
        const target = members.get(userId);
        if (!target) {
          return sendJson(res, 404, { code: "TEAM_MEMBER_NOT_FOUND" });
        }
        // The owner is the account itself; limiting that seat could lock the
        // person out of their own profiles, so it stays at every group.
        if (target.role === "owner") {
          return sendJson(res, user.role === "owner" ? 400 : 403, {
            code: user.role === "owner" ? "TEAM_ACCESS_INVALID" : "TEAM_ACCESS_FORBIDDEN",
          });
        }
        if (user.role === "admin" && target.role === "admin") {
          return sendJson(res, 403, { code: "TEAM_ACCESS_FORBIDDEN" });
        }
        let body;
        try {
          body = await readJson(req);
        } catch {
          return sendJson(res, 400, { code: "TEAM_ACCESS_INVALID" });
        }
        const checked =
          body && typeof body === "object" && "groups" in body
            ? checkedGroups(body.groups)
            : { error: true };
        if (checked.error) {
          return sendJson(res, 400, { code: "TEAM_ACCESS_INVALID" });
        }
        const set = members.setAccess(target.userId, checked.groups);
        if (set.error) {
          return sendJson(res, 404, { code: "TEAM_MEMBER_NOT_FOUND" });
        }
        if (set.changed) {
          team.note("member.access_change", {
            actor: actorOf(user),
            targetId: target.userId,
            targetName: target.email,
            detail: { groups: checked.groups === null ? "all" : checked.groups.length },
          });
        }
        return sendJson(res, 200, memberRow(set.member));
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
          if (!canSeeTeam(user)) {
            return sendJson(res, 403, { code: "TEAM_ACCESS_FORBIDDEN" });
          }
          return sendJson(res, 200, members.list().map(memberRow));
        }

        if (method !== "GET") {
          return sendJson(res, 405, { code: "METHOD_NOT_ALLOWED" });
        }

        // Who did what is for the owner and admins; a plain member's own
        // actions are in it, but the log is not theirs to read.
        if (!canSeeTeam(user)) {
          return sendJson(res, 403, { code: "TEAM_ACTIVITY_FORBIDDEN" });
        }

        const filters = activityFilters(url);
        if (filters.error) return activityError(res, filters.error);

        if (resource === "activity.csv") {
          const rows = team.all(filters);
          // `device` is last so a script reading columns by position keeps working.
          const header =
            "at,actor_email,actor_kind,action,target_type,target_id,target_name,detail,device";
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
              event.actor?.deviceName,
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

      // One person at a time per profile. A lock lapses after LOCK_TTL_MS
      // unless the desktop renews it, so a computer that crashes frees its
      // profiles. Launches and stops are not written here: the desktop reports
      // them as activity, with the computer they happened on.
      if (path === "/api/profile-locks" && method === "GET") {
        sweepLocks();
        return sendJson(
          res,
          200,
          [...locks].map(([profileId, lock]) => lockInfo(profileId, lock)),
        );
      }

      const lockMatch = path.match(/^\/api\/profile-locks\/([^/]+)(\/heartbeat)?$/);
      if (lockMatch) {
        let profileId;
        try {
          profileId = decodeURIComponent(lockMatch[1]);
        } catch {
          return sendJson(res, 400, { code: "PROFILE_LOCK_INVALID" });
        }
        if (!PROFILE_ID.test(profileId)) {
          return sendJson(res, 400, { code: "PROFILE_LOCK_INVALID" });
        }
        sweepLocks();
        const held = locks.get(profileId);
        const mine = held?.userId === user.userId;

        if (lockMatch[2] && method === "POST") {
          if (!mine) return sendJson(res, 200, { refreshed: false, profileId });
          held.expiresAt = Date.now() + lockTtl;
          return sendJson(res, 200, { refreshed: true, profileId });
        }

        if (!lockMatch[2] && method === "POST") {
          // A refusal is a normal answer, not an error: the desktop reads
          // `success` and names the holder to the person.
          if (held && !mine) {
            return sendJson(res, 200, {
              success: false,
              lockedBy: held.userId,
              lockedByEmail: held.email,
            });
          }
          if (!held && locks.size >= MAX_LOCKS) {
            return sendJson(res, 429, { code: "PROFILE_LOCK_UNAVAILABLE" });
          }
          let device = null;
          try {
            device = checkedDevice((await readJson(req)).device);
          } catch {
            // A launch sends no body; one that is not JSON names no computer.
          }
          const now = Date.now();
          locks.set(profileId, {
            userId: user.userId,
            email: user.email,
            deviceId: device?.id ?? held?.deviceId ?? null,
            lockedAt: held ? held.lockedAt : now,
            expiresAt: now + lockTtl,
          });
          return sendJson(res, 200, {
            success: true,
            lockedBy: user.userId,
            lockedByEmail: user.email,
          });
        }

        if (!lockMatch[2] && method === "DELETE") {
          // Only the holder lets go. Someone else's release (a lock that
          // lapsed and was taken since) frees nothing and says so.
          if (held && !mine) return sendJson(res, 200, { success: false });
          locks.delete(profileId);
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
