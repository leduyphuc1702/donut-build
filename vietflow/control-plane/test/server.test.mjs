import assert from "node:assert/strict";
import { createHmac } from "node:crypto";
import {
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, test } from "node:test";

import { createControlPlane } from "../server.mjs";

const dataDir = mkdtempSync(join(tmpdir(), "control-plane-test-"));

const config = {
  controlToken: "test-control-token-0123456789",
  autoLoginKey: "test-auto-login-key-0123456789",
  jwtSecret: "test-jwt-secret",
  accountId: "owner-1",
  email: "owner@example.test",
  plan: "enterprise",
  syncServerUrl: "https://sync.example.test",
  teamId: "team-1",
  teamName: "Test team",
  dataDir,
};

let server;
let base;

before(async () => {
  server = createControlPlane(config);
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  base = `http://127.0.0.1:${server.address().port}`;
});

after(async () => {
  await new Promise((resolve) => server.close(resolve));
  rmSync(dataDir, { recursive: true, force: true });
});

async function json(path, options = {}) {
  const response = await fetch(`${base}${path}`, options);
  const body = await response.json();
  return { status: response.status, body };
}

function auth(token) {
  return { Authorization: `Bearer ${token}` };
}

async function oneTimeCode() {
  const form = new URLSearchParams({ token: config.controlToken });
  const response = await fetch(`${base}/auth/link`, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: form,
  });
  assert.equal(response.status, 200);
  const html = await response.text();
  const match = html.match(/<code>([0-9A-F]+)<\/code>/);
  assert.ok(match, "the link page shows a one-time code");
  return match[1];
}

async function signIn() {
  const code = await oneTimeCode();
  const challenge = await json("/api/auth/device-code/challenge", {
    method: "POST",
  });
  assert.equal(challenge.status, 200);
  return json("/api/auth/device-code/exchange", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      code,
      challengeId: challenge.body.challengeId,
      nonce: "0",
    }),
  });
}

test("health is public", async () => {
  const response = await json("/healthz");
  assert.equal(response.status, 200);
  assert.equal(response.body.status, "ok");
});

test("a one-time code signs in and yields an unlimited account", async () => {
  const login = await signIn();
  assert.equal(login.status, 200);
  assert.ok(login.body.accessToken);
  assert.ok(login.body.refreshToken);
  assert.equal(login.body.user.plan, "enterprise");
  assert.equal(login.body.user.entitlements.active, true);
  assert.equal(login.body.user.entitlements.requestsPerHour, 0);
  assert.ok(login.body.user.entitlements.profileLimit > 1000);

  const me = await json("/api/auth/me", {
    headers: auth(login.body.accessToken),
  });
  assert.equal(me.status, 200);
  assert.equal(me.body.id, config.accountId);
  assert.equal(me.body.entitlements.crossOsFingerprints, true);
});

test("a code can be used once only", async () => {
  const code = await oneTimeCode();
  const challenge = await json("/api/auth/device-code/challenge", {
    method: "POST",
  });
  const body = JSON.stringify({
    code,
    challengeId: challenge.body.challengeId,
    nonce: "0",
  });
  const first = await json("/api/auth/device-code/exchange", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body,
  });
  assert.equal(first.status, 200);

  const secondChallenge = await json("/api/auth/device-code/challenge", {
    method: "POST",
  });
  const second = await json("/api/auth/device-code/exchange", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      code,
      challengeId: secondChallenge.body.challengeId,
      nonce: "0",
    }),
  });
  assert.equal(second.status, 401);
});

test("a wrong control token cannot mint a code", async () => {
  const form = new URLSearchParams({ token: "wrong-token" });
  const response = await fetch(`${base}/auth/link`, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: form,
  });
  assert.equal(response.status, 401);
});

test("the provisioning key signs a build in without a code", async () => {
  const login = await json("/api/auth/auto", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ key: config.autoLoginKey }),
  });
  assert.equal(login.status, 200);
  assert.ok(login.body.accessToken);
  assert.equal(login.body.user.entitlements.active, true);
  assert.equal(login.body.user.entitlements.crossOsFingerprints, true);

  const me = await json("/api/auth/me", {
    headers: auth(login.body.accessToken),
  });
  assert.equal(me.status, 200);
  assert.equal(me.body.id, config.accountId);
});

test("the control token is not the provisioning key when they differ", async () => {
  const login = await json("/api/auth/auto", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ key: config.controlToken }),
  });
  assert.equal(login.status, 401);
});

test("a wrong provisioning key is refused", async () => {
  const login = await json("/api/auth/auto", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ key: "wrong-key" }),
  });
  assert.equal(login.status, 401);
});

test("refresh rotates the access token", async () => {
  const login = await signIn();
  const refreshed = await json("/api/auth/token/refresh", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ refreshToken: login.body.refreshToken }),
  });
  assert.equal(refreshed.status, 200);
  assert.notEqual(refreshed.body.accessToken, login.body.accessToken);
  const me = await json("/api/auth/me", {
    headers: auth(refreshed.body.accessToken),
  });
  assert.equal(me.status, 200);
});

test("the sync token is bound to the requested server", async () => {
  const login = await signIn();
  const server = "https://sync.owner.test";
  const response = await json("/api/auth/sync-token", {
    method: "POST",
    headers: {
      ...auth(login.body.accessToken),
      "content-type": "application/json",
    },
    body: JSON.stringify({ server }),
  });
  assert.equal(response.status, 200);
  const [header, payload] = response.body.syncToken.split(".");
  assert.ok(header && payload);
  const claims = JSON.parse(Buffer.from(payload, "base64url").toString("utf8"));
  assert.equal(claims.scope, "server-sync");
  assert.equal(claims.aud, server);
});

test("a signed-in device is handed the shared sync server and token", async () => {
  const login = await signIn();
  const unset = await json("/api/auth/sync-config", {
    headers: auth(login.body.accessToken),
  });
  assert.equal(unset.status, 404);
  assert.equal(unset.body.code, "SYNC_NOT_PROVIDED");

  await withPlane(
    async (plane) => {
      const anonymous = await plane.call("/api/auth/sync-config");
      assert.equal(anonymous.status, 401);
      const owner = await ownerLogin(plane);
      const handed = await plane.call("/api/auth/sync-config", {
        headers: auth(owner.accessToken),
      });
      assert.equal(handed.status, 200);
      assert.deepEqual(handed.body, {
        server: "https://sync.example.test",
        token: "shared-sync-token",
      });
    },
    { syncSharedToken: "shared-sync-token" },
  );
});

test("profile locks let a single owner launch without a conflict", async () => {
  const login = await signIn();
  const headers = auth(login.body.accessToken);

  const empty = await json("/api/profile-locks", { headers });
  assert.deepEqual(empty.body, []);

  const acquired = await json("/api/profile-locks/profile-1", {
    method: "POST",
    headers,
  });
  assert.equal(acquired.body.success, true);
  assert.equal(acquired.body.lockedBy, config.accountId);

  const listed = await json("/api/profile-locks", { headers });
  assert.equal(listed.body.length, 1);
  assert.equal(listed.body[0].profileId, "profile-1");
  assert.equal(listed.body[0].lockedBy, config.accountId);
  assert.equal(listed.body[0].lockedByEmail, config.email);
  assert.ok(Date.parse(listed.body[0].expiresAt) > Date.now());

  const heartbeat = await json("/api/profile-locks/profile-1/heartbeat", {
    method: "POST",
    headers,
  });
  assert.equal(heartbeat.body.refreshed, true);

  const released = await json("/api/profile-locks/profile-1", {
    method: "DELETE",
    headers,
  });
  assert.equal(released.status, 200);
});

