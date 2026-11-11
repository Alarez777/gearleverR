---
description: Review the current branch diff with rust-review
agent: general
subagent: true
---

Load the `rust-review` skill and review the diff below. Do not modify files.

The project's writing rules live in `rust-best-practices`; read the matching
file under `.agents/skills/rust-best-practices/references/` to cite the exact
rule ID (`C-*`, Effective Rust item, `M-*`, or a clippy lint) for each finding.

Use the skill's report format: Must fix / Should fix / Consider / What's
already good.

!`git diff develop...HEAD`
