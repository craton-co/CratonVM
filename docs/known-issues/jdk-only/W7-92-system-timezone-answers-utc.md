# W7-92 — the system time zone answered `UTC` on every host, and it is the other two bytes of `RJdkLogging`

**Status: SOURCE LANDED, NOT BUILT and NOT RE-RUN.** This lane could not build,
could not run the suite, and could not run a VM or a `java`. Everything below is
either source-verified (a quoted line at a named function) or a host fact read
with a read-only tool and labelled as such. The one number this record stakes
itself on is a **prediction**, in §6.

Filed as the second half of `W7-91-format-date-symbols-hardcoded-english.md`.
That record's §4 states this defect, names three of its producers and hands it
on; read it for the arithmetic that isolated the two characters. This record
does not repeat it — it corrects it on two points, adds the producer §4 missed,
and states what landed.

---

## 1. The measurement this stands on

Run against a **pristine dev binary** by the orchestrating lane, both runtime
modes red on the same row:

```
HotSpot  : CK RJdkLogging streamBytes=179   handlerLevelGate=ok bytes=89
CratonVM : CK RJdkLogging streamBytes=175   handlerLevelGate=ok bytes=87
```

`W7-91` §1 models `SimpleFormatter`'s default pattern character by character
with the rendered month length `M` and the *unpadded* 12-hour length `H` as the
only unknowns:

```
streamBytes      = 2M + 2H + 167      (HotSpot 179, CratonVM 175)
handlerLevelGate =  M +  H +  83      (HotSpot  89, CratonVM  87)
```

Two equations per VM, both solving to integers in range: **HotSpot M=4 H=2,
CratonVM M=3 H=1.** All four measured numbers reproduce exactly, so this is two
independent one-character defects. `M` is W7-91's (the month name); **`H` is
this record's**: CratonVM's `%1$tl` is one digit where HotSpot's is two, because
CratonVM's default zone is UTC and the host's is not.

I re-derived every step of that model against the JDK 25 sources rather than
inheriting it. The pattern is `SurrogateLogger.getSimpleFormat`'s default,
quoted verbatim from `jdk/internal/logger/SimpleConsoleLogger.java`:

```
"%1$tb %1$td, %1$tY %1$tl:%1$tM:%1$tS %1$Tp %2$s%n%4$s: %5$s%6$s%n"
```

and `java.util.logging.SimpleFormatter.format` builds its first argument as

```java
ZonedDateTime zdt = ZonedDateTime.ofInstant(record.getInstant(), ZoneId.systemDefault());
```

so the wall clock is fixed before `String.format` runs, and `%1$tl` is
`ZonedDateTime.getHour()` reduced to a 12-hour clock **with no padding** — the
single conversion in that pattern whose WIDTH depends on the zone.

### 1.1 Two corrections to W7-91 §4, both host facts

* **The host is UTC−03:00, not UTC+03:00.** Read with a read-only registry
  query: `TimeZoneKeyName` is `Argentina Standard Time`, and .NET's
  `TimeZoneInfo.Local.Id` agrees. So CratonVM's UTC clock runs three hours
  **ahead** of HotSpot's here, not behind. The sign does not touch the
  arithmetic — `H` is a digit COUNT — but it does change which zone the fix has
  to produce, and a record that names the wrong offset invites the next lane to
  "verify" against the wrong number.
* **The month evidence is consistent with the same host.** The format locale
  here is `ru-RU` (read-only `Get-Culture`), whose CLDR abbreviated August is
  four characters. `M=4` on HotSpot and `M=3` (`Aug`) on CratonVM is exactly
  W7-91's defect, measured on this host, and it is independent confirmation
  that the two-unknown model is the right model.

---

## 2. The mechanism — there are FOUR producers, on two dispatch routes

W7-91 §4 lists three producers in dependency order and concludes that
`native_timezone_get_system_id` "decides". That is right for `--jdk-only` and
**wrong for `Compatible`/`--real-jdk`**, which is why the row was red in *both*
modes. The missing producer is the reason.

Registration facts, all source-verified:

* `getSystemTimeZoneID` is registered **exactly once** in the tree —
  `native-builtins/src/lib.rs`, inside `register_essential_natives_with_shims`,
  as `register_with_kind(..., NativeKind::Bridge)`. No sibling registration, no
  synthetic override, nothing to lose a last-write-wins race to. It is the
  registrar that ships on both boot paths (`vm_init` reaches
  `register_essential_natives_with_shims` in real-JDK mode directly and in
  synthetic mode through `register_builtins`).