test("the account is the owner of a one-seat local team", async () => {
  const login = await signIn();
  assert.equal(login.body.user.teamId, config.teamId);
  assert.equal(login.body.user.teamName, config.teamName);
  assert.equal(login.body.user.teamRole, "owner");
  assert.equal(login.body.user.entitlements.teamCollaboration, true);

  const members = await json(`/api/teams/${config.teamId}/members`, {
    headers: auth(login.body.accessToken),
  });
  assert.equal(members.status, 200);
  assert.deepEqual(members.body, [
    {
      userId: config.accountId,
      email: config.email,
      role: "owner",
      groupAccess: null,
    },
  ]);
});

test("the owner seat cannot be limited to some groups", async () => {
  const login = await signIn();
  const headers = auth(login.body.accessToken);
  const url = `/api/teams/${config.teamId}/members/${config.accountId}/access`;

  const limited = await json(url, {
    method: "PUT",
    headers: { ...headers, "content-type": "application/json" },
    body: JSON.stringify({ groups: ["g1"] }),
  });
  assert.equal(limited.status, 400);
  assert.equal(limited.body.code, "TEAM_ACCESS_INVALID");

  const unknown = await json(
    `/api/teams/${config.teamId}/members/someone-else/access`,
    {
      method: "PUT",
      headers: { ...headers, "content-type": "application/json" },
      body: JSON.stringify({ groups: null }),
    },
  );
  assert.equal(unknown.status, 404);
  assert.equal(unknown.body.code, "TEAM_MEMBER_NOT_FOUND");

  const otherTeam = await json(`/api/teams/not-this-team/members`, {
    headers,
  });
  assert.equal(otherTeam.status, 404);
});

test("edits reported by the desktop come back as a filtered page", async () => {
  const login = await signIn();
  const headers = auth(login.body.accessToken);
  const report = await json("/api/teams/activity", {
    method: "POST",
    headers: { ...headers, "content-type": "application/json" },
    body: JSON.stringify({
      events: [
        {
          action: "profile.create",
          targetType: "profile",
          targetId: "p1",
          targetName: "Buying account",
        },
        {
          action: "profile.rename",
          targetType: "profile",
          targetId: "p1",
          targetName: "Buying account",
          detail: { fields: ["name"] },
        },
        {
          action: "proxy.update",
          targetType: "proxy",
          targetId: "x1",
        },
        // An action this build has no shape for is dropped, not stored blind.
        { action: "Not An Action", targetId: "y1" },
      ],
    }),
  });
  assert.equal(report.status, 200);
  assert.equal(report.body.accepted, 3);

  const all = await json(`/api/teams/${config.teamId}/activity`, { headers });
  assert.equal(all.status, 200);
  assert.ok(all.body.items.length >= 3);
  const newest = all.body.items[0];
  assert.equal(newest.action, "proxy.update");
  assert.equal(newest.target.type, "proxy");
  assert.equal(newest.actor.userId, config.accountId);
  assert.equal(newest.source, "client");
  assert.equal(typeof newest.id, "number");

  const profiles = await json(
    `/api/teams/${config.teamId}/activity?targetType=profile`,
    { headers },
  );
  assert.ok(profiles.body.items.length >= 2);
  assert.ok(profiles.body.items.every((item) => item.target.type === "profile"));

  const renamed = await json(
    `/api/teams/${config.teamId}/activity?action=profile.rename`,
    { headers },
  );
  assert.equal(renamed.body.items.length, 1);
  assert.deepEqual(renamed.body.items[0].detail, { fields: ["name"] });

  const wildcard = await json(
    `/api/teams/${config.teamId}/activity?action=profile.*`,
    { headers },
  );
  assert.ok(wildcard.body.items.length >= 2);
  assert.ok(wildcard.body.items.every((item) => item.action.startsWith("profile.")));

  const paged = await json(`/api/teams/${config.teamId}/activity?limit=1`, {
    headers,
  });
  assert.equal(paged.body.items.length, 1);
  assert.ok(paged.body.nextCursor);
  const next = await json(
    `/api/teams/${config.teamId}/activity?limit=1&cursor=${paged.body.nextCursor}`,
    { headers },
  );
  assert.notEqual(next.body.items[0].id, paged.body.items[0].id);
});

test("a malformed filter is refused, never read as an empty page", async () => {
  const login = await signIn();
  const headers = auth(login.body.accessToken);
  for (const query of [
    "cursor=1;drop",
    "limit=lots",
    "action=PROFILE.CREATE",
    "targetType=profile%2F..",
    "since=yesterday",
  ]) {
    const response = await json(
      `/api/teams/${config.teamId}/activity?${query}`,
      { headers },
    );
    assert.equal(response.status, 400, query);
    assert.equal(response.body.code, "TEAM_ACTIVITY_INVALID", query);
  }
});

test("the log exports as a CSV with one row per entry", async () => {
  const login = await signIn();
  const response = await fetch(
    `${base}/api/teams/${config.teamId}/activity.csv?targetType=profile`,
    { headers: auth(login.body.accessToken) },
  );
  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type") ?? "", /text\/csv/);
  const csv = await response.text();
  const lines = csv.trim().split("\n");
  assert.equal(
    lines[0],
    "at,actor_email,actor_kind,action,target_type,target_id,target_name,detail,device",
  );
  assert.ok(lines.length >= 3, "the profile events are rows");
  assert.ok(csv.includes("profile.rename"));
  assert.ok(csv.includes("Buying account"));
});

test("taking and releasing a lock writes nothing to the log", async () => {
  const login = await signIn();
  const headers = auth(login.body.accessToken);
  const lock = `/api/profile-locks/profile-logged`;

  await json(lock, { method: "POST", headers });
  await json(lock, { method: "DELETE", headers });
  for (const action of ["profile.launch", "profile.stop"]) {
    const found = await json(
      `/api/teams/${config.teamId}/activity?action=${action}`,
      { headers },
    );
    assert.equal(
      found.body.items.filter((item) => item.target.id === "profile-logged")
        .length,
      0,
      `${action} comes from the desktop that ran the browser`,
    );
  }
});

test("the agent service keeps routines and their runs locally", async () => {
  const login = await signIn();
  const headers = auth(login.body.accessToken);

  const credits = await json("/api/agent/credits", { headers });
  assert.equal(credits.status, 200);
  assert.equal(credits.body.unlimited, true);
  assert.ok(credits.body.balance > 0);

  const token = await json("/api/agent/token", {
    method: "POST",
    headers: { ...headers, "content-type": "application/json" },
    body: "{}",
  });
  assert.equal(token.status, 200);
  assert.ok(token.body.token.length >= 16);
  assert.ok(!Number.isNaN(Date.parse(token.body.expiresAt)));

  const created = await json("/api/agent/routines", {
    method: "POST",
    headers: { ...headers, "content-type": "application/json" },
    body: JSON.stringify({
      name: "Morning check",
      prompt: "Open the inbox and note new orders",
      startUrl: "https://mail.example.test",
      profileIds: ["p1"],
      schedule: { cron: "0 9 * * 1-5", timezone: "Asia/Ho_Chi_Minh" },
      maxCreditsPerRun: 200,
      concurrency: 1,
      enabled: true,
    }),
  });
  assert.equal(created.status, 200);
  const routine = created.body;
  assert.ok(routine.id);
  assert.equal(routine.name, "Morning check");
  assert.deepEqual(routine.profileIds, ["p1"]);

  const listed = await json("/api/agent/routines", { headers });
  assert.equal(listed.body.routines.length, 1);

  const started = await json(`/api/agent/routines/${routine.id}/runs`, {
    method: "POST",
    headers: { ...headers, "content-type": "application/json" },
    body: JSON.stringify({ profileIds: ["p1"] }),
  });
  assert.equal(started.status, 200);
  assert.ok(started.body.batchId);
  const runId = started.body.runs[0].runId;
  assert.equal(started.body.runs[0].status, "running");

  const reported = await json(`/api/agent/runs/${runId}`, {
    method: "PATCH",
    headers: { ...headers, "content-type": "application/json" },
    body: JSON.stringify({
      status: "done",
      summary: "Two new orders",
      creditsUsed: 12,
    }),
  });
  assert.equal(reported.status, 200);
  assert.equal(reported.body.status, "done");
  assert.ok(reported.body.finishedAt);

  const runs = await json(`/api/agent/routines/${routine.id}/runs`, {
    headers,
  });
  assert.equal(runs.body.runs.length, 1);
  assert.equal(runs.body.runs[0].summary, "Two new orders");

  const updated = await json(`/api/agent/routines/${routine.id}`, {
    method: "PATCH",
    headers: { ...headers, "content-type": "application/json" },
    body: JSON.stringify({
      name: "Evening check",
      prompt: "Open the inbox",
      profileIds: ["p1"],
      enabled: false,
    }),
  });
  assert.equal(updated.status, 200);
  assert.equal(updated.body.name, "Evening check");
  assert.equal(updated.body.enabled, false);

  const bad = await json("/api/agent/routines", {
    method: "POST",
    headers: { ...headers, "content-type": "application/json" },
    body: JSON.stringify({ name: "", prompt: "x" }),
  });
  assert.equal(bad.status, 400);

  const missing = await json("/api/agent/routines/nope/runs", {
    method: "POST",
    headers: { ...headers, "content-type": "application/json" },
    body: JSON.stringify({ profileIds: ["p1"] }),
  });
  assert.equal(missing.status, 404);
  assert.equal(missing.body.code, "AGENT_ROUTINE_NOT_FOUND");

  const deleted = await json(`/api/agent/routines/${routine.id}`, {
    method: "DELETE",
    headers,
  });
  assert.equal(deleted.status, 200);
  const empty = await json("/api/agent/routines", { headers });
  assert.equal(empty.body.routines.length, 0);
});

