# Fix nb-bc-headers — BouncyCastle provenance/license headers

## Finding
oss-readiness (§1 Licensing, Blocker 2) flagged that four files in
`native-builtins/src/` are verbatim / faithful ports of BouncyCastle Java code and
precomputed tables, but their top-of-file headers claimed sole Craton copyright
under `SPDX-License-Identifier: Apache-2.0` and several docstrings asserted the
incorrect "BouncyCastle, Apache-2.0". BouncyCastle is actually distributed under
the Bouncy Castle Licence (an MIT-style permissive licence), not Apache-2.0.
Claiming exclusive Craton/Apache-2.0 copyright over mechanically-transcribed
third-party code is an IP-provenance defect, and the BC origin was invisible to
downstream users at the file level.

## Root cause
The four `bc_*.rs` files were authored as native fast-paths that mechanically
transcribe BouncyCastle algorithm tables / round logic, but their headers were
copied from the standard Craton SPDX boilerplate without preserving the upstream
copyright + permission notice. The root `THIRD-PARTY-NOTICES.md` (created in
Round 2 by the oss-meta agent) already documents the Bouncy Castle MIT-style
licence and lists exactly these four files; the per-file headers just needed to
be brought into agreement with it.

## Exact change (comments only — no code/logic/tables touched)
For each of the four files I rewrote the header comment block to:
1. Change the SPDX tag from `Apache-2.0` to the dual `MIT AND Apache-2.0`.
2. Attribute the BouncyCastle-derived portions as
   `Copyright (c) 2000-2024 The Legion of the Bouncy Castle Inc.`, used under the
   Bouncy Castle Licence (MIT-style), explicitly stating "NOT Apache-2.0".
3. Note that the Craton-authored glue/integration code remains
   `Copyright 2024-2026 Craton Software Company (Apache-2.0)`.
4. Point to `../../THIRD-PARTY-NOTICES.md` (the file lives at the repo root;
   these files are under `native-builtins/src/`, so `../../` resolves to root)
   for the full Bouncy Castle Licence text.

I also corrected the three remaining inline docstrings that still said
"(BouncyCastle, Apache-2.0)":
- `bc_aes.rs` — "transcribed from AESEngine.java (BouncyCastle, Apache-2.0)" →
  now references the Bouncy Castle Licence (MIT-style) + the notices file.
- `bc_newhope.rs` — "transcription of NTT/Reduce/Poly (BouncyCastle, Apache-2.0)"
  → same correction.
- `bc_newhope_tables.rs` — the second-line `//` comment
  "Auto-extracted verbatim from BouncyCastle Precomp.java / NTT.java (Apache-2.0)."
  was folded into the new attribution block.

No constant, table value, function, or algorithm line was modified. A
`grep Apache-2.0` over the four files now shows only intentional mentions (the
dual SPDX tag and the Craton-glue copyright clarification); no "BouncyCastle,
Apache-2.0" mislabel remains.

## Files touched
- `native-builtins/src/bc_aes.rs` (header block + 1 inline docstring)
- `native-builtins/src/bc_chacha.rs` (header block)
- `native-builtins/src/bc_newhope.rs` (header block + 1 inline docstring)
- `native-builtins/src/bc_newhope_tables.rs` (header block)

## Tests added
None — changes are comment-only. No test surface to add for a license-header
correction, and adding one would not exercise any behavior.

## Follow-up & risk
- Risk: negligible. Comment-only edits; the crate compiles unchanged.
- The relative path `../../THIRD-PARTY-NOTICES.md` assumes the notices file stays
  at the repo root (verified present). If it is ever moved, these pointers need
  updating.
- Out of my ownership but related (already covered by other agents / the report):
  ensure `THIRD-PARTY-NOTICES.md` ships inside the published
  `cratonvm-native-builtins` crate (it sits one level above `native-builtins/`,
  so the crate's package `include`/working dir may not capture it — the publish
  agent should confirm the attribution actually travels with the artifact, e.g.
  via a copy or `include` entry under `native-builtins/`). Flagging only; not my
  file to edit.
