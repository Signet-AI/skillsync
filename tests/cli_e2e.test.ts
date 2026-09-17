import { expect, test } from "bun:test";
import { chmod, lstat, mkdtemp, mkdir, readFile, readdir, realpath, rm, stat, symlink, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { childEnv, commandBinary } from "./test_harness";

type Env = Record<string, string>;
type Result = { code: number; stdout: string; stderr: string };

type Fixture = {
  root: string;
  config: string;
  library: string;
  env: Env;
};

const binary = commandBinary();
const decoder = new TextDecoder();

function inheritedEnv(): Env {
  return Object.fromEntries(
    Object.entries(process.env).filter((entry): entry is [string, string] => entry[1] !== undefined),
  );
}

function run(program: string, args: string[], cwd: string | undefined, env: Env): Result {
  const result = Bun.spawnSync({
    cmd: [program, ...args],
    cwd,
    env: childEnv(env),
    stdout: "pipe",
    stderr: "pipe",
  });
  return {
    code: result.exitCode,
    stdout: decoder.decode(result.stdout),
    stderr: decoder.decode(result.stderr),
  };
}

function checked(result: Result, label: string): Result {
  expect(result.code, `${label}\nstderr: ${result.stderr}`).toBe(0);
  return result;
}

async function put(path: string, contents: string): Promise<void> {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, contents);
}

async function makeFixture(): Promise<Fixture> {
  const root = await mkdtemp(join(tmpdir(), "skillsync-bun-"));
  const home = join(root, "home");
  const config = join(root, "config");
  const library = join(root, "library");
  await mkdir(home, { recursive: true });
  const gitConfig = join(root, "gitconfig");
  await writeFile(
    gitConfig,
    "[user]\n\tname = Skillsync Bun Test\n\temail = skillsync@example.invalid\n[core]\n\tautocrlf = false\n",
  );
  return {
    root,
    config,
    library,
    env: {
      HOME: home,
      SKILLSYNC_CONFIG_DIR: config,
      SKILLSYNC_LIBRARY: library,
      GIT_CONFIG_GLOBAL: gitConfig,
      GIT_TERMINAL_PROMPT: "0",
    },
  };
}

function git(fixture: Fixture, cwd: string, args: string[]): Result {
  return checked(run("git", args, cwd, fixture.env), `git ${args.join(" ")}`);
}

function skillsync(fixture: Fixture, args: string[], success = true, extra: Env = {}): { result: Result; json: any } {
  const result = run(commandBinary(extra), args, undefined, { ...fixture.env, ...extra });
  expect(result.code, `skillsync ${args.join(" ")}\nstderr: ${result.stderr}`).toBe(success ? 0 : 1);
  expect(result.stdout.length, `skillsync stdout was empty: ${result.stderr}`).toBeGreaterThan(0);
  return { result, json: JSON.parse(result.stdout) };
}

function countPublications(status: any): number {
  return Object.keys(status.publications ?? {}).length;
}

test("inventory discovers an effective library before initialization without mutation", async () => {
  const fixture = await makeFixture();
  await put(join(fixture.library, "SKILL.md"), "name: root\n");
  await put(join(fixture.library, "references/child/SKILL.md"), "name: child\n");
  const before = await readdir(fixture.library, { recursive: true });
  const result = skillsync(fixture, ["--json", "inventory"]).json;
  expect(result.library).toBe(process.platform === "win32" ? await realpath(fixture.library) : fixture.library);
  expect(result.packages.map((item: any) => [item.name, item.path])).toEqual([
    ["root", "."],
    ["child", "references/child"],
  ]);
  expect(await readdir(fixture.library, { recursive: true })).toEqual(before);
  expect(await Bun.file(join(fixture.config, "config.toml")).exists()).toBe(false);
  expect(await Bun.file(join(fixture.config, "state.json")).exists()).toBe(false);
  expect(await Bun.file(join(fixture.config, "state.lock")).exists()).toBe(false);
});

test("inventory is deterministic for root and nested packages without mutation", async () => {
  const fixture = await makeFixture();
  await put(join(fixture.library, "SKILL.md"), "name: root\n");
  await put(join(fixture.library, "references/child/SKILL.md"), "name: child\n");
  skillsync(fixture, ["--json", "init"]);
  const statePath = join(fixture.config, "state.json");
  const lockPath = join(fixture.config, "state.lock");
  const beforeState = await stat(statePath);
  const beforeLock = await stat(lockPath);
  const first = skillsync(fixture, ["--json", "inventory"]).json;
  const second = skillsync(fixture, ["--json", "inventory"]).json;
  expect(first).toEqual(second);
  expect(first.packages.map((item: any) => [item.name, item.path])).toEqual([
    ["root", "."],
    ["child", "references/child"],
  ]);
  expect(first.packages[0].sources[0].kind).toBe("local");
  expect(first.capabilities.hermes_autonomous_curation).toBe("unsupported");
  expect((await stat(statePath)).mtimeMs).toBe(beforeState.mtimeMs);
  expect((await stat(lockPath)).mtimeMs).toBe(beforeLock.mtimeMs);
});

test("inventory attributes publication only to the canonical same-named package", async () => {
  const fixture = await makeFixture();
  const destination = join(fixture.root, "publication-destination.git");
  await put(join(fixture.library, "canonical/SKILL.md"), `name: canonical
canonical
`);
  await put(join(fixture.library, "nested/canonical/SKILL.md"), `name: canonical
nested
`);
  checked(run("git", ["init", "--bare", destination], undefined, fixture.env), "init publication destination");
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "publish", "canonical", "--repo", destination, "--yes"]);

  const inventory = skillsync(fixture, ["--json", "inventory"]).json;
  const packages = inventory.packages.filter((item: any) => item.name === "canonical");
  expect(packages.map((item: any) => item.path)).toEqual(["canonical", "nested/canonical"]);
  expect(packages[0].relationship.publications).toHaveLength(1);
  expect(packages[1].relationship.publications).toEqual([]);
});