test("only the services this server does not serve are reported off", async () => {
  const login = await signIn();
  const status = await json("/api/mcp/status", {
    headers: auth(login.body.accessToken),
  });
  assert.equal(status.body.entitled, false);
  assert.equal(login.body.user.entitlements.remoteControl, false);
  assert.equal(login.body.user.entitlements.agent, true);
  assert.equal(login.body.user.entitlements.cookieBot, true);
  assert.equal(login.body.user.entitlements.remoteInteractive, true);
  assert.equal(login.body.user.entitlements.scheduledAutomation, false);
});

test("unauthenticated account routes are refused", async () => {
  const me = await json("/api/auth/me");
  assert.equal(me.status, 401);
});

test("unknown routes are 404", async () => {
  const login = await signIn();
  const response = await json("/api/does-not-exist", {
    headers: auth(login.body.accessToken),
  });
  assert.equal(response.status, 404);
  assert.equal(response.body.code, "NOT_FOUND");
});

test("the remote-session stream is an empty snapshot, not a 404 loop", async () => {
  const login = await signIn();
  const controller = new AbortController();
  const response = await fetch(`${base}/api/remote-sessions/events`, {
    headers: auth(login.body.accessToken),
    signal: controller.signal,
  });
  assert.equal(response.status, 200);
  assert.match(
    response.headers.get("content-type") ?? "",
    /text\/event-stream/,
  );
  const reader = response.body.getReader();
  const first = await reader.read();
  assert.ok(first.value);
  assert.match(Buffer.from(first.value).toString("utf8"), /"type":"snapshot"/);
  controller.abort();
  await reader.cancel().catch(() => {});

  const list = await json("/api/remote-sessions", {
    headers: auth(login.body.accessToken),
  });
  assert.deepEqual(list.body, { sessions: [] });
});

// --- Members: each test runs against a plane of its own ---------------------

const FORM = { "content-type": "application/x-www-form-urlencoded" };
const JSON_HEADERS = { "content-type": "application/json" };

async function startPlane(overrides = {}) {
  const dir = overrides.dataDir ?? mkdtempSync(join(tmpdir(), "control-plane-team-"));
  const plane = createControlPlane({ ...config, ...overrides, dataDir: dir });
  await new Promise((resolve) => plane.listen(0, "127.0.0.1", resolve));
  const url = `http://127.0.0.1:${plane.address().port}`;
  const call = async (path, options = {}) => {
    const response = await fetch(`${url}${path}`, options);
    const text = await response.text();
    let body = text;
    try {
      body = JSON.parse(text);
    } catch {
      // HTML or CSV.
    }
    return { status: response.status, body, headers: response.headers };
  };
  return {
    url,
    dir,
    call,
    close: () => new Promise((resolve) => plane.close(resolve)),
    // A directory the test passed in is the test's to remove, so a restart
    // can read what the first run left.
    cleanup: () => {
      if (!overrides.dataDir) rmSync(dir, { recursive: true, force: true });
    },
  };
}

async function withPlane(run, overrides) {
  const plane = await startPlane(overrides);
  try {
    await run(plane);
  } finally {
    await plane.close();
    plane.cleanup();
  }
}

async function exchange(plane, code) {
  const challenge = await plane.call("/api/auth/device-code/challenge", {
    method: "POST",
  });
  return plane.call("/api/auth/device-code/exchange", {
    method: "POST",
    headers: JSON_HEADERS,
    body: JSON.stringify({
      code,
      challengeId: challenge.body.challengeId,
      nonce: "0",
    }),
  });
}

async function ownerLogin(plane) {
  const link = await plane.call("/auth/link", {
    method: "POST",
    headers: FORM,
    body: new URLSearchParams({ token: config.controlToken }),
  });
  const login = await exchange(plane, link.body.match(/<code>([0-9A-F]+)<\/code>/)[1]);
  assert.equal(login.status, 200);
  return login.body;
}

async function adminSession(plane, token = config.controlToken) {
  const login = await fetch(`${plane.url}/admin/login`, {
    method: "POST",
    redirect: "manual",
    headers: FORM,
    body: new URLSearchParams({ token }),
  });
  const cookie = login.headers.get("set-cookie")?.split(";")[0];
  assert.ok(cookie, "a correct CONTROL_TOKEN opens a session");
  const page = await fetch(`${plane.url}/admin`, { headers: { cookie } });
  const html = await page.text();
  return { cookie, csrf: html.match(/name="csrf" value="([^"]+)"/)[1], html };
}

async function adminPost(plane, session, path, fields = {}, headers = {}) {
  const response = await fetch(`${plane.url}${path}`, {
    method: "POST",
    redirect: "manual",
    headers: { ...FORM, cookie: session.cookie, ...headers },
    body: new URLSearchParams({ csrf: session.csrf, ...fields }),
  });
  return {
    status: response.status,
    html: await response.text(),
    location: response.headers.get("location"),
  };
}

/// Add a staff member on /admin and sign them in with the code it shows.
async function addStaff(plane, admin, email, role) {
  const added = await adminPost(plane, admin, "/admin/members/add", { email, role });
  assert.equal(added.status, 200, added.html);
  const code = added.html.match(/<code>([0-9A-F]+)<\/code>/)[1];
  const login = await exchange(plane, code);
  assert.equal(login.status, 200);
  return {
    code,
    ...login.body,
    headers: auth(login.body.accessToken),
    id: login.body.user.id,
  };
}

function report(plane, who, body) {
  return plane.call("/api/teams/activity", {
    method: "POST",
    headers: { ...who.headers, ...JSON_HEADERS },
    body: JSON.stringify(body),
  });
}

const activity = (plane, who, query = "") =>
  plane.call(`/api/teams/${config.teamId}/activity${query}`, { headers: who.headers });

