# Skillsync

Local-first Rust CLI for importing agent skill packages, preserving local edits during upstream updates, and publishing selected packages to Git repositories.

## Commands

```sh
skillsync init [--library PATH]
skillsync config path
skillsync subscribe REPOSITORY --skill NAME
skillsync update | sync | status | diff | doctor
skillsync worker --once
skillsync worker --interval 300
skillsync publish NAME --repo REPOSITORY --yes   # --dry-run previews
skillsync unsubscribe NAME
skillsync unpublish NAME --repo REPOSITORY
```

Use `--json` for structured success and error envelopes. GitHub `owner/repo`, local paths, HTTPS, SSH, and scp-style SSH sources are accepted. Subscribe records a stable source-plus-relative-path relationship key, so same-named skills from different repositories do not overwrite state. Persisted subscription branches are checked out explicitly during updates.

## Safety and storage

Configuration, baselines, recovery copies, and state live outside the library. `SKILLSYNC_CONFIG_DIR` and `SKILLSYNC_LIBRARY` are useful for isolated runs. State is written through a temporary file and rename. Skill names and source-relative package paths reject absolute paths, parent traversal, separators where a package key is required, and control characters. Symlinks are rejected; regular-file scans use no-follow opens on Unix, and live package/publication replacements are staged and swapped through an anchored directory rename. Managed snapshots are rechecked before commit or replacement and abort when a concurrent change is detected; arbitrary hostile external filesystem writers are not fully serialized. Bundled files are never executed.

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

The foreground worker is supported for `worker --once` and `worker --interval SECONDS`; it owns the same advisory state lock as mutating CLI commands. Durable sign-in startup registration, a full TUI, registry integration, Hermes autonomous curation, harness write-back, sets, and personal-library sync remain unsupported. Semantic conflict resolution, source-specific authentication, and cross-device worker coordination remain unsupported. `unsubscribe` and `unpublish` remove only the relationship and retain installed/published content.