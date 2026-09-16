# Skillsync

Local-first Rust CLI for importing agent skill packages, preserving local edits during upstream updates, and publishing selected packages to Git repositories.

## Commands

```sh
skillsync init [--library PATH]
skillsync config path
skillsync subscribe REPOSITORY [--skill NAME]
# Without --skill, a TTY offers a line-oriented single-select picker.
# With neither argument, the TTY asks for the repository first.
skillsync import --from PATH --skill NAME
skillsync update | sync | status | diff | doctor
skillsync worker --once
skillsync worker --interval 300
skillsync publish NAME --repo REPOSITORY --yes   # --dry-run previews
skillsync unsubscribe NAME
skillsync unpublish NAME --repo REPOSITORY
skillsync delete NAME --yes
skillsync set create NAME | list | show NAME | add NAME SKILL | remove NAME SKILL
skillsync harness link --root PATH --skill NAME
skillsync harness unlink --root PATH --skill NAME
skillsync harness list
```

Subscribe records a stable source-plus-relative-path relationship key, so same-named skills from different repositories do not overwrite state. `import --from PATH --skill NAME` explicitly adopts one package from an existing local directory (including a root package or nested package), stages and validates it, preserves modes/resources, and records local provenance without creating a subscription. The picker is deliberately not a full TUI: it lists discovered packages and accepts one validated number. Multi-select, unattended onboarding, automatic registry discovery, and `full TUI: unsupported` remain explicit limitations.

## Safety and storage

State loads strictly validate every local adoption record (identity, absolute no-follow source, canonical library destination, hash, and status); tampered records are rejected before status, doctor, or mutation. `SKILL.md` must begin with exactly one unquoted `name: <safe-component>` line; prose or duplicate/conflicting names are rejected. Imports stage content and roll back the newly installed package if provenance state cannot be persisted. Skill names and source-relative package paths reject absolute paths, parent traversal, separators where a package key is required, and control characters. Symlinks are rejected; regular-file scans use no-follow opens on Unix, and live package/publication replacements are staged and swapped through an anchored directory rename. Managed snapshots are rechecked before commit or replacement and abort when a concurrent change is detected; import installation uses an anchored no-replace primitive (Linux `renameat2`, Windows native no-overwrite move) and fails closed where unavailable. External writers are not fully serialized. Bundled files are never executed. `SKILLSYNC_TEST_FAIL_STATE_SAVE=1` is a test-only failure-injection hook for exercising rollback; it is not a configuration or recovery bypass.

Package copies exclude operational files such as `.env`, logs, credential directories, and private keys. Publication updates managed files in `skills/<name>` without deleting the destination package wholesale, preserving unrelated operational files. Destination changes that cannot be attributed to the recorded publication are rejected. Publication records are written only after a meaningful commit and successful push; a durable pending-publication intent is written before each push so a failed push or post-push state write can be retried without losing the relationship. Mutating commands share a persistent advisory lock in the config directory; the lock is held for a whole operation or worker run, and status probes the lock rather than trusting a stale PID.

Updates compare the durable baseline, live package, and fetched upstream package. Upstream-only additions (including nested directories) are applied; local-only edits remain; clean non-overlapping text changes merge. Conflicts, including unsupported file/directory type transitions, retain live content and write local/incoming recovery copies without placing conflict markers in the live package. Failed network, branch, merge, or push operations retain local content and report a non-synced status.

## Testing

Build the native binary, then run the TypeScript integration suite with Bun:

```sh
cargo build
bun install
bun test
bunx tsc --noEmit
```

The Bun suite uses isolated temporary Git repositories and configuration roots. It covers nested selection, default-branch checkout, local/upstream merge preservation, conflict recovery, scoped publication, operational-file retention, multiple publication destinations, unsubscribe/unpublish retention, foreground worker locking, JSON failures, and tampered state paths.

## Honest limits

The explicit harness integration is deliberately narrow: `harness link` creates one native directory symlink from a caller-supplied existing harness skill root to one canonical library skill, and edits through that link write back to the canonical package. `harness unlink` removes only a recorded link whose target still resolves to the expected canonical skill; canonical content is retained. Existing unrelated harness files are preserved and collisions, symlink/reparse roots, and unsafe names are rejected. `harness list`, `status`, and `doctor` report these relationships. Skillsync does not currently discover harnesses automatically, filter harness skills, reload running harnesses, curate Hermes autonomous learning, or promise universal harness compatibility; there is no silent copy fallback. Windows uses native directory symlinks and reports privilege/API failures. Windows directory-link and import runtime behavior is compile-checked in this environment but not runtime exercised; native Windows verification is not claimed.

The foreground worker is supported for `worker --once` and `worker --interval SECONDS`; it owns the same advisory state lock as mutating CLI commands. Named local sets are supported for organizing canonical library skills (`set create/list/show/add/remove`); membership is portable state only. Set publication, set subscription metadata, personal-library sync, and membership-change propagation remain unsupported. Durable sign-in startup registration, a full TUI, registry integration, Hermes autonomous curation, and automatic harness discovery/filtering/reload remain unsupported. Semantic conflict resolution, source-specific authentication, and cross-device worker coordination remain unsupported. `unsubscribe` and `unpublish` remove only the relationship and retain installed/published content. `delete NAME --yes` is the only canonical-library deletion surface: it fails closed for active subscriptions, publications, harness links, or set membership, stages a complete recoverable package snapshot under `recovery/`, then removes only that package. Safe no-replace directory quarantine is verified on Linux (`renameat2`) and Windows (native no-overwrite move); on other Unix platforms deletion fails closed because this runtime has no portable atomic no-replace directory primitive. Set membership is intentionally not auto-removed; remove the skill from every set first. Repeated or missing deletion is reported as `already_absent` and never affects unrelated paths.

## Private npm launcher plumbing

`npm/` contains a private, development-only npm launcher package. Its `skillsync` bin forwards arguments and standard streams to a bundled platform/architecture native binary, or to the explicit `SKILLSYNC_NATIVE_BIN` override used by tests and deployments. It never searches `PATH` or an npx cache, downloads artifacts, installs a worker, or registers startup. Missing artifacts fail clearly.

This is launcher plumbing only, not a published `skillsync` npm distribution. Package ownership, release publication, and the precompiled native artifact matrix remain unresolved; no native artifacts are bundled in this repository. Durable installation and sign-in startup registration remain unsupported.