test("a staff member signs in with a code the owner made and gets their own account", async () => {
  await withPlane(async (plane) => {
    const owner = await ownerLogin(plane);
    const admin = await adminSession(plane);
    const staff = await addStaff(plane, admin, "Nhan.Vien@Example.test", "launcher");

    assert.notEqual(staff.user.id, config.accountId);
    assert.match(staff.user.id, /^[A-Za-z0-9_-]{1,128}$/);
    assert.equal(staff.user.email, "nhan.vien@example.test");
    assert.equal(staff.user.teamRole, "launcher");
    assert.equal(staff.user.teamId, config.teamId);
    assert.equal(staff.user.plan, owner.user.plan);
    assert.equal(staff.user.effectivePlan, owner.user.effectivePlan);
    assert.deepEqual(staff.user.entitlements, owner.user.entitlements);

    const me = await plane.call("/api/auth/me", { headers: staff.headers });
    assert.equal(me.body.id, staff.id);
    assert.equal(me.body.teamRole, "launcher");
    assert.equal(me.body.deviceCount, 1);

    const ownerMe = await plane.call("/api/auth/me", { headers: auth(owner.accessToken) });
    assert.equal(ownerMe.body.id, config.accountId);
    assert.equal(ownerMe.body.teamRole, "owner");

    // The code was theirs alone, and once.
    assert.equal((await exchange(plane, staff.code)).status, 401);

    // JWTs name the person and the version of their sign-in.
    const claims = JSON.parse(
      Buffer.from(staff.accessToken.split(".")[1], "base64url").toString("utf8"),
    );
    assert.equal(claims.sub, staff.id);
    assert.equal(claims.tv, 0);
    assert.equal(claims.scope, "account");

    const refreshed = await plane.call("/api/auth/token/refresh", {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify({ refreshToken: staff.refreshToken }),
    });
    assert.equal(refreshed.status, 200);
    const refreshedMe = await plane.call("/api/auth/me", {
      headers: auth(refreshed.body.accessToken),
    });
    assert.equal(refreshedMe.body.id, staff.id);

    const sync = await plane.call("/api/auth/sync-token", {
      method: "POST",
      headers: { ...staff.headers, ...JSON_HEADERS },
      body: JSON.stringify({ server: "https://sync.example.test" }),
    });
    const syncClaims = JSON.parse(
      Buffer.from(sync.body.syncToken.split(".")[1], "base64url").toString("utf8"),
    );
    assert.equal(syncClaims.sub, staff.id);
  });
});

test("inviting and joining are in the log, by the right people", async () => {
  await withPlane(async (plane) => {
    const owner = auth((await ownerLogin(plane)).accessToken);
    const admin = await adminSession(plane);
    const added = await adminPost(plane, admin, "/admin/members/add", {
      email: "a@example.test",
      role: "member",
    });
    const code = added.html.match(/<code>([0-9A-F]+)<\/code>/)[1];

    const beforeJoin = await plane.call(
      `/api/teams/${config.teamId}/activity?action=member.join`,
      { headers: owner },
    );
    assert.equal(beforeJoin.body.items.length, 1, "only the owner's own join so far");

    const login = await exchange(plane, code);
    const invites = await plane.call(
      `/api/teams/${config.teamId}/activity?action=member.invite`,
      { headers: owner },
    );
    assert.equal(invites.body.items.length, 1);
    assert.equal(invites.body.items[0].actor.email, config.email);
    assert.equal(invites.body.items[0].target.type, "member");
    assert.equal(invites.body.items[0].target.id, login.body.user.id);
    assert.equal(invites.body.items[0].target.name, "a@example.test");
    assert.deepEqual(invites.body.items[0].detail, { role: "member" });

    const joins = await plane.call(
      `/api/teams/${config.teamId}/activity?action=member.join`,
      { headers: owner },
    );
    assert.equal(joins.body.items.length, 2);
    assert.equal(joins.body.items[0].actor.userId, login.body.user.id);
    assert.equal(joins.body.items[0].actor.email, "a@example.test");

    // A new code for someone already in is not a second join.
    const again = await adminPost(plane, admin, "/admin/members/code", {
      userId: login.body.user.id,
    });
    assert.equal(again.status, 200);
    await exchange(plane, again.html.match(/<code>([0-9A-F]+)<\/code>/)[1]);
    const joinsAfter = await plane.call(
      `/api/teams/${config.teamId}/activity?action=member.join`,
      { headers: owner },
    );
    assert.equal(joinsAfter.body.items.length, 2);
  });
});

test("a removed member is refused at once, with every token they hold", async () => {
  await withPlane(async (plane) => {
    const admin = await adminSession(plane);
    const staff = await addStaff(plane, admin, "gone@example.test", "member");
    assert.equal((await plane.call("/api/auth/me", { headers: staff.headers })).status, 200);

    const unconfirmed = await adminPost(plane, admin, "/admin/members/remove", {
      userId: staff.id,
    });
    assert.equal(unconfirmed.status, 400);
    assert.equal((await plane.call("/api/auth/me", { headers: staff.headers })).status, 200);

    const removed = await adminPost(plane, admin, "/admin/members/remove", {
      userId: staff.id,
      confirm: "1",
    });
    assert.equal(removed.status, 303);

    assert.equal((await plane.call("/api/auth/me", { headers: staff.headers })).status, 401);
    assert.equal(
      (await plane.call("/api/profile-locks", { headers: staff.headers })).status,
      401,
    );
    const refresh = await plane.call("/api/auth/token/refresh", {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify({ refreshToken: staff.refreshToken }),
    });
    assert.equal(refresh.status, 401);

    const owner = { headers: auth((await ownerLogin(plane)).accessToken) };
    const log = await activity(plane, owner, "?action=member.remove");
    assert.equal(log.body.items.length, 1);
    assert.equal(log.body.items[0].target.name, "gone@example.test");
    assert.equal(log.body.items[0].actor.email, config.email);

    // The same address can be added again, as a new person.
    const back = await addStaff(plane, admin, "gone@example.test", "member");
    assert.notEqual(back.id, staff.id);
    assert.equal((await plane.call("/api/auth/me", { headers: staff.headers })).status, 401);
  });
});

test("signing a member out everywhere ends their tokens but not their place", async () => {
  await withPlane(async (plane) => {
    const admin = await adminSession(plane);
    const staff = await addStaff(plane, admin, "lost@example.test", "member");

    const revoked = await adminPost(plane, admin, "/admin/members/revoke", {
      userId: staff.id,
    });
    assert.equal(revoked.status, 303);
    assert.equal((await plane.call("/api/auth/me", { headers: staff.headers })).status, 401);
    const refresh = await plane.call("/api/auth/token/refresh", {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify({ refreshToken: staff.refreshToken }),
    });
    assert.equal(refresh.status, 401);

    const code = await adminPost(plane, admin, "/admin/members/code", { userId: staff.id });
    const again = await exchange(plane, code.html.match(/<code>([0-9A-F]+)<\/code>/)[1]);
    assert.equal(again.status, 200);
    assert.equal(again.body.user.id, staff.id);
    const claims = JSON.parse(
      Buffer.from(again.body.accessToken.split(".")[1], "base64url").toString("utf8"),
    );
    assert.equal(claims.tv, 1);
    assert.equal(
      (await plane.call("/api/auth/me", { headers: auth(again.body.accessToken) })).status,
      200,
    );
  });
});

test("a regenerated code voids the one before it", async () => {
  await withPlane(async (plane) => {
    const admin = await adminSession(plane);
    const added = await adminPost(plane, admin, "/admin/members/add", {
      email: "late@example.test",
      role: "member",
    });
    const first = added.html.match(/<code>([0-9A-F]+)<\/code>/)[1];
    const members = await plane.call(`/api/teams/${config.teamId}/members`, {
      headers: auth((await ownerLogin(plane)).accessToken),
    });
    const userId = members.body.find((m) => m.email === "late@example.test").userId;
    const second = (
      await adminPost(plane, admin, "/admin/members/code", { userId })
    ).html.match(/<code>([0-9A-F]+)<\/code>/)[1];
    assert.notEqual(first, second);
    assert.equal((await exchange(plane, first)).status, 401);
    assert.equal((await exchange(plane, second)).status, 200);
  });
});