* `java.util.TimeZone.getSystemTimeZoneID` is `private static native String` in
  JDK 25 — **no `Code` attribute**. So the `--jdk-only` yield
  (`policy.is_jdk_only() && bytecode_available && kind != Intrinsic`) cannot
  fire on it: there is no bytecode to yield to. A `Bridge` native over a genuine
  `native` method survives strict mode. This one runs in **every** mode.
* `getDefault`, `getDefaultRef` and **`setDefaultZone`** are ALSO registered, a
  few thousand lines later in the same registrar, by plain `registry.register`
  — i.e. at the ambient category, which `set_category` fixes to `Bridge` for
  that whole function body. All three are real bytecode in the JDK. So in
  `--jdk-only` they yield and the real bytecode runs; in
  `Compatible`/`--real-jdk` the native wins outright.

That gives two routes to the answer, and until this fix both ended at `"UTC"`:

| Mode | Route | Producer |
|---|---|---|
| `--jdk-only` | real `getDefaultRef` → real `setDefaultZone` bytecode → `getSystemTimeZoneID` native | `native_timezone_get_system_id` — `let tz_id = "UTC";` |
| `Compatible` / `--real-jdk` | `getDefault` native, which never reaches the bytecode and therefore never reaches `getSystemTimeZoneID` at all | `timezone_default_ref` — `.unwrap_or_else(\|\| "UTC".to_string())` |

`setDefaultZone`'s real body (quoted from the JDK 25 sources) is what makes the
first route reachable at all, and what makes `user.timezone` decisive:

```java
String zoneID = props.getProperty("user.timezone");
if (zoneID == null || zoneID.isEmpty()) {
    zoneID = getSystemTimeZoneID(StaticProperty.javaHome());
    if (zoneID == null) { zoneID = GMT_ID; }
}
tz = getTimeZone(zoneID, false);
if (tz == null) {
    String gmtOffsetID = getSystemGMTOffsetID();
    if (gmtOffsetID != null) { zoneID = gmtOffsetID; }
    tz = getTimeZone(zoneID, true);
}
props.setProperty("user.timezone", id);
defaultTimeZone = tz;
```

`user.timezone` is always empty here — `vm/src/vm/vm_init.rs` seeds it from
`$TZ` alone and Windows does not set `$TZ` — which W7-91 §4 already established
and which I re-read. **A patch that only widens `$TZ`/`user.timezone` handling
is inert on this host**, and so is `util_time.rs`'s `os_default_zone_id`; the
`JDK-ONLY-NOTE` W7-91 left on its Windows arm is accurate and stays.

### 2.1 A fifth thing that was wrong, found while editing the fourth

`timezone_default_ref`'s comment claimed it honoured *"the embedder's
`user.timezone` system property"*. It read
`cratonvm_types::flags::runtime_var("user.timezone")`, and `runtime_var` returns
`std::env::var(key)` for any key that is not a declared flag name. `user.timezone`
is not a declared flag name. So that read looked for an **environment variable
literally named `user.timezone`** and never once saw the system property
`vm_init` writes — `-Duser.timezone=Asia/Tokyo` was invisible to
`TimeZone.getDefault()` in Compatible mode. Same family as
`declared-flags-latch-so-set-var-is-invisible-to-tests`. Fixed in passing, with
the old read kept underneath the new one so nothing that depended on it
regresses.

---

## 3. The fix

Three functions, one new resolver. All in this lane's own files; **no crate
dependency was added** and none is needed (§5).

**`native-builtins/src/tzdb.rs` — `system_zone_id(ctx, java_home)`.** The
resolver, added at the end of the module next to the other `pub` wrappers,
because it needs the same `tzdb.dat` catalog they do. It mirrors HotSpot's
`TimeZone_md.c`:

1. `GetDynamicTimeZoneInformation` for the platform's zone key. This is the
   crux, and it needs no registry API: the Win32 call returns
   `TimeZoneKeyName` — the *same* value HotSpot reads out of
   `HKLM\SYSTEM\CurrentControlSet\Control\TimeZoneInformation` — in its own
   struct field. `kernel32` is linked by the Rust standard library on every
   `*-pc-windows-*` target and the entry point is Vista-and-later, so the
   import cannot fail to bind. Declared with a raw `extern "system"` block, the
   idiom `native-io` and this crate's own `inet_address.rs` already use; the
   struct layout is spelled out in a comment offset by offset because
   `clashing_extern_declarations` is `deny` at the workspace and there is no
   second declaration of this symbol in the tree (grepped: none).
2. `<java.home>/lib/tzmappings` to map that key to an IANA id. This is the file
   HotSpot's `matchJavaTZ` reads and the reason the JDK hands this native a
   `javaHome`. Format on this host's JDK 25, verified by reading it:
   `<Windows key>:<region>:<IANA id>:`, CRLF, 361 rows, no header and no
   comment lines. `Argentina Standard Time:001:America/Buenos_Aires:` is the
   row this host hits.
