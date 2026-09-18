import { expect, test } from "bun:test";
import { lstat, mkdir, mkdtemp, readFile, readdir, rm, symlink, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { childEnv, commandBinary } from "./test_harness";

const dec = new TextDecoder();

type F = { root: string; config: string; library: string; env: Record<string,string> };
async function fixture(): Promise<F> {
  const root = await mkdtemp(join(tmpdir(), "skillsync-discover-"));
  const config = join(root, "config"), library = join(root, "library"), home = join(root, "home");
  await mkdir(home, { recursive: true });
  return { root, config, library, env: childEnv({ HOME: home, SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library, GIT_TERMINAL_PROMPT: "0" }) };
}
function run(f: F, args: string[]) {
  const r = Bun.spawnSync({ cmd: [commandBinary(), "--json", ...args], env: f.env, stdout: "pipe", stderr: "pipe" });
  const stdout = new TextDecoder().decode(r.stdout), stderr = new TextDecoder().decode(r.stderr);
  expect(r.exitCode, `${stderr}\n${stdout}`).toBe(0);
  return JSON.parse(stdout);
}
async function pkg(path: string) { await mkdir(path, { recursive: true }); await writeFile(join(path, "SKILL.md"), `name: ${path.split("/").pop()}\n`); }
async function invalidPkg(path: string) { await mkdir(path, { recursive: true }); await writeFile(join(path, "SKILL.md"), "# fixture\n"); }

 test("explicit roots discover occurrences without adoption proposals", async () => {
  const f = await fixture(); const explicit = join(f.root, "explicit", "nested"); await pkg(join(explicit, "found"));
  const report = run(f, ["onboarding", "discover", "--root", join(f.root, "explicit"), "--root", join(f.root, "explicit")]);
  expect(report.roots.filter((r:any)=>r.kind === "explicit_root")).toHaveLength(1);
  expect(report.occurrences.map((o:any)=>[o.name,o.root,o.path])).toEqual([["found", join(f.root,"explicit"), "nested/found"]]);
  expect(report.proposals).toEqual([]);
 });

 test("uninitialized discovery finds root and nested packages without creating state", async () => {
  const f = await fixture(); await pkg(f.library); await pkg(join(f.library, "nested", "same"));
  const before = (await readdir(f.root, { recursive: true })).sort(); const report = run(f, ["onboarding", "discover"]);
  expect(report.mode).toBe("read_only"); expect(report.packages.map((p:any)=>p.path)).toEqual([".", "nested/same"]);
  expect((await readdir(f.root, { recursive: true })).sort()).toEqual(before);
  expect(await Bun.file(join(f.config, "state.json")).exists()).toBe(false);
});

test("discovery is deterministic and reports unsafe links without following them", async () => {
  const f = await fixture(); await pkg(join(f.library, "a")); await pkg(join(f.library, "b"));
  await symlink(join(f.library, "a"), join(f.library, "external-link")); await symlink("missing", join(f.library, "dangling"));
  const first = run(f, ["onboarding", "discover"]), second = run(f, ["onboarding", "discover"]);
  expect(first.packages.map((p:any)=>p.path)).toEqual(["a", "b"]);
  expect(first.diagnostics.filter((d:any)=>d.code === "unsupported_reparse").map((d:any)=>d.path)).toEqual([join(f.library,"dangling"), join(f.library,"external-link")]);
  expect(first.proposals.map((p:any)=>p.path)).toEqual(["a", "b"]);
});

test("duplicate names remain separate by canonical path", async () => {
  const f = await fixture(); await pkg(join(f.library, "same")); await pkg(join(f.library, "two", "same"));
  const report = run(f, ["onboarding", "discover"]);
  expect(report.packages.map((p:any)=>[p.name,p.path,p.status])).toEqual([["same","same","unmanaged"],["same","two/same","unmanaged"]]);
});

test("invalid manifests are diagnosed and never proposed for adoption", async () => {
  const f = await fixture(); await invalidPkg(join(f.library, "invalid"));
  const report = run(f, ["onboarding", "discover"]);
  expect(report.packages).toEqual([]);
  expect(report.proposals).toEqual([]);
  expect(report.diagnostics).toContainEqual({ code: "invalid_manifest", path: join(f.library, "invalid/SKILL.md"), detail: "SKILL.md manifest is invalid" });
});


test("unmanaged packages produce explicit review-only adoption proposals", async () => {
  const f = await fixture(); await pkg(join(f.library, "unmanaged"));
  const report = run(f, ["onboarding", "discover"]);
  expect(report.proposals).toEqual([{ kind: "adopt", path: "unmanaged", requires_approval: true, destructive: false }]);
});

test("onboarding plan emits a hash-bound adoption plan", async () => {
  const f = await fixture(); await pkg(join(f.library, "unmanaged"));
  const plan = join(f.root, "plan.json");
  const result = Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "plan", "--out", plan], env: f.env, stdout: "pipe", stderr: "pipe" });
  expect(result.exitCode).toBe(0);
  const document = JSON.parse(new TextDecoder().decode(await Bun.file(plan).arrayBuffer()));
  expect(document.format).toBe("skillsync-onboarding-adoption-plan");
  expect(document.version).toBe(1);
  expect(document.operations).toHaveLength(1);
  expect(document.operations[0]).toMatchObject({ skill: "unmanaged", source_package: "unmanaged", content_hash: expect.any(String) });
});