test("an expired member code is refused", async () => {
  await withPlane(async (plane) => {
    const admin = await adminSession(plane);
    const added = await adminPost(plane, admin, "/admin/members/add", {
      email: "slow@example.test",
      role: "member",
    });
    const code = added.html.match(/<code>([0-9A-F]+)<\/code>/)[1];
    const real = Date.now;
    Date.now = () => real() + 8 * 24 * 60 * 60 * 1000;
    try {
      assert.equal((await exchange(plane, code)).status, 401);
    } finally {
      Date.now = real;
    }
  });
});

test("a role change shows in the very next request", async () => {
  await withPlane(async (plane) => {
    const admin = await adminSession(plane);
    const staff = await addStaff(plane, admin, "up@example.test", "launcher");
    const denied = await activity(plane, staff);
    assert.equal(denied.status, 403);

    const changed = await adminPost(plane, admin, "/admin/members/role", {
      userId: staff.id,
      role: "admin",
    });
    assert.equal(changed.status, 303);

    const me = await plane.call("/api/auth/me", { headers: staff.headers });
    assert.equal(me.body.teamRole, "admin", "the same token, the new role");
    assert.equal((await activity(plane, staff)).status, 200);

    const log = await activity(plane, staff, "?action=member.role_change");
    assert.equal(log.body.items.length, 1);
    assert.deepEqual(log.body.items[0].detail, { from: "launcher", to: "admin" });
    assert.equal(log.body.items[0].target.id, staff.id);
    assert.equal(log.body.items[0].actor.email, config.email);

    // Setting the role it already has is not news.
    await adminPost(plane, admin, "/admin/members/role", { userId: staff.id, role: "admin" });
    assert.equal((await activity(plane, staff, "?action=member.role_change")).body.items.length, 1);

    const bad = await adminPost(plane, admin, "/admin/members/role", {
      userId: staff.id,
      role: "owner",
    });
    assert.equal(bad.status, 400);
    const stillAdmin = await plane.call("/api/auth/me", { headers: staff.headers });
    assert.equal(stillAdmin.body.teamRole, "admin");
  });
});

test("access can be set by the owner, and by an admin for non-admins only", async () => {
  await withPlane(async (plane) => {
    const owner = { headers: auth((await ownerLogin(plane)).accessToken) };
    const admin = await adminSession(plane);
    const boss = await addStaff(plane, admin, "boss@example.test", "admin");
    const peer = await addStaff(plane, admin, "peer@example.test", "admin");
    const worker = await addStaff(plane, admin, "worker@example.test", "member");
    const accessUrl = (userId) => `/api/teams/${config.teamId}/members/${userId}/access`;
    const put = (who, userId, body) =>
      plane.call(accessUrl(userId), {
        method: "PUT",
        headers: { ...who.headers, ...JSON_HEADERS },
        body: JSON.stringify(body),
      });

    const limited = await put(owner, worker.id, { groups: ["g1", "g2", "g1"] });
    assert.equal(limited.status, 200);
    assert.deepEqual(limited.body, {
      userId: worker.id,
      email: "worker@example.test",
      role: "member",
      groupAccess: ["g1", "g2"],
    });

    const listed = await plane.call(`/api/teams/${config.teamId}/members`, {
      headers: owner.headers,
    });
    assert.equal(listed.status, 200);
    assert.deepEqual(
      listed.body.map((m) => [m.email, m.role]),
      [
        [config.email, "owner"],
        ["boss@example.test", "admin"],
        ["peer@example.test", "admin"],
        ["worker@example.test", "member"],
      ],
    );
    assert.deepEqual(
      listed.body.find((m) => m.userId === worker.id).groupAccess,
      ["g1", "g2"],
    );
    assert.equal(listed.body[0].groupAccess, null);

    const changes = await plane.call(
      `/api/teams/${config.teamId}/activity?action=member.access_change`,
      { headers: owner.headers },
    );
    assert.equal(changes.body.items.length, 1);
    assert.deepEqual(changes.body.items[0].detail, { groups: 2 });
    assert.equal(changes.body.items[0].actor.email, config.email);
    assert.equal(changes.body.items[0].target.id, worker.id);

    // An admin sets a non-admin's access, back to every group.
    const reset = await put(boss, worker.id, { groups: null });
    assert.equal(reset.status, 200);
    assert.equal(reset.body.groupAccess, null);
    const afterReset = await plane.call(
      `/api/teams/${config.teamId}/activity?action=member.access_change`,
      { headers: owner.headers },
    );
    assert.deepEqual(afterReset.body.items[0].detail, { groups: "all" });
    assert.equal(afterReset.body.items[0].actor.email, "boss@example.test");

    // An admin cannot touch another admin or the owner.
    assert.equal((await put(boss, peer.id, { groups: [] })).status, 403);
    assert.equal((await put(boss, peer.id, { groups: [] })).body.code, "TEAM_ACCESS_FORBIDDEN");
    assert.equal((await put(boss, config.accountId, { groups: [] })).status, 403);
    // The owner may limit an admin, but not their own seat.
    assert.equal((await put(owner, peer.id, { groups: ["g9"] })).status, 200);
    const self = await put(owner, config.accountId, { groups: ["g1"] });
    assert.equal(self.status, 400);
    assert.equal(self.body.code, "TEAM_ACCESS_INVALID");

    // A plain member changes nothing and lists nothing.
    const refused = await put(worker, worker.id, { groups: null });
    assert.equal(refused.status, 403);
    assert.equal(refused.body.code, "TEAM_ACCESS_FORBIDDEN");
    const roster = await plane.call(`/api/teams/${config.teamId}/members`, {
      headers: worker.headers,
    });
    assert.equal(roster.status, 403);
    assert.equal(roster.body.code, "TEAM_ACCESS_FORBIDDEN");
    assert.equal(
      (await plane.call(`/api/teams/${config.teamId}/members`, { headers: boss.headers })).status,
      200,
    );

    // Malformed access and unknown people.
    for (const body of [{ groups: ["a/b"] }, { groups: "all" }, { groups: [1] }, {}]) {
      const bad = await put(owner, worker.id, body);
      assert.equal(bad.status, 400, JSON.stringify(body));
      assert.equal(bad.body.code, "TEAM_ACCESS_INVALID");
    }
    const unknown = await put(owner, "nobody", { groups: null });
    assert.equal(unknown.status, 404);
    assert.equal(unknown.body.code, "TEAM_MEMBER_NOT_FOUND");
    // The refused changes left the owner's choice standing.
    const final = await plane.call(`/api/teams/${config.teamId}/members`, {
      headers: owner.headers,
    });
    assert.deepEqual(final.body.find((m) => m.userId === peer.id).groupAccess, ["g9"]);
    assert.equal(final.body.find((m) => m.userId === worker.id).groupAccess, null);
  });
});

