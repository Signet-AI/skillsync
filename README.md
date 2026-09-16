# Skillsync

Local-first Rust CLI for importing agent skill packages, preserving local edits during upstream updates, and publishing selected packages to Git repositories.

## Commands

```sh
skillsync init [--library PATH]
skillsync config path
skillsync subscribe REPOSITORY --skill NAME
skillsync update | sync | status | diff | doctor
skillsync publish NAME --repo REPOSITORY --yes   # --dry-run previews
skillsync unsubscribe NAME
skillsync unpublish NAME --repo REPOSITORY
```

Use `--json` for structured success and error envelopes. GitHub `owner/repo`, local paths, HTTPS, SSH, and scp-style SSH sources are accepted. Subscribe records a stable source-plus-relative-path relationship key, so same-named skills from different repositories do not overwrite state. Persisted subscription branches are checked out explicitly during updates.

## Safety and storage

Configuration, baselines, recovery copies, and state live outside the library. `SKILLSYNC_CONFIG_DIR` and `SKILLSYNC_LIBRARY` are useful for isolated runs. State is written through a temporary file and rename. Skill names and source-relative package paths reject absolute paths, parent traversal, separators where a package key is required, and control characters. Symlinks are rejected; regular-file scans use no-follow opens on Unix, and live package/publication replacements are staged and swapped through an anchored directory rename. Bundled files are never executed.

Package copies exclude operational files such as `.env`, logs, credential directories, and private keys. Publication updates managed files in `skills/<name>` without deleting the destination package wholesale, preserving unrelated operational files. Destination changes that cannot be attributed to the recorded publication are rejected. Publication records are written only after a meaningful commit and successful push.

Updates compare the durable baseline, live package, and fetched upstream package. Upstream-only additions (including nested directories) are applied; local-only edits remain; clean non-overlapping text changes merge. Conflicts retain live content and write local/incoming recovery copies without placing conflict markers in the live package. Failed network, branch, merge, or push operations retain local content and report a non-synced status.

## Testing

Build the native binary, then run the TypeScript integration suite with Bun:

```sh
cargo build
bun install
bun test
bunx tsc --noEmit
```

The Bun suite uses isolated temporary Git repositories and configuration roots. It covers nested selection, default-branch checkout, local/upstream merge preservation, conflict recovery, scoped publication, operational-file retention, multiple publication destinations, unsubscribe/unpublish retention, JSON failures, and tampered state paths.

## Honest limits

Update/sync and publication are explicit CLI operations; there is no background worker, TUI, registry integration, Hermes autonomous curation, harness write-back, sets, or personal-library sync. Semantic conflict resolution, source-specific authentication, and cross-device worker coordination remain unsupported. `unsubscribe` and `unpublish` remove only the relationship and retain installed/published content.