test("onboarding apply requires explicit approval and makes no mutation", async () => {
  const f = await fixture(); await pkg(join(f.library, "unmanaged"));
  const plan = join(f.root, "plan.json");
  const made = Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "plan", "--out", plan], env: f.env, stdout: "pipe", stderr: "pipe" });
  expect(made.exitCode).toBe(0);
  const before = (await readdir(f.root, { recursive: true })).sort();
  const result = Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "apply", "--plan", plan], env: f.env, stdout: "pipe", stderr: "pipe" });
  expect(result.exitCode).toBe(1);
  expect(new TextDecoder().decode(result.stdout)).toContain("confirmation required");
  expect((await readdir(f.root, { recursive: true })).sort()).toEqual(before);
});

test("onboarding apply records one adoption and replay is idempotent", async () => {
  const f = await fixture(); await pkg(join(f.library, "unmanaged"));
  run(f, ["init"]); const plan = join(f.root, "plan.json");
  const made = Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "plan", "--out", plan], env: f.env, stdout: "pipe", stderr: "pipe" }); expect(made.exitCode).toBe(0);
  const first = Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "apply", "--plan", plan, "--yes"], env: f.env, stdout: "pipe", stderr: "pipe" }); expect(first.exitCode).toBe(0);
  expect(JSON.parse(new TextDecoder().decode(first.stdout)).status).toBe("adopted");
  const second = Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "apply", "--plan", plan, "--yes"], env: f.env, stdout: "pipe", stderr: "pipe" }); expect(second.exitCode).toBe(0);
  expect(JSON.parse(new TextDecoder().decode(second.stdout)).status).toBe("already_adopted");
  expect(Object.keys(run(f, ["status"]).local_adoptions)).toEqual(["local:unmanaged"]);
});


test("onboarding plan applies a canonical root package", async () => {
  const f = await fixture(); await pkg(join(f.library, "unmanaged"));
  run(f, ["init"]); const plan = join(f.root, "plan.json");
  expect(Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "plan", "--out", plan], env: f.env, stdout: "pipe", stderr: "pipe" }).exitCode).toBe(0);
  const applied = Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "apply", "--plan", plan, "--yes"], env: f.env, stdout: "pipe", stderr: "pipe" });
  expect(applied.exitCode).toBe(0);
  expect(JSON.parse(new TextDecoder().decode(applied.stdout))).toMatchObject({status: "adopted", skill: "unmanaged"});
  expect(run(f, ["status"]).local_adoptions["local:unmanaged"].source_package).toBe("unmanaged");
});