test("only the owner and admins read the log; everyone's work is in it", async () => {
  await withPlane(async (plane) => {
    const owner = { headers: auth((await ownerLogin(plane)).accessToken) };
    const admin = await adminSession(plane);
    const worker = await addStaff(plane, admin, "w@example.test", "member");
    const launcher = await addStaff(plane, admin, "l@example.test", "launcher");
    const lead = await addStaff(plane, admin, "lead@example.test", "admin");

    const sent = await report(plane, worker, {
      events: [{ action: "profile.create", targetType: "profile", targetId: "p1" }],
    });
    assert.equal(sent.body.accepted, 1);

    for (const who of [worker, launcher]) {
      const list = await activity(plane, who);
      assert.equal(list.status, 403);
      assert.equal(list.body.code, "TEAM_ACTIVITY_FORBIDDEN");
      const csv = await plane.call(`/api/teams/${config.teamId}/activity.csv`, {
        headers: who.headers,
      });
      assert.equal(csv.status, 403);
    }
    for (const who of [owner, lead]) {
      const list = await activity(plane, who, "?action=profile.create");
      assert.equal(list.status, 200);
      assert.equal(list.body.items[0].actor.email, "w@example.test");
      assert.equal(
        (await plane.call(`/api/teams/${config.teamId}/activity.csv`, { headers: who.headers }))
          .status,
        200,
      );
    }

    // The log can be narrowed to one person's work.
    await report(plane, launcher, {
      events: [{ action: "profile.launch", targetType: "profile", targetId: "p1" }],
    });
    const mine = await activity(plane, owner, `?actorUserId=${launcher.id}`);
    assert.equal(mine.body.items.length >= 2, true, "their sign-in and their launch");
    assert.ok(mine.body.items.every((item) => item.actor.userId === launcher.id));
  });
});

test("a report names who sent it and from which computer", async () => {
  await withPlane(async (plane) => {
    const owner = { headers: auth((await ownerLogin(plane)).accessToken) };
    const admin = await adminSession(plane);
    const worker = await addStaff(plane, admin, "dev@example.test", "member");

    const sent = await report(plane, worker, {
      device: { id: "dev-aaa_1", name: "  May-ke-toan  " },
      events: [
        { action: "profile.launch", targetType: "profile", targetId: "p1", targetName: "Shop" },
        {
          action: "profile.stop",
          targetType: "profile",
          targetId: "p1",
          device: { id: "dev-bbb", name: "Laptop B" },
        },
      ],
    });
    assert.equal(sent.body.accepted, 2);

    const log = await activity(plane, owner, "?targetType=profile");
    const launch = log.body.items.find((item) => item.action === "profile.launch");
    const stop = log.body.items.find((item) => item.action === "profile.stop");
    assert.equal(launch.actor.userId, worker.id);
    assert.equal(launch.actor.email, "dev@example.test");
    assert.equal(launch.actor.kind, "desktop");
    assert.equal(launch.actor.deviceId, "dev-aaa_1");
    assert.equal(launch.actor.deviceName, "May-ke-toan");
    assert.equal(stop.actor.deviceId, "dev-bbb", "an event's own device wins over the batch's");
    assert.equal(stop.actor.deviceName, "Laptop B");

    const csv = await plane.call(`/api/teams/${config.teamId}/activity.csv?targetType=profile`, {
      headers: owner.headers,
    });
    const lines = csv.body.trim().split("\n");
    assert.ok(lines[0].endsWith(",device"));
    assert.ok(lines.some((line) => line.includes("profile.launch") && line.endsWith(",May-ke-toan")));
    assert.ok(lines.some((line) => line.includes("profile.stop") && line.endsWith(",Laptop B")));

    // The person now has two computers, seen in their account and on /admin.
    const me = await plane.call("/api/auth/me", { headers: worker.headers });
    assert.equal(me.body.deviceCount, 2);
    const page = await fetch(`${plane.url}/admin`, { headers: { cookie: admin.cookie } });
    const html = await page.text();
    assert.ok(html.includes("May-ke-toan"));
    assert.ok(html.includes("Laptop B"));
  });
});

test("a computer that does not fit the log is dropped, not stored", async () => {
  await withPlane(async (plane) => {
    const owner = { headers: auth((await ownerLogin(plane)).accessToken) };
    const admin = await adminSession(plane);
    const worker = await addStaff(plane, admin, "odd@example.test", "member");
    const event = { action: "proxy.update", targetType: "proxy", targetId: "x1" };

    await report(plane, worker, { events: [{ ...event, device: { id: "../etc", name: "Bad id" } }] });
    await report(plane, worker, { events: [{ ...event, device: { id: "x".repeat(129), name: "Long id" } }] });
    await report(plane, worker, { events: [{ ...event, device: "laptop" }] });
    await report(plane, worker, {
      events: [{ ...event, device: { id: "ok-1", name: `PC\n\u202eevil\u0007${"n".repeat(100)}` } }],
    });
    await report(plane, worker, { events: [{ ...event, device: { id: "ok-2" } }] });

    const items = (await activity(plane, owner, "?action=proxy.update")).body.items.reverse();
    assert.equal(items.length, 5);
    assert.equal(items[0].actor.deviceId, null);
    assert.equal(items[0].actor.deviceName, null);
    assert.equal(items[1].actor.deviceId, null);
    assert.equal(items[2].actor.deviceId, null);
    assert.equal(items[3].actor.deviceId, "ok-1");
    assert.equal(items[3].actor.deviceName.length, 64);
    assert.ok(items[3].actor.deviceName.startsWith("PCevil"), items[3].actor.deviceName);
    assert.ok(!/[\u0000-\u001f\u202e]/.test(items[3].actor.deviceName));
    assert.equal(items[4].actor.deviceName, "ok-2", "no name falls back to the id");
  });
});

test("membership entries are the server's to write, not the desktop's", async () => {
  await withPlane(async (plane) => {
    const owner = { headers: auth((await ownerLogin(plane)).accessToken) };
    const admin = await adminSession(plane);
    const worker = await addStaff(plane, admin, "forger@example.test", "member");
    const sent = await report(plane, worker, {
      events: [
        { action: "member.remove", targetType: "member", targetId: "someone" },
        { action: "invitation.cancel", targetType: "invitation", targetId: "i1" },
        { action: "profile.rename", targetType: "profile", targetId: "p1" },
      ],
    });
    assert.equal(sent.body.accepted, 1);
    const removes = await activity(plane, owner, "?action=member.remove");
    assert.equal(removes.body.items.length, 0);
  });
});

test("the CSV defuses cells a spreadsheet would run", async () => {
  await withPlane(async (plane) => {
    const owner = { headers: auth((await ownerLogin(plane)).accessToken) };
    await report(plane, owner, {
      device: { id: "d1", name: "=HYPERLINK(x)" },
      events: [
        { action: "profile.rename", targetType: "profile", targetId: "p1", targetName: "+cmd|calc" },
      ],
    });
    const csv = await plane.call(
      `/api/teams/${config.teamId}/activity.csv?action=profile.rename`,
      { headers: owner.headers },
    );
    const row = csv.body.trim().split("\n")[1];
    assert.ok(row.includes(",'+cmd|calc,"), row);
    assert.ok(row.endsWith(",'=HYPERLINK(x)"), row);
  });
});

test("a profile lock is held by one person at a time", async () => {
  await withPlane(async (plane) => {
    const admin = await adminSession(plane);
    const alice = await addStaff(plane, admin, "alice@example.test", "member");
    const bob = await addStaff(plane, admin, "bob@example.test", "member");
    const lock = (who, profileId, method = "POST", tail = "") =>
      plane.call(`/api/profile-locks/${profileId}${tail}`, { method, headers: who.headers });

    const first = await lock(alice, "profile-a");
    assert.equal(first.status, 200);
    assert.equal(first.body.success, true);
    assert.equal(first.body.lockedBy, alice.id);
    assert.equal(first.body.lockedByEmail, "alice@example.test");

    // A conflict is a normal 200 the desktop reads, naming the holder.
    const taken = await lock(bob, "profile-a");
    assert.equal(taken.status, 200);
    assert.deepEqual(taken.body, {
      success: false,
      lockedBy: alice.id,
      lockedByEmail: "alice@example.test",
    });

    // The holder asking again is the same launch retried.
    assert.equal((await lock(alice, "profile-a")).body.success, true);

    const listed = await plane.call("/api/profile-locks", { headers: bob.headers });
    assert.equal(listed.body.length, 1);
    assert.equal(listed.body[0].profileId, "profile-a");
    assert.equal(listed.body[0].lockedBy, alice.id);
    assert.equal(listed.body[0].lockedByEmail, "alice@example.test");
    assert.ok(!Number.isNaN(Date.parse(listed.body[0].lockedAt)));
    assert.ok(Date.parse(listed.body[0].expiresAt) > Date.now());

    // Another profile is free, and only the holder renews or releases.
    assert.equal((await lock(bob, "profile-b")).body.success, true);
    assert.equal((await lock(bob, "profile-a", "POST", "/heartbeat")).body.refreshed, false);
    assert.equal((await lock(alice, "profile-a", "POST", "/heartbeat")).body.refreshed, true);
    assert.equal((await lock(bob, "profile-a", "DELETE")).body.success, false);
    assert.equal((await lock(bob, "profile-a")).body.success, false, "bob's release freed nothing");

    assert.equal((await lock(alice, "profile-a", "DELETE")).body.success, true);
    assert.equal((await lock(bob, "profile-a")).body.success, true);

    assert.equal((await lock(alice, "no/such", "POST")).status, 404);
    assert.equal((await lock(alice, "bad%ZZid", "POST")).status, 400);
  });
});

