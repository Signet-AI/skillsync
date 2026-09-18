import { expect, test, afterEach } from "bun:test";
import { mkdtemp, mkdir, rm, readFile, writeFile, symlink, unlink } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { childEnv, commandBinary } from "./test_harness";

type F = { root: string; config: string; data: string; env: Record<string,string> };
const fixtures: F[] = [];
const dec = new TextDecoder();
async function fixture(): Promise<F> { const root=await mkdtemp(join(tmpdir(),"skillsync-worker-")); const env: Record<string,string>={...process.env as Record<string,string>,HOME:join(root,"home"),XDG_DATA_HOME:join(root,"data"),SKILLSYNC_CONFIG_DIR:join(root,"config")}; if (process.platform === "win32") { env.LOCALAPPDATA=join(root,"data"); env.APPDATA=join(root,"appdata"); env.USERPROFILE=join(root,"home"); } const f={root,config:join(root,"config"),data:join(root,"data"),env}; await mkdir(join(root,"home"),{recursive:true}); fixtures.push(f); return f; }
function run(f:F,args:string[],ok=true,extra:Record<string,string>={}) { const r=Bun.spawnSync({cmd:[commandBinary(extra),"--json",...args],env:childEnv({...f.env,...extra}),stdout:"pipe",stderr:"pipe"}); const out=dec.decode(r.stdout); expect(r.exitCode,dec.decode(r.stderr)).toBe(ok?0:1); return JSON.parse(out); }
afterEach(async()=>{ await Promise.all(fixtures.splice(0).map(f=>rm(f.root,{recursive:true,force:true}))); });

function expectedArtifacts(f: F) {
  if (process.platform === "darwin") {
    const root = join(f.env.HOME, "Library/Application Support/skillsync");
    return {
      executable: join(root, "bin", "skillsync"),
      registration: join(f.env.HOME, "Library/LaunchAgents/com.skillsync.worker.plist"),
    };
  }
  if (process.platform === "win32") {
    const root = join(f.env.LOCALAPPDATA ?? "", "skillsync");
    return { executable: join(root, "bin", "skillsync.exe"), registration: join(root, "worker-task.xml") };
  }
  const root = join(f.env.XDG_DATA_HOME ?? join(f.env.HOME, ".local/share"), "skillsync");
  return {
    executable: join(root, "bin", "skillsync"),
    registration: join(f.env.HOME, ".config/systemd/user/skillsync-worker.service"),
  };
}

test("status is read-only before setup and enable is idempotent with exact artifacts",async()=>{ const f=await fixture(); const expected = expectedArtifacts(f); expect(run(f,["worker","status"]).registered).toBe(false); const e=run(f,["worker","enable","--interval","17"],true,{SKILLSYNC_TEST_WORKER_PROVIDER:"ok"}); expect(e.worker).toBe("enabled"); const meta=JSON.parse(await readFile(join(f.config,"worker-registration.json"),"utf8")); expect(meta.interval).toBe(17); expect(meta.executable_path).toBe(expected.executable); expect(meta.registration_path).toBe(expected.registration); const registration = await readFile(meta.registration_path,"utf8"); expect(registration).toContain(meta.executable_path); expect(registration).toContain("worker"); expect(registration).toContain("17"); expect(run(f,["worker","enable","--interval","17"],true,{SKILLSYNC_TEST_WORKER_PROVIDER:"ok"}).worker).toBe("already_enabled"); });

test("changed executable and malformed metadata fail closed",async()=>{ const f=await fixture(); run(f,["worker","enable"],true,{SKILLSYNC_TEST_WORKER_PROVIDER:"ok"}); const metaPath=join(f.config,"worker-registration.json"); const m=JSON.parse(await readFile(metaPath,"utf8")); await writeFile(m.executable_path,"tampered"); expect(run(f,["worker","status"],false).ok).toBe(false); await writeFile(metaPath,JSON.stringify({...m,registration_path:join(f.root,"victim")})); expect(run(f,["worker","uninstall"],false).ok).toBe(false); });

