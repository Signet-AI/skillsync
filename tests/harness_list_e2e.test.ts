import { expect, test } from "bun:test";
import { lstat, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { childEnv, commandBinary } from "./test_harness";

const dec = new TextDecoder();
type F = { root: string; config: string; library: string; env: Record<string,string> };
async function put(p: string, s: string) { await mkdir(dirname(p), {recursive:true}); await writeFile(p, s); }
async function fixture(): Promise<F> {
  const root = await mkdtemp(join(tmpdir(), "skillsync-harness-list-"));
  const f = { root, config: join(root, "config"), library: join(root, "library"), env: { ...process.env as Record<string,string>, HOME: join(root,"home"), SKILLSYNC_CONFIG_DIR: join(root,"config"), SKILLSYNC_LIBRARY: join(root,"library") } };
  await mkdir(join(root, "home"), {recursive:true});
  return f;
}
function run(f:F, args:string[], ok=true, extra:Record<string,string>={}) {
  const r = Bun.spawnSync({cmd:[commandBinary(extra), ...args], env:childEnv({...f.env, ...extra}), stdout:"pipe", stderr:"pipe"});
  const out = dec.decode(r.stdout), err = dec.decode(r.stderr);
  expect(r.exitCode, err).toBe(ok ? 0 : 1);
  expect(out).not.toBe("");
  return JSON.parse(out);
}
async function linkedFixture() {
  const f = await fixture();
  const harness = join(f.root, "harness");
  await put(join(f.library, "one", "SKILL.md"), "name: one\n");
  await mkdir(harness, {recursive:true});
  await put(join(harness, "unrelated.txt"), "preserve\n");
  run(f, ["--json", "init"]);
  run(f, ["--json", "harness", "link", "--root", harness, "--skill", "one"]);
  return {f, harness};
}

test("initialized harness list is deterministic, read-only, and reports a valid recorded link", async () => {
  const {f, harness} = await linkedFixture();
  const before = new Map<string,string>();
  for (const p of [join(f.config,"state.json"), join(harness,"unrelated.txt")]) before.set(p, await readFile(p,"utf8"));
  const first = run(f, ["--json", "harness", "list"]);
  const second = run(f, ["--json", "harness", "list"]);
  expect(second).toEqual(first);
  expect(first.diagnostics).toHaveLength(1);
  expect(first.diagnostics[0].status).toBe("healthy");
  for (const [p, content] of before) expect(await readFile(p,"utf8")).toBe(content);
  expect(await lstat(join(harness,"one"))).toBeTruthy();
});

test("initialized harness list fails while the shared exclusive lock is held", async () => {
  const {f} = await linkedFixture();
  const marker = join(f.root, "held");
  const lock = Bun.spawn(["python3", join(import.meta.dir, "hold_lock.py"), join(f.config,"state.lock"), marker], {stdout:"ignore", stderr:"pipe"});
  for (let i=0; i<100 && !(await Bun.file(marker).exists()); i++) await new Promise(r => setTimeout(r, 10));
  expect(await Bun.file(marker).exists()).toBe(true);
  const result = Bun.spawnSync({cmd:[commandBinary(), "--json", "harness", "list"], env:childEnv(f.env), stdout:"pipe", stderr:"pipe"});
  expect(result.exitCode).toBe(1);
  expect(dec.decode(result.stdout)).toContain("busy");
  lock.kill();
});

test("uninitialized harness list has no filesystem side effects", async () => {
  const f = await fixture();
  const result = run(f, ["--json", "harness", "list"]);
  expect(result.links).toEqual({});
  expect(result.diagnostics).toEqual([]);
  expect(await Bun.file(f.config).exists()).toBe(false);
  expect(await Bun.file(f.library).exists()).toBe(false);
  expect(await Bun.file(join(f.config,"state.json")).exists()).toBe(false);
  expect(await Bun.file(join(f.config,"state.lock")).exists()).toBe(false);
});

test("harness list inspects only recorded ownership and fails closed for unsafe roots and links", async () => {
  const {f, harness} = await linkedFixture();
  const statePath = join(f.config, "state.json");
  const stateBefore = JSON.parse(await readFile(statePath, "utf8"));
  await symlink(join(f.library,"one"), join(harness,"external"), "dir");
  const adopted = run(f, ["--json", "harness", "list"]);
  expect(Object.keys(adopted.links)).toHaveLength(1);
  expect(adopted.diagnostics.some((x:any) => x.link_path.endsWith("external"))).toBe(false);

  await rm(join(harness,"one"));
  let missing = run(f, ["--json", "harness", "list"]);
  expect(missing.diagnostics[0].status).toBe("missing");
  await put(join(f.library, "two", "SKILL.md"), "name: two\n");
  await symlink(join(f.library,"two"), join(harness,"one"), "dir");
  let wrong = run(f, ["--json", "harness", "list"]);
  expect(wrong.diagnostics[0].status).toBe("wrong_target");
  await rm(join(harness,"one"));
  await put(join(harness,"one"), "collision\n");
  let collision = run(f, ["--json", "harness", "list"]);
  expect(collision.diagnostics[0].status).toBe("collision");
  await rm(join(harness,"one"));
  await rm(harness, {recursive:true});
  await symlink(join(f.root,"missing-root"), harness, "dir");
  const symlinkRoot = run(f, ["--json", "harness", "list"], false);
  expect(symlinkRoot.ok).toBe(false);
  await rm(harness);
  await mkdir(join(f.root,"ancestor"), {recursive:true});
  await symlink(join(f.root,"real-root"), join(f.root,"ancestor","root"), "dir");
  const tampered = JSON.parse(await readFile(statePath, "utf8"));
  const key = Object.keys(tampered.harness_links)[0];
  tampered.harness_links[key].harness_root = join(f.root,"ancestor","root");
  tampered.harness_links[key].link_path = join(f.root,"ancestor","root","one");
  await writeFile(statePath, JSON.stringify(tampered));
  const ancestor = run(f, ["--json", "harness", "list"], false);
  expect(ancestor.ok).toBe(false);
  expect(stateBefore.harness_links[key]).toBeDefined();
});
