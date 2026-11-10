# Vendored agent skills — BeautifulRust

The three skills in this directory come from
[morganlinton/BeautifulRust](https://github.com/morganlinton/BeautifulRust):

| Field | Value |
| --- | --- |
| Version | v1.5.0 (`40db61c6fee84bd121cf288755f0e2f22a50e995`), latest published release |
| Vendored on | 2026-11-10 |
| License | MIT — see `LICENSES/BeautifulRust-MIT.txt` |
| Contents | `SKILL.md` and, for `rust-best-practices`, `references/` |

We track the latest published GitHub release, not tags that only exist in
`main`.

The upstream `evals/` directories are deliberately left out: they are the
authors' test fixtures (input projects and graders), no agent reads them, and
they would add foreign `Cargo.toml`/`Cargo.lock` files to this repository.

`.agents/skills/` is read by OpenCode and by Codex, GitHub Copilot, Cursor and
Gemini CLI. OpenCode registers it as a project compatibility source; nothing
else in this repository defines the same skill IDs.

## Updating

Clone the tag you want, verify the skill tree, and copy it over, still without
`evals/`:

```sh
git clone --depth 1 --branch vX.Y.Z https://github.com/morganlinton/BeautifulRust /tmp/beautiful-rust
cp -R /tmp/beautiful-rust/skills/rust-best-practices/SKILL.md \
      /tmp/beautiful-rust/skills/rust-best-practices/references \
      .agents/skills/rust-best-practices/
cp /tmp/beautiful-rust/skills/rust-review/SKILL.md .agents/skills/rust-review/
cp /tmp/beautiful-rust/skills/rust-port/SKILL.md .agents/skills/rust-port/
cp /tmp/beautiful-rust/LICENSE LICENSES/BeautifulRust-MIT.txt
```

Then update the version and commit in the table above and the clippy lints in
`clippy.toml` and `Cargo.toml` if `lints/` changed too.