test("does not replace a destination that appears after the absence check", async () => {
  if (process.platform !== "linux") return;
  const fixture = await makeFixture();
  const source = join(fixture.root, "source");
  await put(join(source, "SKILL.md"), "name: raced\nincoming\n");
  skillsync(fixture, ["--json", "init"]);
  const failed = run(commandBinary({ SKILLSYNC_TEST_IMPORT_COLLISION: "1" }), ["--json", "import", "--from", source, "--skill", "raced"], undefined, {
    ...fixture.env,
    SKILLSYNC_TEST_IMPORT_COLLISION: "1",
  });
  expect(failed.code).toBe(1);
  expect(failed.stdout).toContain("without replacement");
  expect(await readFile(join(fixture.library, "raced/SKILL.md"), "utf8")).toBe("external collision\n");
  expect(skillsync(fixture, ["--json", "status"]).json.local_adoptions).toEqual({});
});

test("rolls back after post-install verification failure", async () => {
  const fixture = await makeFixture();
  const source = join(fixture.root, "source");
  await put(join(source, "SKILL.md"), "name: verify\nv1\n");
  skillsync(fixture, ["--json", "init"]);
  const failed = run(commandBinary({ SKILLSYNC_TEST_IMPORT_VERIFY_FAILURE: "1" }), ["--json", "import", "--from", source, "--skill", "verify"], undefined, {
    ...fixture.env,
    SKILLSYNC_TEST_IMPORT_VERIFY_FAILURE: "1",
  });
  expect(failed.code).toBe(1);
  expect(failed.stdout).toContain("recovery required");
  expect(await readFile(join(fixture.library, "verify/SKILL.md"), "utf8")).toBe("post-install mutation\n");
  expect(skillsync(fixture, ["--json", "status"]).json.local_adoptions).toEqual({});
});
test("imports a local package idempotently and records provenance", async () => {
  const fixture = await makeFixture();
  const source = join(fixture.root, "local-source");
  await put(join(source, "nested/SKILL.md"), "name: demo\nlocal\n");
  skillsync(fixture, ["--json", "init"]);
  const first = skillsync(fixture, ["--json", "import", "--from", source, "--skill", "nested"]).json;
  expect(first.status).toBe("adopted");
  const second = skillsync(fixture, ["--json", "import", "--from", source, "--skill", "nested"]).json;
  expect(second.status).toBe("already_present");
  expect(second.provenance).toBe("recorded");
  const status = skillsync(fixture, ["--json", "status"]).json;
  expect(Object.keys(status.local_adoptions)).toEqual(["local:demo"]);
  expect(await readFile(join(fixture.library, "demo/SKILL.md"), "utf8")).toBe("name: demo\nlocal\n");
});

test("rejects a symlinked existing destination without adopting external content", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  const source = join(fixture.root, "source");
  const external = join(fixture.root, "external-package");
  await put(join(source, "SKILL.md"), "name: adopted\nincoming\n");
  await put(join(external, "SKILL.md"), "name: adopted\nexternal\n");
  skillsync(fixture, ["--json", "init"]);
  await symlink(external, join(fixture.library, "adopted"), "dir");

  const rejected = skillsync(
    fixture,
    ["--json", "import", "--from", source, "--skill", "adopted"],
    false,
  );
  expect(rejected.json.ok).toBe(false);
  expect(rejected.json.message).toMatch(/symlink|reparse point/);
  expect(await readFile(join(external, "SKILL.md"), "utf8")).toBe("name: adopted\nexternal\n");
  expect(skillsync(fixture, ["--json", "status"]).json.local_adoptions).toEqual({});
});
test("imports a root package with nested resources and preserves Unix modes", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  const source = join(fixture.root, "root-package");
  await put(join(source, "SKILL.md"), "name: root-demo\nroot\n");
  await put(join(source, "references/nested/SKILL.md"), "name: child\n");
  await put(join(source, "scripts/run.sh"), "#!/bin/sh\nexit 0\n");
  await chmod(join(source, "scripts/run.sh"), 0o755);
  skillsync(fixture, ["--json", "init"]);
  expect(skillsync(fixture, ["--json", "import", "--from", source, "--skill", "root-demo"]).json.status).toBe("adopted");
  expect(await readFile(join(fixture.library, "root-demo/references/nested/SKILL.md"), "utf8")).toBe("name: child\n");
  expect((await stat(join(fixture.library, "root-demo/scripts/run.sh"))).mode & 0o111).toBe(0o111);
});

test("rejects unsupported special files before reading them", async () => {
  if (process.platform !== "linux") return;
  const fixture = await makeFixture();
  const source = join(fixture.root, "special-file");
  await put(join(source, "SKILL.md"), "name: special\n");
  const fifo = join(source, "blocked.fifo");
  const created = Bun.spawnSync({ cmd: ["mkfifo", fifo], stdout: "pipe", stderr: "pipe" });
  expect(created.exitCode).toBe(0);
  skillsync(fixture, ["--json", "init"]);
  const rejected = skillsync(fixture, ["--json", "import", "--from", source, "--skill", "special"], false);
  expect(rejected.json.ok).toBe(false);
  expect(await Bun.file(join(fixture.library, "special/SKILL.md")).exists()).toBe(false);
});

test("imports one selected package and fails missing selection", async () => {
  const fixture = await makeFixture();
  const source = join(fixture.root, "many");
  await put(join(source, "one/SKILL.md"), "name: one\n");
  await put(join(source, "two/SKILL.md"), "name: two\n");
  skillsync(fixture, ["--json", "init"]);
  expect(skillsync(fixture, ["--json", "import", "--from", source], false).json.message).toContain("--skill is required");
  expect(skillsync(fixture, ["--json", "import", "--from", source, "--skill", "one"]).json.skill).toBe("one");
  expect(await Bun.file(join(fixture.library, "two/SKILL.md")).exists()).toBe(false);
  expect(skillsync(fixture, ["--json", "import", "--from", source, "--skill", "missing"], false).json.message).toContain("skill not found");
});