test("a lock lapses unless the heartbeat renews it", async () => {
  await withPlane(
    async (plane) => {
      const admin = await adminSession(plane);
      const alice = await addStaff(plane, admin, "alice@example.test", "member");
      const bob = await addStaff(plane, admin, "bob@example.test", "member");
      const lock = (who, profileId, method = "POST", tail = "") =>
        plane.call(`/api/profile-locks/${profileId}${tail}`, { method, headers: who.headers });
      const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

      await lock(alice, "kept");
      await lock(alice, "lapsed");
      await sleep(250);
      assert.equal((await lock(alice, "kept", "POST", "/heartbeat")).body.refreshed, true);
      await sleep(250);
      // 500 ms in: "lapsed" ran out at 400 ms, "kept" was renewed at 250 ms.
      assert.equal((await lock(bob, "kept")).body.success, false);
      assert.equal((await lock(bob, "lapsed")).body.success, true);
      assert.deepEqual(
        (await plane.call("/api/profile-locks", { headers: bob.headers })).body
          .map((info) => info.profileId)
          .sort(),
        ["kept", "lapsed"],
      );
      // The lapsed holder learns it from the heartbeat and may take it back
      // only when it is free again.
      assert.equal((await lock(alice, "lapsed", "POST", "/heartbeat")).body.refreshed, false);
      await sleep(450);
      assert.deepEqual(
        (await plane.call("/api/profile-locks", { headers: bob.headers })).body,
        [],
      );
    },
    { lockTtlMs: 400 },
  );
});

test("removing a member frees the locks they held", async () => {
  await withPlane(async (plane) => {
    const owner = { headers: auth((await ownerLogin(plane)).accessToken) };
    const admin = await adminSession(plane);
    const alice = await addStaff(plane, admin, "alice@example.test", "member");
    await plane.call("/api/profile-locks/held", { method: "POST", headers: alice.headers });
    assert.equal((await plane.call("/api/profile-locks", { headers: owner.headers })).body.length, 1);
    await adminPost(plane, admin, "/admin/members/remove", { userId: alice.id, confirm: "1" });
    assert.deepEqual((await plane.call("/api/profile-locks", { headers: owner.headers })).body, []);
  });
});

test("the admin page needs the control token, a session and a matching form token", async () => {
  await withPlane(async (plane) => {
    // No session: a login form, never the member list.
    const anonymous = await fetch(`${plane.url}/admin`);
    assert.equal(anonymous.status, 200);
    const loginHtml = await anonymous.text();
    assert.ok(loginHtml.includes('action="/admin/login"'));
    assert.ok(!loginHtml.includes("/admin/members/add"));

    // A bearer token is not a session: the page is for a person at a form.
    const bearerOnly = await fetch(`${plane.url}/admin`, {
      headers: auth(config.controlToken),
    });
    assert.ok(!(await bearerOnly.text()).includes("/admin/members/add"));

    // A wrong token, and a login from another site, are refused.
    const wrong = await fetch(`${plane.url}/admin/login`, {
      method: "POST",
      redirect: "manual",
      headers: FORM,
      body: new URLSearchParams({ token: "wrong-token" }),
    });
    assert.equal(wrong.status, 401);
    assert.equal(wrong.headers.get("set-cookie"), null);
    const foreign = await fetch(`${plane.url}/admin/login`, {
      method: "POST",
      redirect: "manual",
      headers: { ...FORM, origin: "https://evil.example" },
      body: new URLSearchParams({ token: config.controlToken }),
    });
    assert.equal(foreign.status, 403);
    assert.equal(foreign.headers.get("set-cookie"), null);

    // The session cookie is HttpOnly and SameSite=Strict, scoped to /admin.
    const login = await fetch(`${plane.url}/admin/login`, {
      method: "POST",
      redirect: "manual",
      headers: FORM,
      body: new URLSearchParams({ token: config.controlToken }),
    });
    assert.equal(login.status, 303);
    assert.equal(login.headers.get("location"), "/admin");
    const cookie = login.headers.get("set-cookie");
    assert.match(cookie, /^donut_admin=[A-Za-z0-9_-]{40,};/);
    assert.match(cookie, /HttpOnly/);
    assert.match(cookie, /SameSite=Strict/);
    assert.match(cookie, /Path=\/admin/);
    assert.match(cookie, /Max-Age=\d+/);
    assert.ok(!/Secure/.test(cookie), "plain http in the test");

    const behindHttps = await fetch(`${plane.url}/admin/login`, {
      method: "POST",
      redirect: "manual",
      headers: { ...FORM, "x-forwarded-proto": "https" },
      body: new URLSearchParams({ token: config.controlToken }),
    });
    assert.match(behindHttps.headers.get("set-cookie"), /; Secure/);

    const session = await adminSession(plane);
    const fields = { email: "csrf@example.test", role: "member" };

    // Changes without a session, without the form token, with a wrong one, or
    // from another origin all do nothing.
    const noCookie = await fetch(`${plane.url}/admin/members/add`, {
      method: "POST",
      headers: FORM,
      body: new URLSearchParams({ csrf: session.csrf, ...fields }),
    });
    assert.equal(noCookie.status, 401);
    const noToken = await fetch(`${plane.url}/admin/members/add`, {
      method: "POST",
      headers: { ...FORM, cookie: session.cookie },
      body: new URLSearchParams(fields),
    });
    assert.equal(noToken.status, 403);
    const wrongToken = await adminPost(plane, { ...session, csrf: "x".repeat(32) }, "/admin/members/add", fields);
    assert.equal(wrongToken.status, 403);
    const crossSite = await adminPost(plane, session, "/admin/members/add", fields, {
      origin: "https://evil.example",
    });
    assert.equal(crossSite.status, 403);
    const nullOrigin = await adminPost(plane, session, "/admin/members/add", fields, {
      origin: "null",
    });
    assert.equal(nullOrigin.status, 403);
    const owner = { headers: auth((await ownerLogin(plane)).accessToken) };
    const none = await plane.call(`/api/teams/${config.teamId}/members`, { headers: owner.headers });
    assert.equal(none.body.length, 1, "none of the refused forms added anyone");

    // The same form from this origin works.
    const host = new URL(plane.url).host;
    const ok = await adminPost(plane, session, "/admin/members/add", fields, {
      origin: `http://${host}`,
    });
    assert.equal(ok.status, 200);
    assert.ok(ok.html.includes("csrf@example.test"));

    // Logging out ends the session.
    const out = await adminPost(plane, session, "/admin/logout");
    assert.equal(out.status, 303);
    const after = await fetch(`${plane.url}/admin`, { headers: { cookie: session.cookie } });
    assert.ok(!(await after.text()).includes("/admin/members/add"));

    // A GET never changes anything.
    const get = await fetch(`${plane.url}/admin/members/add`, { headers: { cookie: session.cookie } });
    assert.equal(get.status, 405);
  });
});

