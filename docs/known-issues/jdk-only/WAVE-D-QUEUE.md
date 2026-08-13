# Wave D queue — nominations landed by wave C that no lane has applied

Each of these has exact literal old/new text in the record named beside it.
They are queued rather than lost: wave C's pool was held down deliberately so
the post-wave build would be a clean snapshot rather than a mix of half-saved
source files (see the two mixed-binary incidents in this session).

| item | file | record | why it is not optional |
|---|---|---|---|
| `SimpleDateFormat.format` resolves the zone offset from the ID field, below dispatch | `native-builtins/src/date_format_fast.rs` (drop `simple_tz_class` from `vm_implemented`) | C12-1 §5 | The `SimpleTimeZone` unregistration CANNOT reach this path. Without it the fix is half-landed and `format` still ignores a caller's `rawOffset`. |
| `java.util.Optional` written with an `int` presence flag in slot 0, where the real class has `value` in slot 0 | `native-builtins/src/http2.rs` (nine sites) | C12-3 | `ref_operand_is_null` treats `Int(0)` as NOT null, so `isPresent()` is `true` for an EMPTY Optional and `get()` returns the flag. `classfile_api.rs` / `jdk25_concurrency.rs` are the correct precedent (1 slot, with a test). |
| seven rustls cipher-suite spellings where the JSSE name is required | `native-builtins/src/t27_tls.rs` `:3254 :3418 :3601 :3676 :5898 :9011 :9419` | C12-2 NOM 2 | The helper already exists in `http_url_connection.rs`; make it `pub(crate)`. Checked: the `auth_type` `contains("ECDSA")` consumer at `:9067` is unaffected. |
| `#[allow(dead_code)]` on `record_https_carrier_session` now has a caller | `native-builtins/src/net_phase_e.rs` | C12-2 NOM 1 | The attribute's own comment names its removal as the cue. |
| interpreter `Serializable[]`/`Cloneable[]` arm is measurably wrong but must be fixed as a PAIR | `typecheck.rs` + `synthetic_implements` | W8-C10-1 | Landing half produces FALSE ArrayStoreExceptions in synthetic-JDK mode. |
| four stale baseline rows | `scripts/baselines/jdk-only-kind-map-25-linux.tsv:7255-7258`, `jdk-only-dead-everywhere.tsv:163` | C12-1 | Checked: no cargo test reads them, nothing goes red today. Linux-only re-freeze. |

## Measurements owed once a build carrying wave C exists

1. `RSimpleTimeZoneRaw` — settles C12's base-capture reading. PASS/104 confirms it; a `MISMATCHED` red means the base IS capturing and C6-2 step 3's guard is needed. Do NOT add that guard pre-emptively: it sits on every `ZoneInfo` offset query's hot path.
2. `RJdkIntrinsics2 --only=<family>` for all eleven families, in separate processes, BEFORE any aggregate run.
3. `ShutdownProbe` across all nine exit modes; falsifier is `normal` printing no `HOOK-RAN-FD1`, or `ran=0` with a hook registered. **Still owed — but the pre-fix half is no longer in question: MEASURED 2026-08-12, hooks never run. HotSpot prints three hook lines on the same program, CratonVM prints none, and there is no output-lost marker, so the "the hook ran and its output was lost" alternative is ruled out (`W7-92` §0). What this probe still has to settle is whether lane C11's RUNNER works, not whether the defect was real.**
4. `RArrayStoreTiers` and `RArrayStoreInterfaces`, each with AND without `--nojit` — a single green run cannot distinguish "both tiers right" from "the JIT never engaged".
5. Hibernate `LocalDateTimeTest` — the one place C12's 3A deletion could regress.

## Added after the queue was first written

| item | file | record | why |
|---|---|---|---|
| the SAME six A7 case-mapping constants, in a THIRD file | `native-builtins/src/case_map.rs` `:118 :126 :141-142`, likely `:283-284` | C2 N7 | The Turkish/Lithuanian locale branch BYPASSES the fix landed in `lang_string.rs`, so `toUpperCase(Locale("tr"))` of `U+A7D3` is still wrong. Its module doc says it implements "Unicode's" rules — that sentence is the tell. Three files now need one constant set; hoist rather than patch a third time. |

### The measurement lesson this queue exists to preserve

The six A7 code points were missed by a sweep that was genuinely exhaustive over
all 65,536 BMP code units — because it diffed the JDK against **Python's**
`str.upper()`/`lower()` as a stand-in for Rust's tables. Python on this host is
UCD 16.0.0 and agrees with the JDK at all six, so the diff was empty and the
sweep reported success.

**An exhaustive sweep against the wrong oracle is still exhaustive.** One axis
was real and complete; the other was a proxy that was never validated as a
proxy. Transliterating Rust into Java proves the repo's CONSTANTS encode the
JDK's answers; it cannot execute `char::to_uppercase`, which is exactly where
these went wrong. Strong witness for enumerations, weak for anything still
deriving from Rust's Unicode tables at runtime — which is the argument for
replacing derivations with enumerations wherever the set is small.

## R11 — ServiceLoader enumerates one provider twice (NEW, intermittent)

Found by re-adjudicating stored corpus logs, not by a probe. Four bc-java
classes (`util.encoders`, `util.utiltest`, `util.io.pem`, `pqc.math.ntru`) were
published as "agrees, failed=0 on both arms". The stored CratonVM logs show it
ran ZERO tests in all four:

    CORPUS-THROW SbRunner org.junit.platform.commons.JUnitException:
      Cannot create Launcher for multiple engines with the same ID 'junit-jupiter'

Both arms used the same `cp.args`, so the classpath is not duplicated — CratonVM's
`ServiceLoader` enumerated one provider twice.

**Lead, not proof:** the same log carries a refusal for `java/util/Enumeration$Impl`
from `native-builtins\src\classloader.rs:6074`, which is on the `getResources`
path `ServiceLoader` walks to find provider-configuration files. If `getResources`
returns the same URL twice (or the refusal makes a fallback re-enumerate), that
would produce exactly this.

**It is INTERMITTENT** — nine other classes in the same run were fine. So a single
green re-run does NOT clear it. The useful instrument is a small REPEATED
`ServiceLoader`/`getResources` probe that counts distinct URLs across many
iterations, not a corpus row.

**No stored log supports the published `failed=0` for those four rows.** They were
counted as agreement because both arms "failed 0 tests" — one of them by running
none. That is the denominator defect in its purest form.