test("uninstall rejects a tampered executable hash without deleting the executable", async () => {
  const f = await fixture();
  run(f, ["worker", "enable"], true, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" });
  const metaPath = join(f.config, "worker-registration.json");
  const metadata = JSON.parse(await readFile(metaPath, "utf8"));
  await writeFile(metaPath, JSON.stringify({ ...metadata, executable_hash: "0".repeat(64) }));
  const failed = run(f, ["worker", "uninstall"], false, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" });
  expect(failed.ok).toBe(false);
  expect(failed.message).toContain("executable ownership");
  expect(await Bun.file(metadata.executable_path).exists()).toBe(true);
  expect(await Bun.file(metaPath).exists()).toBe(true);
});

test("provider failure never reports success",async()=>{ const f=await fixture(); const r=run(f,["worker","enable"],false,{SKILLSYNC_TEST_WORKER_PROVIDER:"fail"}); expect(r.ok).toBe(false); expect(await Bun.file(join(f.config,"worker-registration.json")).exists()).toBe(false); });

test("disable and uninstall are idempotent", async () => {
  const f = await fixture();
  run(f, ["worker", "enable"], true, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" });
  expect(run(f, ["worker", "disable"], true, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" }).worker).toBe("disabled");
  expect(run(f, ["worker", "disable"], true, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" }).worker).toBe("already_disabled");
  expect(run(f, ["worker", "status"]).enabled).toBe(false);
  expect(run(f, ["worker", "uninstall"], true, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" }).worker).toBe("uninstalled");
  expect(run(f, ["worker", "uninstall"], true, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" }).worker).toBe("already_absent");
}, { timeout: 20000 });

test("metadata-save failure deactivates and removes owned registration artifacts", async () => {
  const f = await fixture();
  const failed = run(f, ["worker", "enable"], false, {
    SKILLSYNC_TEST_WORKER_PROVIDER: "ok",
    SKILLSYNC_TEST_FAIL_REGISTRATION_METADATA_SAVE: "1",
  });
  expect(failed.ok).toBe(false);
  expect(failed.message).toContain("rolled back");
  expect(await Bun.file(join(f.config, "worker-registration.json")).exists()).toBe(false);
  expect(await Bun.file(join(f.data, "skillsync/bin/skillsync")).exists()).toBe(false);
  expect(await Bun.file(join(f.env.HOME, ".config/systemd/user/skillsync-worker.service")).exists()).toBe(false);
});

test("registration, metadata, and executable symlinks fail closed", async () => {
  const f = await fixture();
  run(f, ["worker", "enable"], true, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" });
  const metaPath = join(f.config, "worker-registration.json");
  const meta = JSON.parse(await readFile(metaPath, "utf8"));
  const external = join(f.root, "external");
  await writeFile(external, "must survive\n");

  await unlink(meta.registration_path);
  await symlink(external, meta.registration_path);
  expect(run(f, ["worker", "status"], false).ok).toBe(false);
  expect(await readFile(external, "utf8")).toBe("must survive\n");

  await unlink(meta.registration_path);
  await unlink(metaPath);
  await symlink(external, metaPath);
  expect(run(f, ["worker", "status"], false).ok).toBe(false);
  expect(await readFile(external, "utf8")).toBe("must survive\n");

  await unlink(metaPath);
  await writeFile(metaPath, JSON.stringify(meta));
  await unlink(meta.executable_path);
  await symlink(external, meta.executable_path);
  expect(run(f, ["worker", "status"], false).ok).toBe(false);
  expect(await readFile(external, "utf8")).toBe("must survive\n");
});

test("ordinary commands do not register a worker", async () => {
  const f = await fixture();
  expect(run(f, ["status"]).ok).toBe(true);
  expect(run(f, ["doctor"]).ok).toBe(true);
  expect(await Bun.file(join(f.config, "worker-registration.json")).exists()).toBe(false);
  run(f, ["init"]);
  run(f, ["worker", "--once"]);
  expect(await Bun.file(join(f.config, "worker-registration.json")).exists()).toBe(false);
  expect(await Bun.file(join(f.data, "skillsync/bin/skillsync")).exists()).toBe(false);
});

test("worker subcommands reject legacy worker flags", async () => {
  const f = await fixture();
  for (const args of [
    ["worker", "--once", "enable"],
    ["worker", "--interval", "9", "status"],
    ["worker", "--once", "uninstall"],
  ]) {
    const result = run(f, args, false, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" });
    expect(result.ok).toBe(false);
    expect(result.message).toContain("cannot be combined");
  }
  expect(await Bun.file(join(f.config, "worker-registration.json")).exists()).toBe(false);
});

test("worker registration commands honor the shared state lock", async () => {
  if (process.platform === "win32") return;
  const f = await fixture();
  run(f, ["init"]);
  const marker = join(f.root, "lock-held");
  const holder = Bun.spawn({
    cmd: ["python3", join(import.meta.dir, "hold_lock.py"), join(f.config, "state.lock"), marker],
    stdout: "ignore",
    stderr: "pipe",
  });
  try {
    for (let attempt = 0; attempt < 100 && !(await Bun.file(marker).exists()); attempt += 1) await Bun.sleep(20);
    expect(await Bun.file(marker).exists()).toBe(true);
    for (const args of [["worker", "enable"], ["worker", "disable"], ["worker", "uninstall"]]) {
      const blocked = run(f, args, false, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" });
      expect(blocked.message).toContain("state is busy");
    }
  } finally {
    holder.kill();
    await holder.exited;
  }
});

test("disable metadata-save failure restores enabled metadata and provider state", async () => {
  const f = await fixture();
  run(f, ["worker", "enable"], true, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" });
  const failed = run(f, ["worker", "disable"], false, {
    SKILLSYNC_TEST_WORKER_PROVIDER: "ok",
    SKILLSYNC_TEST_FAIL_REGISTRATION_METADATA_SAVE: "1",
  });
  expect(failed.ok).toBe(false);
  expect(failed.message).toContain("rolled back");
  const status = run(f, ["worker", "status"]);
  expect(status.enabled).toBe(true);
  const metadata = JSON.parse(await readFile(join(f.config, "worker-registration.json"), "utf8"));
  expect(metadata.enabled).toBe(true);
  expect(await Bun.file(metadata.executable_path).exists()).toBe(true);
  expect(await Bun.file(metadata.registration_path).exists()).toBe(true);
});

test("uninstall removal failure rolls back owned artifacts", async () => {
  const f = await fixture();
  run(f, ["worker", "enable"], true, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" });
  const metadata = JSON.parse(await readFile(join(f.config, "worker-registration.json"), "utf8"));
  const registrationBefore = await readFile(metadata.registration_path, "utf8");
  const failed = run(f, ["worker", "uninstall"], false, {
    SKILLSYNC_TEST_WORKER_PROVIDER: "ok",
    SKILLSYNC_TEST_FAIL_REGISTRATION_REMOVE: "worker executable",
  });
  expect(failed.ok).toBe(false);
  expect(failed.message).toContain("rolled back");
  expect(await readFile(metadata.registration_path, "utf8")).toBe(registrationBefore);
  expect(await Bun.file(metadata.executable_path).exists()).toBe(true);
  expect(await Bun.file(join(f.config, "worker-registration.json")).exists()).toBe(true);
  expect(run(f, ["worker", "status"]).enabled).toBe(true);
});

test("uninstall refuses an externally replaced registration artifact", async () => {
  const f = await fixture();
  run(f, ["worker", "enable"], true, { SKILLSYNC_TEST_WORKER_PROVIDER: "ok" });
  const metadata = JSON.parse(await readFile(join(f.config, "worker-registration.json"), "utf8"));
  await writeFile(metadata.registration_path, "external registration\n");
  const failed = run(f, ["worker", "uninstall"], false);
  expect(failed.ok).toBe(false);
  expect(failed.message).toContain("ownership");
  expect(await readFile(metadata.registration_path, "utf8")).toBe("external registration\n");
});