test("an idle admin session expires", async () => {
  await withPlane(async (plane) => {
    const session = await adminSession(plane);
    const real = Date.now;
    Date.now = () => real() + 31 * 60 * 1000;
    try {
      const page = await fetch(`${plane.url}/admin`, { headers: { cookie: session.cookie } });
      assert.ok(!(await page.text()).includes("/admin/members/add"));
      const post = await adminPost(plane, session, "/admin/members/add", {
        email: "late@example.test",
        role: "member",
      });
      assert.equal(post.status, 401);
    } finally {
      Date.now = real;
    }
  });
});

test("the admin page validates people and escapes what it prints", async () => {
  await withPlane(async (plane) => {
    const admin = await adminSession(plane);
    const bad = async (fields) => adminPost(plane, admin, "/admin/members/add", fields);

    assert.equal((await bad({ email: "not-an-email", role: "member" })).status, 400);
    assert.equal((await bad({ email: "<script>@x.test", role: "member" })).status, 400);
    assert.equal((await bad({ email: "ok@example.test", role: "owner" })).status, 400);
    assert.equal((await bad({ email: "ok@example.test", role: "" })).status, 400);
    const ownerEmail = await bad({ email: config.email.toUpperCase(), role: "member" });
    assert.equal(ownerEmail.status, 409);

    await addStaff(plane, admin, "dup@example.test", "member");
    const duplicate = await bad({ email: "DUP@example.test", role: "admin" });
    assert.equal(duplicate.status, 409);
    assert.ok(duplicate.html.includes("đã có trong nhóm"));

    // The owner is not a row anyone can change here.
    for (const path of ["/admin/members/role", "/admin/members/remove", "/admin/members/revoke", "/admin/members/code"]) {
      const response = await adminPost(plane, admin, path, {
        userId: config.accountId,
        role: "member",
        confirm: "1",
      });
      assert.equal(response.status, 404, path);
    }
    assert.equal((await adminPost(plane, admin, "/admin/nope")).status, 404);

    // A computer's name is typed by its owner; the page shows it as text.
    const worker = await addStaff(plane, admin, "xss@example.test", "member");
    await report(plane, worker, {
      device: { id: "d-xss", name: `<img src=x onerror=alert(1)>"'&` },
      events: [{ action: "proxy.update", targetType: "proxy", targetId: "x1" }],
    });
    const page = await (await fetch(`${plane.url}/admin`, { headers: { cookie: admin.cookie } })).text();
    assert.ok(!page.includes("<img src=x"));
    assert.ok(page.includes("&lt;img src=x onerror=alert(1)&gt;&quot;&#39;&amp;"));
  });
});

test("members and their codes survive a restart", async () => {
  const dir = mkdtempSync(join(tmpdir(), "control-plane-restart-"));
  try {
    let code;
    let staffId;
    await withPlane(
      async (plane) => {
        const admin = await adminSession(plane);
        const first = await addStaff(plane, admin, "kept@example.test", "admin");
        staffId = first.id;
        const added = await adminPost(plane, admin, "/admin/members/add", {
          email: "waiting@example.test",
          role: "launcher",
        });
        code = added.html.match(/<code>([0-9A-F]+)<\/code>/)[1];
        await adminPost(plane, admin, "/admin/members/revoke", { userId: first.id });
      },
      { dataDir: dir },
    );

    const stored = readFileSync(join(dir, "members.json"), "utf8");
    assert.ok(!stored.includes(code), "a sign-in code is kept hashed");

    await withPlane(
      async (plane) => {
        const login = await exchange(plane, code);
        assert.equal(login.status, 200, "a code issued before the restart still works");
        assert.equal(login.body.user.teamRole, "launcher");
        const owner = { headers: auth((await ownerLogin(plane)).accessToken) };
        const roster = await plane.call(`/api/teams/${config.teamId}/members`, {
          headers: owner.headers,
        });
        const kept = roster.body.find((m) => m.userId === staffId);
        assert.equal(kept.email, "kept@example.test");
        assert.equal(kept.role, "admin");
        // The version bump from before the restart is remembered.
        const again = await addStaffCode(plane, kept.userId);
        assert.equal(JSON.parse(Buffer.from(again.accessToken.split(".")[1], "base64url")).tv, 1);
      },
      { dataDir: dir },
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

async function addStaffCode(plane, userId) {
  const admin = await adminSession(plane);
  const regenerated = await adminPost(plane, admin, "/admin/members/code", { userId });
  const login = await exchange(plane, regenerated.html.match(/<code>([0-9A-F]+)<\/code>/)[1]);
  return login.body;
}

test("a members file that cannot be read is moved aside, not overwritten", async () => {
  const dir = mkdtempSync(join(tmpdir(), "control-plane-corrupt-"));
  try {
    writeFileSync(join(dir, "members.json"), "{ not json");
    await withPlane(
      async (plane) => {
        const owner = { headers: auth((await ownerLogin(plane)).accessToken) };
        const roster = await plane.call(`/api/teams/${config.teamId}/members`, {
          headers: owner.headers,
        });
        assert.equal(roster.body.length, 1);
      },
      { dataDir: dir },
    );
    const moved = readdirSync(dir).filter((name) => name.startsWith("members.json.corrupt-"));
    assert.equal(moved.length, 1);
    assert.equal(readFileSync(join(dir, moved[0]), "utf8"), "{ not json");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

function legacyToken(claims) {
  const part = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");
  const head = part({ alg: "HS256", typ: "JWT" });
  const body = part({ iat: Math.floor(Date.now() / 1000), ...claims });
  const signature = createHmac("sha256", config.jwtSecret)
    .update(`${head}.${body}`)
    .digest("base64url");
  return `${head}.${body}.${signature}`;
}

test("tokens from before members existed still sign the owner in", async () => {
  await withPlane(async (plane) => {
    const exp = Math.floor(Date.now() / 1000) + 3600;
    const old = legacyToken({ sub: config.accountId, scope: "account", exp });
    const me = await plane.call("/api/auth/me", { headers: auth(old) });
    assert.equal(me.status, 200);
    assert.equal(me.body.teamRole, "owner");
    assert.equal(me.body.id, config.accountId);

    const refresh = await plane.call("/api/auth/token/refresh", {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify({
        refreshToken: legacyToken({ sub: config.accountId, scope: "refresh", exp }),
      }),
    });
    assert.equal(refresh.status, 200);

    // A token for someone who is not on the team, or the wrong scope, is not
    // a way in.
    const ghost = legacyToken({ sub: "ghost", scope: "account", tv: 0, exp });
    assert.equal((await plane.call("/api/auth/me", { headers: auth(ghost) })).status, 401);
    const wrongScope = legacyToken({ sub: config.accountId, scope: "server-sync", exp });
    assert.equal((await plane.call("/api/auth/me", { headers: auth(wrongScope) })).status, 401);
  });
});

test("the raw control token and the provisioning key are the owner's", async () => {
  await withPlane(async (plane) => {
    const admin = await adminSession(plane);
    await addStaff(plane, admin, "someone@example.test", "member");

    const raw = await plane.call("/api/auth/me", { headers: auth(config.controlToken) });
    assert.equal(raw.body.id, config.accountId);
    assert.equal(raw.body.teamRole, "owner");

    const auto = await plane.call("/api/auth/auto", {
      method: "POST",
      headers: JSON_HEADERS,
      body: JSON.stringify({ key: config.autoLoginKey }),
    });
    assert.equal(auto.status, 200);
    assert.equal(auto.body.user.id, config.accountId);
    assert.equal(auto.body.user.teamRole, "owner");

    const link = await ownerLogin(plane);
    assert.equal(link.user.teamRole, "owner");
  });
});