test("rejects malformed manifests and symlinked import roots", async () => {
  if (process.platform === "win32") return;
  const noWhitespace = await makeFixture();
  await put(join(noWhitespace.root, "source/SKILL.md"), "name:demo\n");
  skillsync(noWhitespace, ["--json", "init"]);
  expect(skillsync(noWhitespace, ["--json", "import", "--from", join(noWhitespace.root, "source"), "--skill", "demo"], false).json.ok).toBe(false);
  for (const [label, manifest] of [["indented", "  name: bad\n"], ["duplicate", "name: bad\nname: other\n"], ["quoted", 'name: "bad"\n'], ["conflicting", "name: bad\nname: other\n"], ["prose", "This is prose\n"]] as const) {
    const fixture = await makeFixture(); const source = join(fixture.root, label);
    await put(join(source, "SKILL.md"), manifest); skillsync(fixture, ["--json", "init"]);
    expect(skillsync(fixture, ["--json", "import", "--from", source, "--skill", "bad"], false).json.ok).toBe(false);
  }
  const fixture = await makeFixture(); const real = join(fixture.root, "real"), linked = join(fixture.root, "linked");
  await put(join(real, "SKILL.md"), "name: linked\n"); await symlink(real, linked, "dir"); skillsync(fixture, ["--json", "init"]);
  expect(skillsync(fixture, ["--json", "import", "--from", linked, "--skill", "linked"], false).json.message).toContain("symlink");
});

test("preserves differing import collisions", async () => {
  const fixture = await makeFixture(); const source = join(fixture.root, "source");
  await put(join(source, "SKILL.md"), "name: collision\nincoming\n"); await put(join(fixture.library, "collision/SKILL.md"), "name: collision\nexisting\n");
  skillsync(fixture, ["--json", "init"]); expect(skillsync(fixture, ["--json", "import", "--from", source, "--skill", "collision"], false).json.message).toContain("different contents");
  expect(await readFile(join(fixture.library, "collision/SKILL.md"), "utf8")).toContain("existing");
});

test("local adoption provenance tampering is rejected", async () => {
  const fixture = await makeFixture(); const source = join(fixture.root, "source");
  await put(join(source, "SKILL.md"), "name: adopted\n"); skillsync(fixture, ["--json", "init"]); skillsync(fixture, ["--json", "import", "--from", source, "--skill", "adopted"]);
  const statePath = join(fixture.config, "state.json"); const state = JSON.parse(await readFile(statePath, "utf8"));
  for (const [key, field, value] of [["local:wrong", "source_package", "../escape"], ["local:adopted", "local_path", join(fixture.root, "outside")], ["local:adopted", "content_hash", "0".repeat(64)]]) {
    const tampered = structuredClone(state); delete tampered.local_adoptions["local:adopted"]; tampered.local_adoptions[key] = { ...state.local_adoptions["local:adopted"], [field]: value }; await writeFile(statePath, JSON.stringify(tampered));
    expect(skillsync(fixture, ["--json", "status"], false).json.ok).toBe(false);
  }
});

test("rejects a dangling persisted local-adoption source symlink", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  const source = join(fixture.root, "source");
  await put(join(source, "SKILL.md"), "name: adopted\n");
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "import", "--from", source, "--skill", "adopted"]);
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  const dangling = join(fixture.root, "dangling");
  await symlink(join(fixture.root, "does-not-exist"), dangling, "dir");
  state.local_adoptions["local:adopted"].source_path = dangling;
  await writeFile(statePath, JSON.stringify(state));
  const rejected = skillsync(fixture, ["--json", "status"], false);
  expect(rejected.json.ok).toBe(false);
  expect(rejected.json.message).toContain("symlink");
});

test("rolls back a new import when provenance state save fails", async () => {
  const fixture = await makeFixture();
  const source = join(fixture.root, "source");
  await put(join(source, "SKILL.md"), "name: rollback\nv1\n");
  skillsync(fixture, ["--json", "init"]);
  const failed = run(commandBinary({ SKILLSYNC_TEST_FAIL_STATE_SAVE: "1" }), ["--json", "import", "--from", source, "--skill", "rollback"], undefined, {
    ...fixture.env,
    SKILLSYNC_TEST_FAIL_STATE_SAVE: "1",
  });
  expect(failed.code).toBe(1);
  expect(failed.stdout).toContain("rolled back");
  expect(await Bun.file(join(fixture.library, "rollback/SKILL.md")).exists()).toBe(false);
  expect(skillsync(fixture, ["--json", "status"]).json.local_adoptions).toEqual({});
});

test("rolls back a failed subscribe completely and allows retry", async () => {
  const fixture = await makeFixture();
  const source = join(fixture.root, "source");
  await put(join(source, "SKILL.md"), "name: subscribe-rollback\nv1\n");
  checked(run("git", ["init", "-q"], source, fixture.env), "init subscribe source");
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-qm", "initial"]);
  skillsync(fixture, ["--json", "init"]);
  const failed = run(commandBinary({ SKILLSYNC_TEST_FAIL_STATE_SAVE: "1" }), ["--json", "subscribe", source, "--skill", "subscribe-rollback"], undefined, {
    ...fixture.env,
    SKILLSYNC_TEST_FAIL_STATE_SAVE: "1",
  });
  expect(failed.code).toBe(1);
  expect(failed.stdout).toContain("rolled back");
  expect(await Bun.file(join(fixture.library, "subscribe-rollback/SKILL.md")).exists()).toBe(false);
  expect(await Bun.file(join(fixture.config, "baselines")).exists()).toBe(false);
  expect(skillsync(fixture, ["--json", "status"]).json.subscriptions).toEqual({});
  expect(skillsync(fixture, ["--json", "subscribe", source, "--skill", "subscribe-rollback"]).json.skill).toBe("subscribe-rollback");
  expect(await Bun.file(join(fixture.library, "subscribe-rollback/SKILL.md")).exists()).toBe(true);
  expect(Object.keys(skillsync(fixture, ["--json", "status"]).json.subscriptions)).toHaveLength(1);
});

