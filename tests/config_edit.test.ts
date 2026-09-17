import { expect, test } from "bun:test";
import { chmod, mkdtemp, mkdir, readFile, readdir, realpath, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { childEnv, commandBinary } from "./test_harness";

type Env = Record<string, string>;
type Result = { code: number; stdout: string; stderr: string };
type Fixture = { root: string; config: string; library: string; home: string; gitconfig: string; env: Env };

const binary = commandBinary();
const decoder = new TextDecoder();

function inheritedEnv(): Env {
  return Object.fromEntries(
    Object.entries(process.env).filter((entry): entry is [string, string] => entry[1] !== undefined),
  );
}

function run(args: string[], env: Env, cwd?: string): Result {
  const result = Bun.spawnSync({
    cmd: [binary, ...args],
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

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

function runInPty(args: string[], env: Env): Result {
  const command = [commandBinary(env), ...args].map(shellQuote).join(" ");
  const scriptCommand = process.platform === "darwin"
    ? ["python3", resolve(import.meta.dir, "pty_runner.py"), commandBinary(env), ...args]
    : ["script", "-qefc", command, "/dev/null"];
  const result = Bun.spawnSync({
    cmd: scriptCommand,
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

async function fixture(): Promise<Fixture> {
  const root = await mkdtemp(join(tmpdir(), "skillsync-config-edit-"));
  const home = join(root, "home");
  const config = join(root, "config");
  const library = join(root, "library");
  const gitconfig = join(root, "gitconfig");
  await mkdir(home, { recursive: true });
  await mkdir(library, { recursive: true });
  await writeFile(
    gitconfig,
    "[user]\n\tname = Skillsync Config Test\n\temail = skillsync@example.invalid\n",
  );
  return {
    root,
    config,
    library,
    home,
    gitconfig,
    env: {
      HOME: home,
      SKILLSYNC_CONFIG_DIR: config,
      SKILLSYNC_LIBRARY: library,
      GIT_CONFIG_GLOBAL: gitconfig,
      GIT_TERMINAL_PROMPT: "0",
    },
  };
}

async function editorScript(path: string): Promise<{ log: string; marker: string }> {
  const log = join(dirname(path), "editor-args.log");
  const marker = join(dirname(path), "editor-ran");
  await writeFile(
    path,
    `#!/bin/sh\nprintf '%s\\n' "$#" > "$SKILLSYNC_EDITOR_LOG"\nprintf '%s\\n' "$1" >> "$SKILLSYNC_EDITOR_LOG"\nprintf '%s\\n' "$1" > "$SKILLSYNC_EDITOR_MARKER"\nprintf 'library = \\"%s\\"\\n' "$SKILLSYNC_EDITOR_LIBRARY" > "$1"\nprintf 'editor-stdout\\n'\nprintf 'editor-stderr\\n' >&2\n`,
  );
  await chmod(path, 0o700);
  return { log, marker };
}

test("config edit refuses JSON and redirected output before launching an editor", async () => {
  const f = await fixture();
  const editor = join(f.root, "editor.sh");
  const { marker } = await editorScript(editor);
  const initialized = run(["--json", "init"], f.env);
  expect(initialized.code).toBe(0);

  const json = run(["--json", "config", "edit"], {
    ...f.env,
    VISUAL: editor,
    EDITOR: editor,
    SKILLSYNC_EDITOR_LOG: join(f.root, "json.log"),
    SKILLSYNC_EDITOR_MARKER: marker,
    SKILLSYNC_EDITOR_LIBRARY: f.library,
  });
  expect(json.code).toBe(1);
  expect(JSON.parse(json.stdout).message).toContain("cannot run with --json");
  expect(await Bun.file(marker).exists()).toBe(false);

  const redirected = run(["config", "edit"], {
    ...f.env,
    VISUAL: editor,
    EDITOR: editor,
    SKILLSYNC_EDITOR_LOG: join(f.root, "redirected.log"),
    SKILLSYNC_EDITOR_MARKER: marker,
    SKILLSYNC_EDITOR_LIBRARY: f.library,
  });
  expect(redirected.code).toBe(1);
  expect(redirected.stdout).toContain("config edit requires an interactive terminal");
  expect(await Bun.file(marker).exists()).toBe(false);
});

test("config edit validates the effective environment library, not an overridden file library", async () => {
  if (process.platform === "win32") return;
  const f = await fixture();
  const editor = join(f.root, "override.sh");
  await writeFile(editor, `#!/bin/sh\nprintf 'library = \\"/definitely/missing\\"\\n' > "$1"\n`);
  await chmod(editor, 0o700);
  const result = runInPty(["config", "edit"], { ...f.env, VISUAL: editor, EDITOR: "/bin/false" });
  expect(result.code).toBe(0);
  expect(await readFile(join(f.config, "config.toml"), "utf8")).toContain("/definitely/missing");
});

test("config edit creates the file and invokes VISUAL with one config path", async () => {
  if (process.platform === "win32") return;
  const f = await fixture();
  const editor = join(f.root, "editor.sh");
  const { log, marker } = await editorScript(editor);
  const result = runInPty(["config", "edit"], {
    ...f.env,
    VISUAL: editor,
    EDITOR: "/bin/false",
    SKILLSYNC_EDITOR_LOG: log,
    SKILLSYNC_EDITOR_MARKER: marker,
    SKILLSYNC_EDITOR_LIBRARY: f.library,
  });
  expect(result.code, `${result.stdout}\n${result.stderr}`).toBe(0);
  expect(result.stdout).toContain("editor-stdout");
  expect(result.stdout).toContain("ok");
  const editedPath = (await readFile(log, "utf8")).split("\n")[1];
  expect(editedPath.startsWith(`${await realpath(f.config)}/`)).toBe(true);
  expect(editedPath).not.toBe(join(f.config, "config.toml"));
  expect(await readFile(join(f.config, "config.toml"), "utf8")).toBe(
    `library = "${f.library}"\n`,
  );
});

test("config edit refuses to replace a config created while editing", async () => {
  if (process.platform === "win32") return;
  const f = await fixture();
  const editor = join(f.root, "race.sh");
  await writeFile(editor, "#!/bin/sh\nprintf 'external = true\\n' > \"$SKILLSYNC_CONFIG_PATH\"\nprintf 'edited = true\\n' > \"$1\"\n");
  await chmod(editor, 0o700);
  const result = runInPty(["config", "edit"], { ...f.env, VISUAL: editor, SKILLSYNC_CONFIG_PATH: join(f.config, "config.toml") });
  expect(result.code).toBe(1);
  expect(await readFile(join(f.config, "config.toml"), "utf8")).toBe("external = true\n");
});

test("config edit rejects an editor-replaced temporary path", async () => {
  if (process.platform === "win32") return;
  const f = await fixture();
  const outside = join(f.root, "outside.toml");
  await writeFile(outside, "outside = true\n");
  const editor = join(f.root, "symlink.sh");
  await writeFile(editor, "#!/bin/sh\nrm -f \"$1\"\nln -s \"$SKILLSYNC_OUTSIDE\" \"$1\"\n");
  await chmod(editor, 0o700);
  const result = runInPty(["config", "edit"], { ...f.env, VISUAL: editor, SKILLSYNC_OUTSIDE: outside });
  expect(result.code).toBe(1);
  expect(await readFile(outside, "utf8")).toBe("outside = true\n");
  expect(await Bun.file(join(f.config, "config.toml")).exists()).toBe(false);
});

test("config edit rejects an editor-replaced temporary regular file", async () => {
  if (process.platform === "win32") return;
  const f = await fixture();
  const editor = join(f.root, "regular-replace.sh");
  await writeFile(editor, "#!/bin/sh\nrm -f \"$1\"\nprintf 'library = \\\"%s\\\"\\n' \"$SKILLSYNC_LIBRARY\" > \"$1\"\n");
  await chmod(editor, 0o700);
  const result = runInPty(["config", "edit"], { ...f.env, VISUAL: editor });
  expect(result.code).toBe(1);
  expect((await readdir(f.config)).filter((name) => name.includes(".config.toml.edit-")).length).toBe(1);
});
test("restores an existing config after publication failure and cleans temps", async () => {
  if (process.platform === "win32") return;
  const f = await fixture();
  await mkdir(f.config, { recursive: true });
  const original = `library = "${f.library}"\n`;
  await writeFile(join(f.config, "config.toml"), original);
  const editor = join(f.root, "failure.sh");
  await writeFile(editor, `#!/bin/sh\nprintf 'library = \\\"changed\\\"\\n' > "$1"\n`);
  await chmod(editor, 0o700);
  const result = runInPty(["config", "edit"], { ...f.env, VISUAL: editor, SKILLSYNC_TEST_CONFIG_PUBLISH_FAILURE: "1" });
  expect(result.code).toBe(1);
  expect(await readFile(join(f.config, "config.toml"), "utf8")).toBe(original);
  expect((await readdir(f.config)).filter((name) => name.includes(".config.toml.edit-")).length).toBe(0);
});
test("preserves a replacement of an owned temp during later failure", async () => {
  if (process.platform === "win32") return;
  const f = await fixture();
  const replacement = join(f.root, "replacement");
  await writeFile(replacement, "must survive\n");
  const editor = join(f.root, "replace-temp.sh");
  await writeFile(editor, `#!/bin/sh\nrm -f "$1"\ncp "$SKILLSYNC_REPLACEMENT" "$1"\nexit 1\n`);
  await chmod(editor, 0o700);
  const result = runInPty(["config", "edit"], { ...f.env, VISUAL: editor, SKILLSYNC_REPLACEMENT: replacement });
  expect(result.code).toBe(1);
  expect((await readdir(f.config)).filter((name) => name.includes(".config.toml.edit-")).length).toBe(1);
  const temp = (await readdir(f.config)).find((name) => name.includes(".config.toml.edit-"))!;
  expect(await readFile(join(f.config, temp), "utf8")).toBe("must survive\n");
});
test("config edit reports editor failure without shell fallback", async () => {
  if (process.platform === "win32") return;
  const f = await fixture();
  const result = runInPty(["config", "edit"], {
    ...f.env,
    VISUAL: "/bin/false",
    EDITOR: "/bin/true",
  });
  expect(result.code).toBe(1);
  expect(result.stdout).toContain(process.platform === "darwin" ? "launch editor /bin/false" : "editor exited unsuccessfully");
});

test("config edit reports a missing editor without mutating through a command string", async () => {
  if (process.platform === "win32") return;
  const f = await fixture();
  const result = runInPty(["config", "edit"], {
    ...f.env,
    VISUAL: "",
    EDITOR: "",
  });
  expect(result.code).toBe(1);
  expect(result.stdout).toContain("no editor configured; set VISUAL or EDITOR");
});

test("config edit fails closed when the validated directory is replaced during the editor", async () => {
  if (process.platform !== "linux") return;
  const f = await fixture();
  await mkdir(f.config, { recursive: true });
  await writeFile(join(f.config, "config.toml"), `library = "${f.library}"\n`);
  run(["--json", "init"], f.env);
  const editor = join(f.root, "blocking-race.sh");
  const started = join(f.root, "editor-started");
  await writeFile(editor, `#!/bin/sh\ntouch "$SKILLSYNC_STARTED"\nsleep 1\nprintf 'library = "attacker"\\n' > "$1"\n`);
  await chmod(editor, 0o700);
  const command = [binary, "config", "edit"].map(shellQuote).join(" ");
  const child = Bun.spawn(["script", "-qefc", command, "/dev/null"], { env: { ...inheritedEnv(), ...f.env, VISUAL: editor, SKILLSYNC_STARTED: started }, stdout: "pipe", stderr: "pipe" });
  try {
    for (let i = 0; i < 100 && !(await Bun.file(started).exists()); i++) await Bun.sleep(10);
    expect(await Bun.file(started).exists()).toBe(true);
    const replacement = join(f.root, "replacement-config");
    await mkdir(replacement, { recursive: true });
    await writeFile(join(replacement, "config.toml"), "attacker = true\\n");
    await rename(f.config, join(f.root, "original-config"));
    await rename(replacement, f.config);
    expect(await child.exited).not.toBe(0);
    expect(await readFile(join(f.config, "config.toml"), "utf8")).toBe("attacker = true\\n");
    expect((await readdir(f.config)).filter((name) => name.includes(".config.toml.edit-")).length).toBe(1);
  } finally {
    child.kill();
    await child.exited;
    await rm(join(f.root, "original-config"), { recursive: true, force: true });
  }
});
