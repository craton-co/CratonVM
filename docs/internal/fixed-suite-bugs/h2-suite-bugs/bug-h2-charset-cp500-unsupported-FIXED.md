# H2 — `Unsupported charset: cp500` / `Unsupported charset: CP500` (extended `jdk.charsets` provider missing) — FIXED

## Status
**FIXED** — CratonVM now implements the IBM500/CP500 EBCDIC codec directly
(a curated addition to the charset engine, not the full `sun.nio.cs.ext`
provider). `Charset.forName("cp500")`/`("IBM500")` resolves and both
previously-affected H2 suite classes pass.

This closes the doc that was recreated on 2026-07-21 after its original
(same-titled) version was deleted by `b71e7402f` ("docs refactor remove
stale bugs", 2026-06-22) without the underlying gap actually being fixed —
see that doc's own "Status" section for the rediscovery history. This time
the closure is a real fix, not another cleanup deletion.

## Affected test classes (h2database-suite-runner, `jit-real`, real-JDK25) — now PASS
- `org.h2.test.db.TestSetCollation` (`testCp500Collator`)
- `org.h2.test.unit.TestCharsetCollator`

Both PASS on the HotSpot JDK25 baseline; both now PASS on CratonVM too.

## Root cause (recap)
CratonVM's `Charset.forName`/provider-lookup path only resolved the
`java.base` standard charset set (`UTF-8`, `ISO-8859-1`, `US-ASCII`,
`UTF-16*`, `windows-1252`, etc.) plus a curated set of extras (IBM850,
IBM1047, KOI8-R/U, the CJK families via `encoding_rs`, …). `cp500`/`IBM500`
(EBCDIC 500 International) was missing from that curated set, so
`Charset.forName` threw `UnsupportedCharsetException`.

## Fix
Added `IBM500` as a first-class single-byte codec in
`../../../../native-api/src/charset.rs`, following the exact pattern the existing
`IBM1047` codec already established (full 256-byte table + `OnceLock`
reverse-lookup for encode):
- `canonical_charset_name`: `CP500`/`IBM500`/`500`/`CCSID500`/
  `EBCDIC-CP-BE`/`EBCDIC-CP-CH` all normalize to `"IBM500"`.
- `decode_bytes`/`encode_chars`/`encode_chars_lossy`: new `"IBM500"` arms
  using a 256-entry `IBM500_TO_U16` table and its reverse.
- The 256-byte decode table was captured **directly from real JDK25**
  (`new String(allBytes, Charset.forName("cp500"))` on the HotSpot
  baseline, class `sun.nio.cs.ext.IBM500`) rather than from a generic
  EBCDIC-500 reference table, so it is byte-for-byte identical to what H2's
  HotSpot baseline actually produces — this matters because IBM500 has a
  documented ambiguity at byte `0x15` (NEL vs LF; some "international"
  EBCDIC-500 tables used elsewhere, e.g. Python's `cp500` codec, map it to
  U+0085 instead of HotSpot's U+000A) and a genuine duplicate-target
  ambiguity in the reverse (encode) direction (both `0x15` and `0x25` decode
  to U+000A; HotSpot's encoder picks `0x15`, confirmed by dumping
  `"\n".getBytes(Charset.forName("cp500"))` on the real JDK — the existing
  `build_full_sb_rev` "lowest byte wins" tie-break already produces that
  exact choice with no changes needed).
- This is a **curated single-charset fix**, not a general `sun.nio.cs.ext`
  provider registration — the doc's original "Fix options" left both paths
  open; the narrower one was sufficient to close both affected test classes
  and matches how every other extended charset in this engine (IBM850,
  IBM1047, KOI8-R/U, ISO-8859-2/15, the CJK families) was added. Other
  `sun.nio.cs.ext` charsets (other EBCDIC pages, ISO-2022 variants beyond
  `ISO-2022-JP`, GB18030 is already covered) remain unregistered and would
  need the same per-charset treatment if a future test needs them.

## Verification (2026-07-22)
- Standalone: `Charset.forName("cp500")` resolves (`name()` = `"IBM500"`),
  round-trips `"AAB".getBytes(cs)` -> `C1 C1 C2` -> back to `"AAB"`
  (matches HotSpot's real `A`=0xC1/`B`=0xC2 EBCDIC encoding), and
  `Charset.forName("IBM500")` canonicalizes to the same charset.
- `org.h2.test.unit.TestCharsetCollator` and
  `org.h2.test.db.TestSetCollation` both exit 0 (silent H2-test-framework
  PASS) against the fixed build; both reproduce the original
  `UnsupportedCharsetException` on an unmodified pre-fix baseline binary
  built from the same `dev` tip (A/B confirmed).
- `cargo test --release -p cratonvm-native-api -p cratonvm-native-builtins
  --lib`: 179 + 3057 passed, 0 failed, 0 regressions.

## Repro (pre-fix; kept for reference)
```bash
cd apps/h2database-suite-runner && ./run-h2-suite.sh setup   # once
cd ../h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25   -c "target/classes:target/test-classes:$(cat craton-testcp.txt)"   org.h2.test.unit.TestCharsetCollator
```
or simply `Charset.forName("cp500")` in any standalone program.