test("local Git subscribe, merge, conflict recovery, and scoped publication", async () => {
  const fixture = await makeFixture();
  const source = join(fixture.root, "source");
  const upstream = join(fixture.root, "upstream.git");

  checked(run("git", ["init", "--bare", upstream], undefined, fixture.env), "init source remote");
  checked(run("git", ["init", "-b", "trunk", source], undefined, fixture.env), "init source");
  await put(join(source, "skills/demo/SKILL.md"), "name: demo\nbase\n");
  await put(join(source, "skills/demo/references/one.md"), "one\n");
  await put(join(source, "skills/demo/references/child/SKILL.md"), "name: child\n");
  await put(join(source, "unrelated.txt"), "unrelated\n");
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "initial"]);
  git(fixture, source, ["remote", "add", "origin", upstream]);
  git(fixture, source, ["push", "-u", "origin", "trunk"]);

  expect(skillsync(fixture, ["--json", "init"]).json.ok).toBe(true);
  const workerOnce = skillsync(fixture, ["--json", "worker", "--once"]).json;
  expect(workerOnce.worker).toBe("completed");
  expect(workerOnce.results).toEqual([]);
  expect(skillsync(fixture, ["--json", "status"]).json.worker).toBe("stopped");
  const subscribed = skillsync(fixture, ["--json", "subscribe", upstream, "--skill", "skills/demo"]).json;
  expect(subscribed.skill).toBe("demo");
  expect(await readFile(join(fixture.library, "demo/SKILL.md"), "utf8")).toBe("name: demo\nbase\n");
  expect(await readFile(join(fixture.library, "demo/references/child/SKILL.md"), "utf8")).toBe(
    "name: child\n",
  );
  expect(await Bun.file(join(fixture.library, "unrelated.txt")).exists()).toBe(false);

  const unchanged = skillsync(fixture, ["--json", "update"]).json;
  expect(unchanged.results.some((item: any) => item.status === "synced")).toBe(true);

  await put(join(fixture.library, "demo/references/one.md"), "local learning\n");
  await put(join(source, "skills/demo/SKILL.md"), "name: demo\nupstream\n");
  await put(join(source, "skills/demo/references/newdir/new.md"), "nested\n");
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "upstream change"]);
  git(fixture, source, ["push"]);

  const merged = skillsync(fixture, ["--json", "update"]).json;
  expect(merged.results.some((item: any) => item.skill === "demo" && item.status === "synced")).toBe(true);
  expect(await readFile(join(fixture.library, "demo/SKILL.md"), "utf8")).toBe("name: demo\nupstream\n");
  expect(await readFile(join(fixture.library, "demo/references/one.md"), "utf8")).toBe("local learning\n");
  expect(await readFile(join(fixture.library, "demo/references/newdir/new.md"), "utf8")).toBe("nested\n");

  await put(join(fixture.library, "demo/SKILL.md"), "name: demo\nlocal conflict\n");
  await put(join(source, "skills/demo/SKILL.md"), "name: demo\nremote conflict\n");
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "conflict"]);
  git(fixture, source, ["push"]);
  const conflict = skillsync(fixture, ["--json", "update"]).json;
  expect(conflict.results.some((item: any) => item.skill === "demo" && item.status === "conflict")).toBe(true);
  const live = await readFile(join(fixture.library, "demo/SKILL.md"), "utf8");
  expect(live).toContain("local conflict");
  expect(live).not.toContain("<<<<<<<");
  expect((await readdir(join(fixture.config, "recovery"))).length).toBe(1);

  const conflictsBefore = skillsync(fixture, ["--json", "conflicts", "list"]).json;
  expect(conflictsBefore.count).toBe(1);
  const statePath = join(fixture.config, "state.json");
  const stateBefore = await readFile(statePath, "utf8");
  const state = JSON.parse(stateBefore);
  const relationship = Object.keys(state.subscriptions)[0]!;
  const shown = skillsync(fixture, ["--json", "conflicts", "show", relationship]);
  expect(shown.json.status).toBe("conflict");
  expect(shown.json.manifest_version).toBe(1);

  const identityTampered = structuredClone(state);
  identityTampered.subscriptions[relationship].baseline_source = "tampered-source";
  await writeFile(statePath, JSON.stringify(identityTampered));
  const identityRejected = skillsync(fixture, ["--json", "conflicts", "show", relationship], false);
  expect(identityRejected.json.ok).toBe(false);
  expect(identityRejected.json.message).toBe("subscription baseline source does not match source");
  expect(await readFile(statePath, "utf8")).toBe(JSON.stringify(identityTampered));
  expect(await readdir(join(fixture.config, "recovery"))).toHaveLength(1);
  expect(await readFile(join(fixture.library, "demo/SKILL.md"), "utf8")).toContain("local conflict");

  const legacyTampered = structuredClone(state);
  delete legacyTampered.subscriptions[relationship].baseline_source;
  delete legacyTampered.subscriptions[relationship].baseline_source_path;
  await writeFile(statePath, JSON.stringify(legacyTampered));
  const legacyRejected = skillsync(fixture, ["--json", "conflicts", "list"], false);
  expect(legacyRejected.json.ok).toBe(false);
  expect(legacyRejected.json.message).toBe("subscription baseline source is required");
  expect(await readFile(statePath, "utf8")).toBe(JSON.stringify(legacyTampered));
  expect(await readdir(join(fixture.config, "recovery"))).toHaveLength(1);

  await writeFile(statePath, stateBefore);
  await put(join(fixture.config, "baselines", relationship, "tampered.txt"), "tampered\n");
  const rejectedInventory = skillsync(fixture, ["--json", "conflicts", "list"], false);
  expect(rejectedInventory.json.ok).toBe(false);
  expect(rejectedInventory.json.message).toContain("baseline content");
  expect(await readFile(statePath, "utf8")).toBe(stateBefore);

  const destination = join(fixture.root, "destination.git");
  const seed = join(fixture.root, "destination-seed");
  checked(run("git", ["init", "--bare", destination], undefined, fixture.env), "init destination remote");
  checked(run("git", ["init", "-b", "trunk", seed], undefined, fixture.env), "init destination");
  await put(join(seed, "README.md"), "keep\n");
  await put(join(seed, "skills/publishable/.env"), "SECRET=keep\n");
  git(fixture, seed, ["add", "."]);
  git(fixture, seed, ["commit", "-m", "seed"]);
  git(fixture, seed, ["remote", "add", "origin", destination]);
  git(fixture, seed, ["push", "-u", "origin", "trunk"]);

  await put(join(fixture.library, "publishable/SKILL.md"), "name: publishable\nv1\n");
  await put(join(fixture.library, "publishable/references/old.md"), "old\n");
  const published = skillsync(fixture, ["--json", "publish", "publishable", "--repo", destination, "--yes"]).json;
  expect(published.status).toBe("published");
  const firstLog = git(fixture, seed, ["ls-remote", destination, "refs/heads/trunk"]).stdout;
  const destinationClone = join(fixture.root, "published-clone");
  checked(
    run("git", ["clone", "--quiet", "--branch", "trunk", destination, destinationClone], undefined, fixture.env),
    "clone published destination",
  );
  expect(await readFile(join(destinationClone, "README.md"), "utf8")).toBe("keep\n");
  expect(await readFile(join(destinationClone, "skills/publishable/.env"), "utf8")).toBe("SECRET=keep\n");
  expect(await readFile(join(destinationClone, "skills/publishable/references/old.md"), "utf8")).toBe("old\n");

  const repeated = skillsync(fixture, ["--json", "publish", "publishable", "--repo", destination, "--yes"]).json;
  expect(repeated.status).toBe("published");
  expect(git(fixture, seed, ["ls-remote", destination, "refs/heads/trunk"]).stdout).toBe(firstLog);

  await put(join(fixture.library, "publishable/SKILL.md"), "name: publishable\nv2\n");
  const synced = skillsync(fixture, ["--json", "sync"]).json;
  expect(synced.results.some((item: any) => item.skill === "publishable" && item.status === "published")).toBe(true);
  const updatedClone = join(fixture.root, "updated-clone");
  checked(
    run("git", ["clone", "--quiet", "--branch", "trunk", destination, updatedClone], undefined, fixture.env),
    "clone updated destination",
  );
  expect(await readFile(join(updatedClone, "skills/publishable/SKILL.md"), "utf8")).toBe(
    "name: publishable\nv2\n",
  );
  expect(await readFile(join(updatedClone, "skills/publishable/.env"), "utf8")).toBe("SECRET=keep\n");

  const secondDestination = join(fixture.root, "second-destination.git");
  checked(run("git", ["init", "--bare", secondDestination], undefined, fixture.env), "init second destination");
  skillsync(fixture, ["--json", "publish", "publishable", "--repo", secondDestination, "--yes"]);
  const statusWithTwo = skillsync(fixture, ["--json", "status"]).json;
  expect(countPublications(statusWithTwo)).toBe(2);
  skillsync(fixture, ["--json", "unpublish", "publishable", "--repo", destination]);
  const statusWithOne = skillsync(fixture, ["--json", "status"]).json;
  expect(countPublications(statusWithOne)).toBe(1);

  const unsubscribed = skillsync(fixture, ["--json", "unsubscribe", "demo"]).json;
  expect(unsubscribed.status).toBe("unsubscribed");
  expect(await Bun.file(join(fixture.library, "demo/SKILL.md")).exists()).toBe(true);
  const unpublished = skillsync(fixture, ["--json", "unpublish", "publishable", "--repo", secondDestination]).json;
  expect(unpublished.status).toBe("unpublished; destination retained");
  expect(await Bun.file(join(fixture.library, "publishable/SKILL.md")).exists()).toBe(true);

  const failed = skillsync(fixture, ["--json", "subscribe", upstream], false);
  expect(failed.json.ok).toBe(false);
}, { timeout: 30000 });