test("onboarding plan applies a nested package without collapsing its identity", async () => {
  const f = await fixture(); await pkg(join(f.library, "parent", "child"));
  run(f, ["init"]); const plan = join(f.root, "plan.json");
  expect(Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "plan", "--out", plan], env: f.env, stdout: "pipe", stderr: "pipe" }).exitCode).toBe(0);
  const document = JSON.parse(await readFile(plan, "utf8"));
  expect(document.operations[0]).toMatchObject({skill: "child", source_package: "parent/child", destination: join(f.library, "parent", "child")});
  const applied = Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "apply", "--plan", plan, "--yes"], env: f.env, stdout: "pipe", stderr: "pipe" });
  expect(applied.exitCode).toBe(0);
  const state = run(f, ["status"]);
  expect(state.local_adoptions["local:child"]).toMatchObject({skill: "child", source_package: "parent/child", local_path: join(f.library, "parent", "child")});
});

test("onboarding apply rejects packages managed by an explicit harness link before mutation", async () => {
  const f = await fixture(); const harness = join(f.root, "harness"); await pkg(join(f.library, "managed")); await mkdir(harness, {recursive:true});
  run(f, ["init"]); const plan = join(f.root, "plan.json");
  const madePlan = Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "plan", "--out", plan], env: f.env, stdout: "pipe", stderr: "pipe" });
  expect(madePlan.exitCode).toBe(0);
  run(f, ["harness", "link", "--root", harness, "--skill", "managed"]);
  const stateBefore = await readFile(join(f.config, "state.json"), "utf8");
  const applied = Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "apply", "--plan", plan, "--yes"], env: f.env, stdout: "pipe", stderr: "pipe" });
  expect(applied.exitCode).toBe(1);
  expect(new TextDecoder().decode(applied.stdout)).toContain("managed relationship");
  expect(await readFile(join(f.config, "state.json"), "utf8")).toBe(stateBefore);
  expect(run(f, ["status"]).local_adoptions).toEqual({});
});

test("managed packages produce no adoption proposals", async () => {
  const f = await fixture(); const source = join(f.root, "source"); await pkg(source); await writeFile(join(source, "SKILL.md"), "name: source\n");
  run(f, ["init"]); run(f, ["import", "--from", source, "--skill", "source"]);
  const report = run(f, ["onboarding", "discover"]);
  expect(report.proposals).toEqual([]);
});

test("proposal output is deterministic and discovery does not mutate state", async () => {
  const f = await fixture(); await pkg(join(f.library, "b")); await pkg(join(f.library, "a"));
  const before = (await readdir(f.root, { recursive: true })).sort();
  const first = run(f, ["onboarding", "discover"]), second = run(f, ["onboarding", "discover"]);
  expect(first).toEqual(second); expect((await readdir(f.root, { recursive: true })).sort()).toEqual(before);
  expect(await Bun.file(join(f.config, "state.json")).exists()).toBe(false);
});

test("initialized discovery uses the shared read lock and classifies persisted packages", async () => {
  const f = await fixture();
  const source = join(f.root, "source"); await pkg(source); await writeFile(join(source, "SKILL.md"), "name: source\n");
  run(f, ["init"]);
  run(f, ["import", "--from", source, "--skill", "source"]);
  const report = run(f, ["onboarding", "discover"]);
  expect(report.packages).toEqual([{ name: "source", path: "source", status: "managed" }]);

  const marker = join(f.root, "held");
  const holder = Bun.spawn(["python3", join(import.meta.dir, "hold_lock.py"), join(f.config, "state.lock"), marker], { stdout: "ignore", stderr: "pipe" });
  try {
    for (let i = 0; i < 100 && !(await Bun.file(marker).exists()); i += 1) await Bun.sleep(10);
    expect(await Bun.file(marker).exists()).toBe(true);
    const blocked = Bun.spawnSync({ cmd: [commandBinary(), "--json", "onboarding", "discover"], env: childEnv(f.env), stdout: "pipe", stderr: "pipe" });
    expect(blocked.exitCode).toBe(1);
    expect(dec.decode(blocked.stdout)).toContain("state is busy");
  } finally {
    holder.kill();
    await holder.exited;
  }
});
