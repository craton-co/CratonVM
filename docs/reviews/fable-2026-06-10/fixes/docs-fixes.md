# Fix note — docs-fixes

## Finding
The docs review (`docs/reviews/fable-2026-06-10/docs-review.md`) flagged, in the
user-facing doc set: (1) 9 broken relative links in README.md / docs/README.md
(files moved under `docs/internal/`, plus 2 nonexistent files BUILD_GUIDE.md and
TRADEMARKS.md); (2) a factual contradiction in RELEASING.md §3 about the publish
gate; (3) CI claims pointing at `.github/workflows/ci.yml`; (4) honesty drift on
`craton-gpu`, the GC backend list, and the "0 clippy warnings" claim.

## Root cause
A docs reorg moved files into `docs/internal/` (and `docs/internal/app-jvm-bugs/`)
without updating the two index files. BUILD_GUIDE.md / TRADEMARKS.md were
referenced but never created. RELEASING.md §3 predated the real (per-crate)
publish policy. README/CONTRIBUTING crate one-liners drifted from the actual
Cargo.toml descriptions.

## Exact change
1. **Broken links — repointed (verified each target exists):**
   - README.md: `docs/javafx-status.md`→`docs/internal/javafx-status.md`;
     `docs/embedding.md`→`docs/internal/embedding.md`;
     `docs/gc-tuning.md`→`docs/internal/gc-tuning.md`;
     `docs/jvm-no-synthetic-stubs.md`→`docs/internal/app-jvm-bugs/jvm-no-synthetic-stubs.md`.
   - docs/README.md: `embedding.md`/`gc-tuning.md`/`javafx-status.md`/
     `jck-compliance.md`→their `internal/` paths; `jvm-no-synthetic-stubs.md`→
     `internal/app-jvm-bugs/...`; both `feature_roadmap_*`→`internal/`; the three
     Open-investigations links→`internal/bc-ec-mod-...`,
     `internal/app-jvm-bugs/tomcat-selector-investigation.md`,
     `internal/app-jvm-bugs/jit-safepoint-revert.md`.
   - docs/README.md `lock-order.md`: there is **no** standalone `docs/lock-order.md`
     (and none anywhere). The canonical lock hierarchy is defined in-source in
     `vm/src/runtime/lock_order.rs` (the `LockLevel` enum + enforcement wrappers),
     so I repointed the link there rather than fabricate a doc. (Note: that source
     file's own module doc still cites `docs/lock-order.md`; creating that file is
     out of my owned-files scope — see Follow-up.)
2. **Created `BUILD_GUIDE.md`** (repo root) — real build/test/lint/bench/layout
   instructions consolidated from README + CONTRIBUTING + INSTALL. Keeps the 3
   existing references (README:229, CONTRIBUTING:14-via-link, docs/INSTALL.md:62)
   valid.
3. **Created `TRADEMARKS.md`** (repo root) — standard "Java/OpenJDK/HotSpot are
   Oracle trademarks", nominative-use note, third-party marks, and a CratonVM/
   Craton name notice tied to Apache-2.0 §6.
4. **RELEASING.md §3 rewritten** to describe the real per-crate publish policy:
   no `publish=false` at `[workspace.package]`; `publish=false` on
   `cuda-bridge`/`jit-cuda`/`craton-gpu`/`native-awt`/`fuzz`; all other library
   crates publishable; path deps already carry `version=`. (Did NOT edit any
   Cargo.toml — oss-meta owns those.)
5. **CI:** README badge already targets `actions/workflows/ci.yml` (the activated
   path) — left as-is. CONTRIBUTING.md §"Before Submitting" reworded to keep CI
   active but accurate (gates fmt→clippy→build→test on ubuntu+windows; dropped the
   stale "see commented-out jobs in ci.yml" line). RELEASING.md §2 CI text already
   accurate.
6. **Honesty fixes:** `craton-gpu` one-liner in README + CONTRIBUTING changed from
   "GPU offload runtime integration" to "build-time Java annotation sources
   (`@Parallel` etc.) for GPU offload". GC line softened in README + CONTRIBUTING
   from "(semi-space, G1, ZGC)" to "Generational GC (young/old; Cheney moving +
   non-moving sweep; experimental `zgc`-gated stub, no G1)". Added a footnote in
   README and a clause in CONTRIBUTING that "0 clippy warnings" is under the
   workspace `[lints]` config. README JIT bullet clarified "~7,200 lines" → "x64
   core ~7,200 lines".

## Files touched
- README.md (edited)
- docs/README.md (edited)
- CONTRIBUTING.md (edited)
- RELEASING.md (edited)
- BUILD_GUIDE.md (created)
- TRADEMARKS.md (created)
- docs/reviews/fable-2026-06-10/fixes/docs-fixes.md (this note)

docs/INSTALL.md: inspected; its only flagged link `../BUILD_GUIDE.md` now
resolves once BUILD_GUIDE.md exists, so no edit needed.

## Tests added
None (documentation only). The review's suggested CI markdown-link-checker is the
right mechanical guard but is CI/workflow work owned by the ci-activate agent.

## Follow-up & risk
- Low risk; markdown-only + two new root files. No code touched, build unaffected.
- The badge/CI-active wording assumes the ci-activate agent lands
  `.github/workflows/ci.yml`; if that slips, the badge and the CONTRIBUTING/
  RELEASING "CI gates this" claims would again be aspirational.
- The RELEASING.md §3 crate list assumes oss-meta's publish gates land on exactly
  `cuda-bridge`/`jit-cuda`/`craton-gpu`/`native-awt`/`fuzz`; if their final set
  differs, sync the bullet list.
- `vm/src/runtime/lock_order.rs` still references a nonexistent `docs/lock-order.md`
  in its module doc. Either create that doc (extracting the in-source hierarchy)
  or update the source comment to be self-referential — a small follow-up for
  whoever owns `vm/`.
- Stat figures (~323k LOC, 3,100+ natives, JIT LOC) remain loose per the review
  but were out of this task's scope; only the JIT "~7,200 lines" wording was
  clarified.