test("tampered persisted paths are rejected without reading outside state", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.subscriptions["../escape"] = {
    skill: "tampered",
    source: "/unreachable",
    branch: "trunk",
    source_path: ".",
    baseline_path: join(fixture.root, "outside-baseline"),
    baseline_hash: "deadbeef",
    baseline_source: "",
    baseline_source_path: ".",
    local_path: join(fixture.library, "tampered"),
    status: "synced",
    recovery_path: null,
    last_sync: 0,
    update_count: 0,
  };
  await writeFile(statePath, JSON.stringify(state));
  const failed = skillsync(fixture, ["--json", "update"], false);
  expect(failed.json.ok).toBe(false);
  expect(failed.json.message).toContain("relationship key");
  expect(await Bun.file(join(fixture.root, "outside-baseline")).exists()).toBe(false);
});

test("rejects a persisted library redirect", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.library = join(fixture.root, "outside-library");
  await writeFile(statePath, JSON.stringify(state));
  const failed = skillsync(fixture, ["--json", "status"], false);
  expect(failed.json.ok).toBe(false);
  expect(failed.json.message).toContain("persisted library");
  expect(await Bun.file(join(fixture.root, "outside-library")).exists()).toBe(false);
});

test("rejects symlinked persisted config and state files", async () => {
  if (process.platform === "win32") return;
  const configFixture = await makeFixture();
  skillsync(configFixture, ["--json", "init"]);
  const externalConfig = join(configFixture.root, "external-config.toml");
  await writeFile(externalConfig, `library = ${JSON.stringify(configFixture.library)}\n`);
  const configPath = join(configFixture.config, "config.toml");
  await unlink(configPath);
  await symlink(externalConfig, configPath);
  const configFailure = skillsync(configFixture, ["--json", "status"], false);
  expect(configFailure.json.message).toContain("symlink config");

  const stateFixture = await makeFixture();
  skillsync(stateFixture, ["--json", "init"]);
  const externalState = join(stateFixture.root, "external-state.json");
  await writeFile(externalState, "{}");
  const statePath = join(stateFixture.config, "state.json");
  await unlink(statePath);
  await symlink(externalState, statePath);
  const stateFailure = skillsync(stateFixture, ["--json", "status"], false);
  expect(stateFailure.json.message).toContain("symlink state");
});

