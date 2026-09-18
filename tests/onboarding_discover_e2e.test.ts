import { expect, test } from "bun:test";
import { mkdir, mkdtemp, readdir, symlink, writeFile } from "node:fs/promises";
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
async function pkg(path: string) { await mkdir(path, { recursive: true }); await writeFile(join(path, "SKILL.md"), "# fixture\n"); }

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
  expect(first.proposals).toEqual([]);
});

test("duplicate names remain separate by canonical path", async () => {
  const f = await fixture(); await pkg(join(f.library, "same")); await pkg(join(f.library, "two", "same"));
  const report = run(f, ["onboarding", "discover"]);
  expect(report.packages.map((p:any)=>[p.name,p.path,p.status])).toEqual([["same","same","unmanaged"],["same","two/same","unmanaged"]]);
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
