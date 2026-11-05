import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
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
    "at,actor_email,actor_kind,action,target_type,target_id,target_name,detail",
  );
  assert.ok(lines.length >= 3, "the profile events are rows");
  assert.ok(csv.includes("profile.rename"));
  assert.ok(csv.includes("Buying account"));
});

test("a launch and a stop the control plane sees are in the log", async () => {
  const login = await signIn();
  const headers = auth(login.body.accessToken);
  const lock = `/api/profile-locks/profile-logged`;

  await json(lock, { method: "POST", headers });
  await json(lock, { method: "POST", headers });
  const launched = await json(
    `/api/teams/${config.teamId}/activity?action=profile.launch`,
    { headers },
  );
  assert.equal(
    launched.body.items.filter((item) => item.target.id === "profile-logged")
      .length,
    1,
    "a retried launch is one line",
  );

  await json(lock, { method: "DELETE", headers });
  const stopped = await json(
    `/api/teams/${config.teamId}/activity?action=profile.stop`,
    { headers },
  );
  assert.equal(
    stopped.body.items.filter((item) => item.target.id === "profile-logged")
      .length,
    1,
  );
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