test("rejects symlinked package content without copying it", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  const source = join(fixture.root, "source");
  const upstream = join(fixture.root, "upstream.git");
  const outside = join(fixture.root, "outside.txt");
  await writeFile(outside, "must not be copied\n");
  checked(run("git", ["init", "--bare", upstream], undefined, fixture.env), "init unsafe remote");
  checked(run("git", ["init", "-b", "trunk", source], undefined, fixture.env), "init unsafe source");
  await put(join(source, "skills/unsafe/SKILL.md"), "name: unsafe\n");
  await mkdir(join(source, "skills/unsafe/references"), { recursive: true });
  await symlink(outside, join(source, "skills/unsafe/references/outside.txt"));
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "unsafe package"]);
  git(fixture, source, ["remote", "add", "origin", upstream]);
  git(fixture, source, ["push", "-u", "origin", "trunk"]);
  skillsync(fixture, ["--json", "init"]);
  const failed = skillsync(
    fixture,
    ["--json", "subscribe", upstream, "--skill", "unsafe"],
    false,
  );
  expect(failed.json.ok).toBe(false);
  expect(await Bun.file(join(fixture.library, "unsafe/SKILL.md")).exists()).toBe(false);
  expect(await readFile(outside, "utf8")).toBe("must not be copied\n");
});

test("rejects special state locks for mutating and read-only paths", async () => {
  if (process.platform !== "linux") return;
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  const lock = join(fixture.config, "state.lock");
  await rm(lock);
  const created = Bun.spawnSync({ cmd: ["mkfifo", lock], stdout: "pipe", stderr: "pipe" });
  expect(created.exitCode).toBe(0);

  const mutating = skillsync(fixture, ["--json", "unsubscribe", "missing"], false);
  expect(mutating.json.ok).toBe(false);

  const readOnly = skillsync(fixture, ["--json", "conflicts", "list"], false);
  expect(readOnly.json.ok).toBe(false);
});

test("foreground worker owns the state lock and status reflects live ownership", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);

  const worker = Bun.spawn({
    cmd: [binary, "--json", "worker", "--interval", "30"],
    env: { ...inheritedEnv(), ...fixture.env },
    stdout: "pipe",
    stderr: "pipe",
  });

  try {
    let running = false;
    for (let attempt = 0; attempt < 100; attempt += 1) {
      const status = skillsync(fixture, ["--json", "status"]).json;
      if (status.worker === "running") {
        running = true;
        break;
      }
      await Bun.sleep(20);
    }
    expect(running).toBe(true);

    const blocked = skillsync(fixture, ["--json", "unsubscribe", "missing"], false);
    expect(blocked.json.ok).toBe(false);
    expect(blocked.json.message).toContain("state is busy");
  } finally {
    worker.kill();
    await worker.exited;
  }

  expect(skillsync(fixture, ["--json", "status"]).json.worker).toBe("stopped");
});

test("worker once isolates malformed relationships instead of aborting the cycle", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  for (const key of ["../bad-one", "../bad-two"]) {
    state.subscriptions[key] = {
      skill: "bad",
      source: "/unreachable",
      branch: "trunk",
      source_path: ".",
      baseline_path: join(fixture.config, "baselines", key),
      baseline_hash: "deadbeef",
      baseline_source: "",
      baseline_source_path: ".",
      local_path: join(fixture.library, "bad"),
      status: "synced",
      recovery_path: null,
      last_sync: 0,
      update_count: 0,
    };
  }
  await writeFile(statePath, JSON.stringify(state));

  const once = skillsync(fixture, ["--json", "worker", "--once"]).json;
  expect(once.worker).toBe("completed");
  expect(once.results).toHaveLength(2);
  expect(once.results.every((item: any) => item.status === "conflict")).toBe(true);
});

test("worker Ctrl-C terminates a blocked Git subprocess and releases ownership", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  await put(join(fixture.library, "demo/SKILL.md"), "name: demo\n");
  await put(join(fixture.config, "baselines/fixture-rel/SKILL.md"), "name: demo\n");
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.subscriptions["fixture-rel"] = {
    skill: "demo",
    source: "blocked-source",
    branch: "trunk",
    source_path: ".",
    baseline_path: join(fixture.config, "baselines/fixture-rel"),
    baseline_hash: "deadbeef",
    baseline_source: "",
    baseline_source_path: ".",
    local_path: join(fixture.library, "demo"),
    status: "synced",
    recovery_path: null,
    last_sync: 0,
    update_count: 0,
  };
  await writeFile(statePath, JSON.stringify(state));

  const fakeBin = join(fixture.root, "fake-bin");
  await mkdir(fakeBin, { recursive: true });
  const fakeGit = join(fakeBin, "git");
  await writeFile(fakeGit, "#!/bin/sh\nsleep 30\n");
  await chmod(fakeGit, 0o755);
  fixture.env.PATH = `${fakeBin}:${inheritedEnv().PATH ?? ""}`;

  const worker = Bun.spawn({
    cmd: [binary, "--json", "worker", "--interval", "30"],
    env: { ...inheritedEnv(), ...fixture.env },
    stdout: "pipe",
    stderr: "pipe",
  });
  try {
    let running = false;
    for (let attempt = 0; attempt < 100; attempt += 1) {
      const status = skillsync(fixture, ["--json", "status"]).json;
      if (status.worker === "running") {
        running = true;
        break;
      }
      await Bun.sleep(20);
    }
    expect(running).toBe(true);
    worker.kill("SIGINT");
    const exitCode = await Promise.race([
      worker.exited,
      Bun.sleep(3000).then(() => null),
    ]);
    expect(exitCode).not.toBeNull();
    const output = await new Response(worker.stdout).text();
    const result = JSON.parse(output);
    expect(result.worker).toBe("stopped");
    expect(result.cancelled).toBe(true);
  } finally {
    worker.kill();
    await worker.exited;
  }
  expect(skillsync(fixture, ["--json", "status"]).json.worker).toBe("stopped");
});

