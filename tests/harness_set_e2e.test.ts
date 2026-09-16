import { expect, test } from "bun:test";
import { lstat, mkdir, mkdtemp, readFile, rm, symlink, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";

const bin = process.env.SKILLSYNC_BIN ?? resolve(import.meta.dir, "../target/debug/skillsync");
const dec = new TextDecoder();
type F = { root: string; config: string; library: string; env: Record<string,string> };
async function put(p: string, s: string) { await mkdir(dirname(p), {recursive:true}); await writeFile(p,s); }
async function fixture(): Promise<F> { const root=await mkdtemp(join(tmpdir(),"skillsync-set-")); const f={root,config:join(root,"config"),library:join(root,"library"),env:{...process.env as Record<string,string>,HOME:join(root,"home"),SKILLSYNC_CONFIG_DIR:join(root,"config"),SKILLSYNC_LIBRARY:join(root,"library")}}; await mkdir(join(root,"home"),{recursive:true}); return f; }
function run(f:F,args:string[],ok=true,extra:Record<string,string>={}) { const r=Bun.spawnSync({cmd:[bin,...args],env:{...f.env,...extra},stdout:"pipe",stderr:"pipe"}); const out=dec.decode(r.stdout); expect(r.exitCode,dec.decode(r.stderr)).toBe(ok?0:1); expect(out).not.toBe(""); return {r,json:JSON.parse(out)}; }
async function setup() { const f=await fixture(); for (const [n,c] of [["one","one\n"],["two","two\n"]]) await put(join(f.library,n,"SKILL.md"),`name: ${n}\n${c}`); const h=join(f.root,"harness"); await mkdir(h,{recursive:true}); await put(join(h,"bundled.txt"),"keep\n"); run(f,["--json","init"]); run(f,["--json","set","create","core"]); run(f,["--json","set","add","core","one"]); run(f,["--json","set","add","core","two"]); return {f,h}; }

test("allows empty set enablement across reload, idempotence, and disable",async()=>{
  const f=await fixture();
  const h=join(f.root,"harness");
  await mkdir(h,{recursive:true});
  await put(join(h,"unrelated.txt"),"keep\n");
  run(f,["--json","init"]);
  run(f,["--json","set","create","empty"]);
  const before=await readFile(join(h,"unrelated.txt"),"utf8");
  expect(run(f,["--json","harness","enable","--root",h,"--set","empty"]).json.status).toBe("enabled");
  const state=JSON.parse(await readFile(join(f.config,"state.json"),"utf8"));
  expect((Object.values(state.harness_sets) as any[])[0].members).toEqual([]);
  expect(run(f,["--json","status"]).json.harness_health.some((x:any)=>x.status==="healthy")).toBe(true);
  expect(run(f,["--json","doctor"]).json.harness_enablement).toContain("supported");
  expect(run(f,["--json","harness","enable","--root",h,"--set","empty"]).json.status).toBe("already_enabled");
  expect(await readFile(join(h,"unrelated.txt"),"utf8")).toBe(before);
  expect(await readFile(join(f.config,"state.json"),"utf8")).toContain("harness_sets");
  expect(run(f,["--json","harness","disable","--root",h,"--set","empty"]).json.status).toBe("disabled");
  expect(await readFile(join(h,"unrelated.txt"),"utf8")).toBe(before);
  expect((await readFile(join(f.config,"state.json"),"utf8"))).not.toContain("hset-");
});

test("enables a two-member set with native write-back links and idempotence",async()=>{const {f,h}=await setup(); const e=run(f,["--json","harness","enable","--root",h,"--set","core"]).json; expect(e.status).toBe("enabled"); expect((await lstat(join(h,"one"))).isSymbolicLink()).toBe(true); expect((await lstat(join(h,"two"))).isSymbolicLink()).toBe(true); await put(join(h,"one","references","learn.md"),"canonical\n"); expect(await readFile(join(f.library,"one","references","learn.md"),"utf8")).toBe("canonical\n"); expect(await readFile(join(h,"bundled.txt"),"utf8")).toBe("keep\n"); expect(run(f,["--json","harness","enable","--root",h,"--set","core"]).json.status).toBe("already_enabled");});

test("fails closed when a recorded member link is missing or replaced",async()=>{
  for (const mode of ["missing", "wrong-target"]) {
    const {f,h}=await setup();
    run(f,["--json","harness","enable","--root",h,"--set","core"]);
    await put(join(h,"unrelated.txt"),"keep\n");
    await unlink(join(h,"two"));
    if (mode === "wrong-target") await symlink(join(f.library,"one"),join(h,"two"),"dir");
    const x=run(f,["--json","harness","enable","--root",h,"--set","core"],false);
    expect(x.json.ok).toBe(false);
    expect(await readFile(join(h,"unrelated.txt"),"utf8")).toBe("keep\n");
    if (mode === "wrong-target") {
      expect((await lstat(join(h,"two"))).isSymbolicLink()).toBe(true);
    } else {
      expect(await Bun.file(join(h,"two")).exists()).toBe(false);
    }
    expect((await readFile(join(f.config,"state.json"),"utf8"))).toContain("harness_sets");
  }
});

test("rejects independent member links without adopting or removing them",async()=>{const {f,h}=await setup(); run(f,["--json","harness","link","--root",h,"--skill","one"]); const x=run(f,["--json","harness","enable","--root",h,"--set","core"],false); expect(x.json.ok).toBe(false); expect((await lstat(join(h,"one"))).isSymbolicLink()).toBe(true); expect(await Bun.file(join(h,"two")).exists()).toBe(false); const state=JSON.parse(await readFile(join(f.config,"state.json"),"utf8")); expect(state.harness_sets ?? {}).toEqual({}); const y=run(f,["--json","harness","disable","--root",h,"--set","core"],false); expect(y.json.ok).toBe(false); expect((await lstat(join(h,"one"))).isSymbolicLink()).toBe(true);});

test("preflights collisions without partial links",async()=>{const {f,h}=await setup(); await put(join(h,"two"),"unrelated\n"); const x=run(f,["--json","harness","enable","--root",h,"--set","core"],false); expect(x.json.ok).toBe(false); expect(await Bun.file(join(h,"one")).exists()).toBe(false); expect(await readFile(join(h,"two"),"utf8")).toBe("unrelated\n");});

test("rejects stale per-skill link records before new set expansion",async()=>{const {f,h}=await setup(); run(f,["--json","harness","link","--root",h,"--skill","one"]); const p=join(f.config,"state.json"); const before=JSON.parse(await readFile(p,"utf8")); const oneKey=Object.keys(before.harness_links).find(k=>before.harness_links[k].skill==="one")!; const stale=before.harness_links[oneKey]; await unlink(join(h,"one")); const x=run(f,["--json","harness","enable","--root",h,"--set","core"],false); expect(x.json.ok).toBe(false); expect(await Bun.file(join(h,"one")).exists()).toBe(false); expect(await Bun.file(join(h,"two")).exists()).toBe(false); const after=JSON.parse(await readFile(p,"utf8")); expect(after.harness_sets??{}).toEqual({}); expect(after.harness_links[oneKey]).toEqual(stale);});

test("rejects tampered set state and malformed members",async()=>{const {f}=await setup(); const p=join(f.config,"state.json"); const s=JSON.parse(await readFile(p,"utf8")); s.sets.core.members = ["library:../escape"]; await writeFile(p,JSON.stringify(s)); const x=run(f,["--json","status"],false); expect(x.json.ok).toBe(false); expect(x.json.message).toContain("invalid set member");});

test("valid harness-set state loads and tampered records fail closed",async()=>{
  for (const tamper of ["key","root","members","missing-link","mismatched-link"]) {
    const {f,h}=await setup(); run(f,["--json","harness","enable","--root",h,"--set","core"]);
    const p=join(f.config,"state.json"); const s=JSON.parse(await readFile(p,"utf8"));
    const key=Object.keys(s.harness_sets)[0]; const rec=s.harness_sets[key];
    const oneKey=Object.keys(s.harness_links).find(k=>s.harness_links[k].skill==="one")!;
    if (tamper === "key") { s.harness_sets["hset-tampered"]=rec; delete s.harness_sets[key]; }
    if (tamper === "root") rec.harness_root=join(f.root,"other");
    if (tamper === "members") rec.members=["library:../escape"];
    if (tamper === "missing-link") delete s.harness_links[oneKey];
    if (tamper === "mismatched-link") s.harness_links[oneKey].skill="two";
    await writeFile(p,JSON.stringify(s));
    const x=run(f,["--json","status"],false); expect(x.json.ok).toBe(false);
  }
});

test("one-time expansion survives later set membership changes",async()=>{const {f,h}=await setup(); run(f,["--json","set","remove","core","two"]); run(f,["--json","harness","enable","--root",h,"--set","core"]); await put(join(h,"unrelated.txt"),"keep\n"); run(f,["--json","set","add","core","two"]); expect(run(f,["--json","status"]).json.harness_sets).toBeDefined(); expect(await Bun.file(join(h,"two")).exists()).toBe(false); expect((await lstat(join(h,"one"))).isSymbolicLink()).toBe(true); const d=run(f,["--json","harness","disable","--root",h,"--set","core"]).json; expect(d.status).toBe("disabled"); expect(await Bun.file(join(h,"one")).exists()).toBe(false); expect(await Bun.file(join(h,"two")).exists()).toBe(false); expect(await readFile(join(h,"unrelated.txt"),"utf8")).toBe("keep\n"); expect(await Bun.file(join(f.library,"one","SKILL.md")).exists()).toBe(true); expect(await Bun.file(join(f.library,"two","SKILL.md")).exists()).toBe(true);});

test("disables only recorded links, retains packages, and is atomic on wrong target",async()=>{const {f,h}=await setup(); run(f,["--json","harness","enable","--root",h,"--set","core"]); await unlink(join(h,"two")); await symlink(join(f.library,"one"),join(h,"two"),"dir"); const x=run(f,["--json","harness","disable","--root",h,"--set","core"],false); expect(x.json.ok).toBe(false); expect(await Bun.file(join(f.library,"one","SKILL.md")).exists()).toBe(true); expect(await Bun.file(join(f.library,"two","SKILL.md")).exists()).toBe(true); await unlink(join(h,"two")); expect(await Bun.file(join(f.library,"two","SKILL.md")).exists()).toBe(true);});

test("rolls back links on deterministic mid-loop disable failure",async()=>{const {f,h}=await setup(); run(f,["--json","harness","enable","--root",h,"--set","core"]); await put(join(h,"unrelated.txt"),"keep\n"); const before=await readFile(join(f.config,"state.json"),"utf8"); const x=run(f,["--json","harness","disable","--root",h,"--set","core"],false,{SKILLSYNC_TEST_FAIL_REMOVE_MEMBER:"two"}); expect(x.json.ok).toBe(false); expect((await lstat(join(h,"one"))).isSymbolicLink()).toBe(true); expect((await lstat(join(h,"two"))).isSymbolicLink()).toBe(true); expect(await readFile(join(h,"unrelated.txt"),"utf8")).toBe("keep\n"); expect(await readFile(join(f.config,"state.json"),"utf8")).toBe(before);});

test("loads degraded harness-set state after its root disappears",async()=>{const {f,h}=await setup(); run(f,["--json","harness","enable","--root",h,"--set","core"]); const stateBefore=await readFile(join(f.config,"state.json"),"utf8"); await rm(h,{recursive:true,force:true}); const status=run(f,["--json","status"]).json; expect(status.harness_health.some((x:any)=>x.status==="missing_root")).toBe(true); const doctor=run(f,["--json","doctor"]).json; expect(doctor.harness_links.some((x:any)=>x.status==="missing_root")).toBe(true); const disabled=run(f,["--json","harness","disable","--root",h,"--set","core"],false); expect(disabled.json.ok).toBe(false); expect(await readFile(join(f.config,"state.json"),"utf8")).toBe(stateBefore); expect(await Bun.file(join(f.library,"one","SKILL.md")).exists()).toBe(true);});
