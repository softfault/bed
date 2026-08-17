# Releasing bed

Release notes are a concise, user-facing summary of the corresponding
`CHANGELOG.md` entry. Use the `v0.2.0` GitHub release as the style reference.
Do not use generated pull-request or commit summaries as the release body.

Use this structure when the sections apply:

```markdown
One sentence naming the release theme.

### Added

- Complete descriptions of the user-visible capabilities.

### Fixed

- Problems that users will no longer encounter.

### Semantics

- Important behavior, syntax, or intentional compatibility boundaries.

Install from crates.io with `cargo install bad-editor` (the installed command is `bed`).

**Full Changelog**: https://github.com/softfault/bed/compare/PREVIOUS...CURRENT
```

Keep the following distinctions:

- `CHANGELOG.md` is the complete factual record for a version.
- GitHub release notes are the compact explanation users should read first.
- Pull requests and commit messages describe development history, not the
  released product.
- Patch releases should still list every user-visible addition and fix; they do
  not need filler or internal implementation details.

The release workflow creates a blank draft with platform archives and checksum
files. Review the assets, write the release notes, and only then publish the
draft as the latest stable release or as a prerelease.