test("preserves executable mode on upstream package additions", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  const source = join(fixture.root, "mode-source");
  const upstream = join(fixture.root, "mode-upstream.git");
  checked(run("git", ["init", "--bare", upstream], undefined, fixture.env), "init mode remote");
  checked(run("git", ["init", "-b", "trunk", source], undefined, fixture.env), "init mode source");
  await put(join(source, "skills/mode/SKILL.md"), "name: mode\nbase\n");
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "initial mode package"]);
  git(fixture, source, ["remote", "add", "origin", upstream]);
  git(fixture, source, ["push", "-u", "origin", "trunk"]);
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "subscribe", upstream, "--skill", "mode"]);

  const script = join(source, "skills/mode/scripts/check.sh");
  await put(script, "#!/bin/sh\nexit 0\n");
  await chmod(script, 0o755);
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "add executable"]);
  git(fixture, source, ["push"]);
  skillsync(fixture, ["--json", "update"]);

  const installed = await stat(join(fixture.library, "mode/scripts/check.sh"));
  expect(installed.mode & 0o111).toBe(0o111);
});

test("persists publication intent before a failed push and retries it", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  await put(join(fixture.library, "journal/SKILL.md"), "name: journal\nv1\n");
  const destination = join(fixture.root, "journal-destination.git");
  checked(run("git", ["init", "--bare", destination], undefined, fixture.env), "init journal destination");
  const updateHook = join(destination, "hooks/update");
  await writeFile(updateHook, "#!/bin/sh\nexit 1\n");
  await chmod(updateHook, 0o755);

  const failed = skillsync(
    fixture,
    ["--json", "publish", "journal", "--repo", destination, "--yes"],
    false,
  );
  expect(failed.json.ok).toBe(false);
  const pending = skillsync(fixture, ["--json", "status"]).json;
  expect(Object.keys(pending.pending_publications ?? {})).toHaveLength(1);

  await unlink(updateHook);
  const retried = skillsync(fixture, ["--json", "sync"]).json;
  expect(retried.results.some((item: any) => item.skill === "journal" && item.status === "published")).toBe(true);
  const recovered = skillsync(fixture, ["--json", "status"]).json;
  expect(Object.keys(recovered.pending_publications ?? {})).toHaveLength(0);
  expect(countPublications(recovered)).toBe(1);
});

test("honors JSON mode for clap validation errors", async () => {
  const fixture = await makeFixture();
  const result = run(binary, ["--json", "publish"], undefined, fixture.env);
  expect(result.code).toBe(2);
  const json = JSON.parse(result.stdout);
  expect(json.ok).toBe(false);
  expect(json.message).toContain("Usage:");
});

test("preserves recovery for file-directory transitions", async () => {
  const fixture = await makeFixture();
  const source = join(fixture.root, "type-source");
  const upstream = join(fixture.root, "type-upstream.git");
  checked(run("git", ["init", "--bare", upstream], undefined, fixture.env), "init type remote");
  checked(run("git", ["init", "-b", "trunk", source], undefined, fixture.env), "init type source");
  await put(join(source, "skills/type/SKILL.md"), "name: type\nbase\n");
  await put(join(source, "skills/type/thing/child.txt"), "child\n");
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "initial type package"]);
  git(fixture, source, ["remote", "add", "origin", upstream]);
  git(fixture, source, ["push", "-u", "origin", "trunk"]);
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "subscribe", upstream, "--skill", "type"]);

  await rm(join(source, "skills/type/thing"), { recursive: true, force: true });
  await put(join(source, "skills/type/thing"), "upstream file\n");
  git(fixture, source, ["add", "-A"]);
  git(fixture, source, ["commit", "-m", "replace directory with file"]);
  git(fixture, source, ["push"]);
  const updated = skillsync(fixture, ["--json", "update"]).json;
  expect(updated.results.some((item: any) => item.skill === "type" && item.status === "conflict")).toBe(true);
  expect((await stat(join(fixture.library, "type/thing"))).isDirectory()).toBe(true);
  expect(await readFile(join(fixture.library, "type/thing/child.txt"), "utf8")).toBe("child\n");
  expect(await readdir(join(fixture.config, "recovery"))).toHaveLength(1);
});

test("manages portable named sets over canonical library skills", async () => {
  const fixture = await makeFixture();
  await put(join(fixture.library, "demo/SKILL.md"), "name: demo\n");
  skillsync(fixture, ["--json", "init"]);
  expect(skillsync(fixture, ["--json", "set", "create", "core"]).json.members).toEqual([]);
  expect(skillsync(fixture, ["--json", "set", "add", "core", "demo"]).json.status).toBe("added");
  expect(skillsync(fixture, ["--json", "set", "show", "core"]).json.members).toEqual(["demo"]);
  expect(skillsync(fixture, ["--json", "set", "remove", "core", "demo"]).json.status).toBe("removed");
  expect(skillsync(fixture, ["--json", "set", "show", "core"]).json.members).toEqual([]);
});

test("rejects symlinked library directories as set members", async () => {
  const fixture = await makeFixture();
  await put(join(fixture.library, "outside/SKILL.md"), "name: outside\n");
  skillsync(fixture, ["--json", "init"]);
  await symlink(join(fixture.library, "outside"), join(fixture.library, "link"), "dir");
  skillsync(fixture, ["--json", "set", "create", "core"]);
  const rejected = skillsync(fixture, ["--json", "set", "add", "core", "link"], false);
  expect(rejected.json.ok).toBe(false);
  expect(rejected.json.message).toMatch(/symlink|reparse point/);
});