3. On Unix, `/etc/timezone` if the platform key produced nothing.
   `/etc/localtime` is deliberately not followed — the same choice
   `util_time.rs` documents. `$TZ` is deliberately **not** read: both callers
   consult `user.timezone` first and `vm_init` already derives that from `$TZ`,
   so a third read could only disagree with the other two.
4. Validate, then answer. Every id returned has been resolved through
   `get_zone_rules`, i.e. through the same `tzdb.dat` the offset natives read
   and through its own alias table — which is how `tzmappings`'
   `America/Buenos_Aires` reaches the `America/Argentina/Buenos_Aires` rules
   (both names verified present in this host's `tzdb.dat`). Anything the
   platform does not state, or the catalog does not know, answers `None` and the
   caller keeps its previous answer. **Nothing in the file names a zone**, so
   there is no fabricated third answer to find later.

**`native-builtins/src/lib.rs` — `native_timezone_get_system_id`.** Now reads
`args[0]` (the `javaHome` the JDK passes; falls back to the `java.home`
property) and returns the resolver's answer, `"UTC"` when it has none. Keeping
`"UTC"` rather than the JDK's `null` on failure is deliberate: `null` would send
`setDefaultZone` to `GMT_ID`, changing the answer on every host where resolution
fails, and this fix is meant to change the answer only where the platform DOES
state a zone.

**`native-builtins/src/lib.rs` — `native_timezone_get_gmt_offset_id`.** Was an
unconditional `null`; now returns the host's standard offset as `"GMT±HH:MM"`,
computed from the same Win32 struct's `Bias` field with HotSpot's own
`customZoneName` sign convention (a Win32 bias is minutes to ADD to local time
to reach UTC, so the rendered sign is inverted). This is **not** a hedge, it is
the JDK's own second chance: `setDefaultZone` calls it precisely when
`getTimeZone(id, false)` could not resolve the named id, and it is what keeps
the host's OFFSET right even if the Java-side `ZoneInfoFile`/`tzdb.dat` path is
unhealthy in strict mode. It is the reason this fix does not depend on a
question I could not settle without a run (§7).

**`native-builtins/src/lib.rs` — `timezone_default_ref`.** The Compatible-mode
producer. Its fallback id now goes through a new sibling
`timezone_fallback_zone_id`: the `user.timezone` system property (§2.1), then
the host, then `"UTC"`. It also does what real `setDefaultZone` does with the id
it resolves — `props.setProperty("user.timezone", id)` — which both matches
HotSpot's observable behaviour and memoises the resolution, so no call after the
first re-reads `tzmappings`. Resolution moved from the top of the function into
the two branches that need it, so the overwhelmingly common path (the
`defaultTimeZone` static is already populated) does no file I/O at all.

---

## 4. What this narrows on purpose

**Region-specific `tzmappings` rows are not consulted; only the `001` default
row is.** HotSpot's `matchJavaTZ` prefers the row whose region equals the host's
ISO-3166 country and falls back to `001`. Reading the country would mean a
second Win32 import (`GetUserDefaultGeoName`, Windows 10 1709+) — and an import
that fails to BIND kills the process at load, on a host older than the one this
was written on, for a name refinement. The narrowing cannot change an OFFSET:
every row under one Windows key describes the same Windows zone, hence the same
standard offset and the same DST rule, and differs only in which IANA name (and
therefore which pre-modern history) it points at. So on a host in a country with
its own row, CratonVM can answer a different zone *name* than HotSpot while
computing the identical time. That is why the vector added in §6 prints
`getRawOffset()` and not the zone id.

---

## 5. No dependency was added, and none is needed

The task framed the registry read as the crux and as the likely reason to want
`winreg`. It is not: `GetDynamicTimeZoneInformation` is the documented API over
the same registry value, so the whole feature lands with one `extern "system"`
declaration against a DLL the standard library already links. `Cargo.toml` and
`Cargo.lock` are untouched — they are shared with six other lanes this wave and
a dependency change breaks everyone's build.

---

## 6. THE PREDICTION, and the coverage

**`CK RJdkLogging streamBytes=` should read `179`, matching HotSpot, and the row
should go green.** With W7-91's month fix alone it would read `177`; this fix
supplies the second character per line (`H` 1 → 2), and `2M + 2H + 167` with
`M=4, H=2` is `179`. `handlerLevelGate ... bytes=` goes to `89` by the same
arithmetic. A `177` means this fix did not take effect; a `175` means neither
did.

**Do not pin 179.** That number is a function of the time of day: `%1$tl` is
narrower for a one-digit hour, so at 04:00 local (01:00 UTC) the broken VM and
HotSpot would BOTH render one digit and the row would have been green by
accident. The judge is the live same-session diff against HotSpot, which is the
only reason this defect was ever visible.

Durable coverage added to `regression-suite/src/RJdkLogging.java`,
`defaultZoneReachesTheFormatter()`, deliberately without pinning a zone, an
offset, a month or a byte count:

* **Printed** (so the cross-VM diff judges it, in any zone in any month):
  `defaultZoneRawOffsetMs=` from `TimeZone.getDefault().getRawOffset()`. A
  per-zone CONSTANT — stable across the two VMs' different start instants and
  across a DST transition, and identical for every IANA id that shares the
  host's platform zone, so it does not go red for the §4 name narrowing. On this
  host HotSpot prints `-10800000`; pristine dev prints `0`.
  `zoneAgrees=` reports `ZoneId.systemDefault().equals(TimeZone.getDefault().toZoneId())`
  — the JDK's own identity, reached through two different natives here — and
  catches the throw rather than dying on it, because a broken `toZoneId` is
  evidence for the next lane, not a reason for this vector to fail somewhere
  other than at the thing it gates.
* **Asserted**: the formatter's own clock reading must be the captured
  `LogRecord`'s own instant expressed in the zone the JVM itself calls default.
  Two handlers on one record — a `Capture` and the `StreamHandler` — so the
  instant compared is the formatter's actual input, with no second clock read
  and no `setInstant`. Gated on `SimpleFormatter`'s default pattern being in
  force, and built with `String.format` so a locale with non-ASCII digits cannot
  make it disagree for a non-defect reason.

  **This check would NOT have caught W7-92**, where both sides said UTC and
  agreed with each other. It catches a HALF fix, which with four producers on
  two dispatch routes is the likely way this regresses. Stated plainly because a
  check whose reach is overstated is how the campaign got here.

The stale NOTE in `formattedOutputIsRealBytes()` — which told the next lane that
the date prefix comes from hard-coded English tables in `lang_string.rs` — was
rewritten to record both defects and both records instead. Its "do not
strengthen this line into a month name" directive is untouched and still right.

---

## 7. What I did NOT do, and the one question a run has to settle

* **`vm/src/vm/vm_init.rs:3457-3463` was not touched** and no out-of-file patch
  is needed. Seeding `user.timezone` from `$TZ` alone is *correct*: real
  HotSpot's `setDefaultZone` treats an empty `user.timezone` as "ask the
  platform", which is now exactly what happens. Widening it would have been the
  inert patch W7-91 warned about.
* **`util_time.rs`'s `os_default_zone_id` was not repaired.** Another lane holds
  that file and W7-91's `JDK-ONLY-NOTE` on its Windows arm already says
  repairing it alone changes nothing. It is now doubly dead: `jvm_default_zone_id`
  reaches it only when the `TimeZone.getDefault()` round-trip fails, and that
  round-trip now answers the host's zone.
* **`alloc_synth_timezone`'s `rawOffset` field was left alone.** It is filled
  from the curated ~40-zone `tz_standard_offset_seconds` table, which does not
  contain `America/Buenos_Aires`, so the synthetic default `ZoneInfo` carries a
  `rawOffset` field of 0 while the registered `getRawOffset`/`getOffset` natives
  — which shadow it, and which the vector in §6 reads — answer `-10800000` from
  real tzdb. Routing that field through `tzdb::raw_offset_seconds` would fix the
  inconsistency and would also change the field for every zone outside that
  table, in a wave where six other lanes are measuring. Deliberately out of
  scope; a one-line change when someone can run the suite.
* **No `CRATONVM_*` flag was added.** Nothing here is switchable and nothing
  needs to be.
* **The open question.** In `--jdk-only`, `setDefaultZone`'s
  `getTimeZone(zoneID, false)` runs real bytecode down through
  `ZoneInfoFile`, and a Round-13-era comment in `lib.rs` says that class's
  `<clinit>` fails to load `tzdb.dat`. If that comment is still true, the named
  id cannot resolve and `getSystemGMTOffsetID` — now implemented, §3 — carries
  the offset instead, landing on `GMT-03:00`. Either route gives the same
  `getRawOffset()`, which is why §6's witness reads that and not the id. What a
  run should report is **which** of the two happened: if `zoneAgrees=true` and
  the zone renders as an IANA name, the `ZoneInfoFile` comment is stale and
  should be retired; if the zone renders as `GMT-03:00`, it is still live and
  belongs in a record of its own.