test("rejects malformed persisted set members before listing or mutating", async () => {
  const fixture = await makeFixture();
  await put(join(fixture.library, "demo/SKILL.md"), "name: demo\n");
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "set", "create", "core"]);
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.sets.core.members = ["library:../outside"];
  await writeFile(statePath, JSON.stringify(state));
  const listed = skillsync(fixture, ["--json", "set", "list"], false);
  expect(listed.json.ok).toBe(false);
  expect(listed.json.message).toContain("invalid set member");
  const changed = skillsync(fixture, ["--json", "set", "add", "core", "demo"], false);
  expect(changed.json.ok).toBe(false);
  expect(changed.json.message).toContain("invalid set member");
});

test("rejects state written by a newer Skillsync schema", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.version = 999;
  await writeFile(statePath, JSON.stringify(state));
  const rejected = skillsync(fixture, ["--json", "status"], false);
  expect(rejected.json.ok).toBe(false);
  expect(rejected.json.message).toContain("unsupported state version");
});

test("makes set membership retries safe and removes stale members", async () => {
  const fixture = await makeFixture();
  await put(join(fixture.library, "demo/SKILL.md"), "name: demo\n");
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "set", "create", "core"]);
  expect(skillsync(fixture, ["--json", "set", "add", "core", "demo"]).json.status).toBe("added");
  expect(skillsync(fixture, ["--json", "set", "add", "core", "demo"]).json.status).toBe("already_present");
  await rm(join(fixture.library, "demo"), { recursive: true, force: true });
  expect(skillsync(fixture, ["--json", "set", "remove", "core", "demo"]).json.status).toBe("removed");
  expect(skillsync(fixture, ["--json", "set", "remove", "core", "demo"]).json.status).toBe("already_absent");
});

test("rejects Windows-invalid portable set identifiers", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  const reserved = skillsync(fixture, ["--json", "set", "create", "CON"], false);
  expect(reserved.json.ok).toBe(false);
  expect(reserved.json.message).toContain("invalid set name");
  const trailing = skillsync(fixture, ["--json", "set", "create", "name."], false);
  expect(trailing.json.ok).toBe(false);
  expect(trailing.json.message).toContain("invalid set name");
});

test("links one canonical skill into an explicit harness root and safely unlinks it", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  await put(join(fixture.library, "linked/SKILL.md"), "name: linked\nbase\n");
  const harness = join(fixture.root, "harness-skills");
  await mkdir(harness, { recursive: true });
  await put(join(harness, "bundled.txt"), "keep\n");
  skillsync(fixture, ["--json", "init"]);
  const linked = skillsync(fixture, ["--json", "harness", "link", "--root", harness, "--skill", "linked"]).json;
  expect(linked.status).toBe("linked");
  expect((await lstat(join(harness, "linked"))).isSymbolicLink()).toBe(true);
  await put(join(harness, "linked/references/learning.md"), "canonical\n");
  expect(await readFile(join(fixture.library, "linked/references/learning.md"), "utf8")).toBe("canonical\n");
  expect(await readFile(join(harness, "bundled.txt"), "utf8")).toBe("keep\n");
  const collision = skillsync(fixture, ["--json", "harness", "link", "--root", harness, "--skill", "linked"], false);
  expect(collision.json.ok).toBe(false);
  expect(collision.json.message).toContain("already exists");
  expect(Object.keys(skillsync(fixture, ["--json", "harness", "list"]).json.links)).toHaveLength(1);
  const unlinked = skillsync(fixture, ["--json", "harness", "unlink", "--root", harness, "--skill", "linked"]).json;
  expect(unlinked.canonical_retained).toBe(true);
  expect(await Bun.file(join(fixture.library, "linked/SKILL.md")).exists()).toBe(true);
  expect(await readFile(join(harness, "bundled.txt"), "utf8")).toBe("keep\n");
  const invalid = skillsync(fixture, ["--json", "harness", "link", "--root", join(fixture.root, "missing"), "--skill", "linked"], false);
  expect(invalid.json.ok).toBe(false);
});

test("reports missing explicit harness links and rejects tampered link state", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  await put(join(fixture.library, "linked/SKILL.md"), "name: linked\n");
  const harness = join(fixture.root, "harness-skills");
  await mkdir(harness, { recursive: true });
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "harness", "link", "--root", harness, "--skill", "linked"]);
  const healthy = skillsync(fixture, ["--json", "doctor"]).json;
  expect(healthy.harness_links[0].status).toBe("healthy");
  await unlink(join(harness, "linked"));
  const missing = skillsync(fixture, ["--json", "doctor"]).json;
  expect(missing.harness_links[0].status).toBe("missing");
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  const key = Object.keys(state.harness_links)[0];
  state.harness_links[key].link_path = join(fixture.root, "unrelated");
  await writeFile(statePath, JSON.stringify(state));
  const rejected = skillsync(fixture, ["--json", "status"], false);
  expect(rejected.json.ok).toBe(false);
  expect(rejected.json.message).toContain("harness link path does not match");
});

test("harness link collision is atomic and preserves the preexisting entry", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  await put(join(fixture.library, "collision/SKILL.md"), "name: collision\n");
  const harness = join(fixture.root, "collision-harness");
  await mkdir(harness, { recursive: true });
  await put(join(harness, "collision"), "unrelated\n");
  skillsync(fixture, ["--json", "init"]);
  const rejected = skillsync(
    fixture,
    ["--json", "harness", "link", "--root", harness, "--skill", "collision"],
    false,
  );
  expect(rejected.json.ok).toBe(false);
  expect(await readFile(join(harness, "collision"), "utf8")).toBe("unrelated\n");
  expect(Object.keys(skillsync(fixture, ["--json", "harness", "list"]).json.links)).toHaveLength(0);
});
