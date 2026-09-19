# G14-1 — the URI value surface, and how far `RJdkBridge1` got

**Status:** PARTIAL. **Before: MEASURED** on
`C:/craton/target-fcheck/release/cratonvm.exe` (merge `d87dff06a` + two
orchestrator fixes) under `--jdk-only`. **After: NOT MEASURED — and could not
be.** This lane's brief forbids `cargo build`, `cargo check` and `cargo test`,
so no binary exists that contains the code below. Every "after" here is
therefore **PREDICTED**, and is labelled so at every occurrence. What *is*
measured is the oracle (Temurin 25.0.3+9-LTS), the before-state of the vector,
and the before-state of every row in the tables.

**Provenance:** MEAS on both VMs for the before-state and the oracle; the
algorithm the fixes transcribe is read from
`$JAVA_HOME/lib/src.zip!java.base/java/net/URI.java`, not derived.
Probes: `scratchpad/probe/{P1,P2,P3,P4,P5}.java` (session scratchpad).

**Owned files:** `native-builtins/src/net_uri_inet.rs`,
`native-builtins/src/inet_address.rs`. Everything else is a NOMINATION in §7.

---

> **VERIFIED AGAINST A BINARY 2026-09-03. The blocker is GONE, and §0's "after"
> prediction is superseded rather than wrong.**
>
> ```text
> HotSpot 25            PASS RJdkBridge1 (483 checks)
> CratonVM --jdk-only   PASS RJdkBridge1 (483 checks)     diff: EMPTY
> ```
>
> §0 measured CratonVM dying at check **197**, on `negp.getPort() == -1`, having
> completed 196 of the oracle's 394 — and predicted "**still dies at check 197**,
> the blocking fix is in `net_phase_e.rs`, which this lane may not edit". It does
> not die. It completes every check the oracle does, with byte-identical output.
> Somebody made that one-line change in the file this lane did not own; the
> prediction was correct about the blocker and correct that it could not fix it,
> and has simply been overtaken.
>
> **The check count is the vector's own, not a line count.** The last two lines
> are `CK RJdkBridge1 checks=483` and `PASS RJdkBridge1 (483 checks)`; the output
> is 182 lines. Counting lines here would have given 182 against §0's 394 and
> read as a vector that still stops early — the same trap `RSslNullSession`
> sets, where the suite reports `1 passed` for a vector that died after one check
> of ninety-one. Where a vector self-reports its checks, that number is the
> measurement.
>
> 483, not §0's 394: the vector grew. The `uri` section's tripwire
> (`sectionEnd("uri", 50)`) that §0 records as never reached is reached now,
> since the run completes and the diff against the oracle is empty.
>
> **Scope.** This says the vector passes; it does not re-measure §0's 51 rows of
> URI surface individually. Those were MEASURED before and are unchanged by this
> note.

## 0. The headline

| vector | before (MEASURED) | after (PREDICTED) |
|---|---|---|
| `RJdkBridge1` | dies at check **197**, first failing assertion `negp.getPort() == -1` | **still dies at check 197** — the blocking fix is in `net_phase_e.rs`, which this lane may not edit |
| `RJdkNet` | **PASS, 81 checks** | PASS, 81 checks |
| `RStrings` | **PASS, 46 checks** | PASS, 46 checks |

The oracle passes `RJdkBridge1` with **394 checks**. CratonVM completes five
sections (`props=40 treenav=42 collect=19 deque=33 vector=20` = 154) and then
42 of the `uri` section's 50, for **196 passing checks**; the 197th is the
first failure. The `uri` block's own tripwire (`sectionEnd("uri", 50)`) is
never reached, so no `uri=` line is printed.

**This lane did not move the vector.** It is honest to say so up front. What it
did move is 51 measured rows of the surrounding URI surface, all inside its two
files, and it pinned the vector's blocker to a one-line change in a file it
does not own.

---

## 1. The blocker: `java.net.URI`'s port grammar is digits only

The failing assertion is `RJdkBridge1.java:1181`:

```java
URI negp = new URI("http://h:-5/p");
check(negp.getPort() == -1, "':-5' is not a port ...");
```

MEASURED, both VMs, 2026-08-17:

| input | accessor | HotSpot | CratonVM |
|---|---|---|---|
| `http://h:-5/p` | `getPort()` | `-1` | **`-5`** |
| `http://h:-5/p` | `getHost()` | `null` | **`h`** |
| `http://h:+80/p` | `getPort()` | `-1` | **`80`** |
| `http://h:+80/p` | `getHost()` | `null` | **`h`** |
| `http://h:8x/p` | `getHost()` | `null` | **`h`** |
| `http://h:99999999999/p` | `getHost()` | `null` | **`h`** |
| `http://h:2147483648/p` | `getHost()` | `null` | **`h`** |
| `http://h:x/p` | `getHost()` | `null` | **`h`** |
| `http://u@h:x/p` | `getUserInfo()` | `null` | **`u`** |
| `http://:80/p` | `getPort()` | `-1` | **`80`** |
| `http://u@:80/p` | `getUserInfo()` | `null` | **`u`** |
| `http://u@:80/p` | `getPort()` | `-1` | **`80`** |

The vector's own comment predicted the cause and it is exactly right:
`net_phase_e::uri_parse_authority` ends with

```rust
p.parse::<i32>().ok()
```

and Rust's integer parser accepts a leading `+` or `-`. Java's does not — the
port production is `*DIGIT`, and `checkChars(p, q, L_DIGIT, H_DIGIT, "port
number")` is what rejects the rest.

**But the rule is wider than "reject the sign", and this is where the handoff's
"do not generalise a contract from three rows" bites.** Reading `parseAuthority`
in the JDK source settles it: when the server-based parse fails, the JDK does
**not** throw — it *demotes the authority to registry-based*, which nulls
`host` **and** `userInfo` **and** the port together. That is why
`new URI("http://h:-5/p")` is a perfectly legal URI whose `getAuthority()` is
`h:-5` and whose `getHost()` is `null`. A fix that only clamps the port to `-1`
would satisfy the vector's line 1181 and fail its line 1183
(`negp.getHost() == null`) — the next check.

`getPort` and `getHost` live at `net_phase_e.rs:3643` and `:3588`, and
`uri_parse_authority` at `:3206`. `--dump-native-registry` confirms both own
their slots (`owns_slot=true`, `invocations` 4 and 1 in a run that touches
them), so there is no second body to reach for. **NOMINATION 1 in §7.**

### 1a. The one part of it that *was* mine

`urn:isbn:0451450523` has no authority at all, and CratonVM answered
`getPort() == 451450523`. That one is `url_parse` in this lane's file: with no
`://` to anchor on it splits the whole string on its **last** colon, writes
`host = urn:isbn` and `port = 451450523` into the named fields, and
`net_phase_e`'s `getPort` returns the `port` field verbatim whenever it is
positive — so it never reaches the raw-string parse that would have answered
`-1`.

| input | HotSpot `getPort()` | CratonVM before | after (PREDICTED) |
|---|---|---|---|
| `urn:isbn:0451450523` | `-1` | **`451450523`** | `-1` |
| `a:1234` | `-1` | **`1234`** | `-1` |
| `mailto:a@b.com:25` | `-1` | **`25`** | `-1` |

**Fixed** in `uri_store_named`: when `uri_split` reports no authority, write the
`-1` sentinel back over whatever `url_parse` left. That is enough — a
non-positive field makes `getPort` fall through to the raw-string parse, which
sees no authority and answers `-1` on its own.

`url_parse`'s port parser itself is deliberately **left alone**: it also serves
`java.net.URL`, whose grammar is *different*. MEASURED — `new URL("http://h:+80/p").getPort()`
is **80** on HotSpot (URL uses `Integer.parseInt`, which takes the `+`) while
`new URI("http://h:+80/p").getPort()` is `-1`. All eleven `java.net.URL` rows in
the probe already match HotSpot exactly, including
`new URL("http://h:-5/p")` → `MalformedURLException: Invalid port number :-5`.
Tightening the shared parser to URI's grammar would have broken URL.

---

## 2. `Malformed escape pair` is its own reason, not a component name

CratonVM already refused every malformed `%` triple at the right index. It gave
the wrong reason for all nineteen of them, because it routed them through the
component-naming path.

Full oracle, MEASURED 2026-08-17:

| input | HotSpot `getMessage()` | CratonVM before |
|---|---|---|
| `http://h/a%` | `Malformed escape pair at index 10` | `Illegal character in path at index 10` |
| `http://h/a%2` | `Malformed escape pair at index 10` | `Illegal character in path at index 10` |
| `http://h/a%A` | `Malformed escape pair at index 10` | `Illegal character in path at index 10` |
| `http://h/a%zz` | `Malformed escape pair at index 10` | `Illegal character in path at index 10` |
| `http://h/a%2z` | `Malformed escape pair at index 10` | `Illegal character in path at index 10` |
| `http://h/a%z2` | `Malformed escape pair at index 10` | `Illegal character in path at index 10` |
| `http://h/a%GG` | `Malformed escape pair at index 10` | `Illegal character in path at index 10` |
| `http://h/a%2G` | `Malformed escape pair at index 10` | `Illegal character in path at index 10` |
| `http://h/a%%20` | `Malformed escape pair at index 10` | `Illegal character in path at index 10` |
| `http://h/p?q=%2` | `Malformed escape pair at index 13` | `Illegal character in query at index 13` |
| `http://h/p?q=%zz` | `Malformed escape pair at index 13` | `Illegal character in query at index 13` |
| `http://h/p#f%2` | `Malformed escape pair at index 12` | `Illegal character in fragment at index 12` |
| `http://h/p#f%zz` | `Malformed escape pair at index 12` | `Illegal character in fragment at index 12` |
| `http://h%2/p` | `Malformed escape pair at index 8` | `Illegal character in authority at index 8` |
| `http://h%zz/p` | `Malformed escape pair at index 8` | `Illegal character in authority at index 8` |
| `http://u%2@h/p` | `Malformed escape pair at index 8` | `Illegal character in authority at index 8` |
| `%2` | `Malformed escape pair at index 0` | `Illegal character in path at index 0` |
| `a%zzb` | `Malformed escape pair at index 1` | `Illegal character in path at index 1` |
| `mailto:a%2` | `Malformed escape pair at index 8` | `Illegal character in path at index 8` |

**The component makes no difference to the reason.** Path, query, fragment,
authority and a bare relative reference all produce the identical string. This
is `Parser.scanEscape`, which runs *before* any component-specific character
check — so the tie-break is purely positional, and that is measured too:

| input | HotSpot |
|---|---|
| `http://h/a b%2` | `Illegal character in path at index 10` (the space, at 10) |
| `http://h/a%2 b` | `Malformed escape pair at index 10` (the `%`, at 10) |
| `http://h/a<b%2` | `Illegal character in path at index 10` |
| `http://h/a%2<b` | `Malformed escape pair at index 10` |

**Fixed** by splitting `uri_first_illegal_index` into `uri_first_char_fault`,
which returns *why* it stopped. The old function is now a one-line wrapper, so
`net_phase_e`'s `URI.create` caller is untouched.
**After: PREDICTED** — 19 rows.

---

## 3. Inside an IPv6 literal a `%` is a scope id, and we were refusing valid URIs

This is the worse half of the bug, because it goes the wrong way: CratonVM
**refused input HotSpot accepts.**

| input | HotSpot | CratonVM before | after (PREDICTED) |
|---|---|---|---|
| `http://[::1%eth0]/p` | **OK**, host `[::1%eth0]` | `URISyntaxException: Illegal character in authority at index 11` | OK |
| `http://[::1%zz]/p` | **OK**, host `[::1%zz]` | `URISyntaxException: Illegal character in authority at index 11` | OK |
| `http://[::1%25]/p` | OK | OK | OK |
| `http://[::1%]/p` | `URISyntaxException: scope id expected` | `Illegal character in authority at index 11` | `scope id expected` |

The JDK switches the authority to `L_SERVER_PERCENT` the moment it contains a
`]`. **Fixed** narrowly — only the bytes strictly between the authority's `[`
and its `]` are exempt; a bare `%` anywhere else is still an escape triple that
has to be well formed.

`scope id expected` is the **only** URI parse failure in the whole family that
carries no index. It comes from the JDK's one-argument `fail(String reason)`,
which uses the two-argument `URISyntaxException` constructor, whose index is
`-1`, and `getMessage()` then omits the `" at index N"` clause entirely. It
cannot be composed from the others; it has to be transcribed, and the new
`UriParseFail { index: None }` variant exists solely to carry it.

---

## 4. The IPv6 literal body — thirty rows, six reasons, and indices that are not derivable

CratonVM accepted every malformed IPv6 literal except the two shapes an earlier
commit had already handled (`[]` and an unclosed `[`). Full oracle, MEASURED
2026-08-17:

| input | HotSpot `getMessage()` |
|---|---|
| `http://[1]/p` | `IPv6 address too short at index 8` |
| `http://[12]/p` | `IPv6 address too short at index 8` |
| `http://[abc]/p` | `IPv6 address too short at index 8` |
| `http://[abcd]/p` | `IPv6 address too short at index 8` |
| `http://[1:2]/p` | `IPv6 address too short at index 8` |
| `http://[1:2:3]/p` | `IPv6 address too short at index 8` |
| `http://[1:2:3:4:5:6:7]/p` | `IPv6 address too short at index 8` |
| `http://[abcde]/p` | `IPv6 hexadecimal digit sequence too long at index 8` |
| `http://[12345]/p` | `IPv6 hexadecimal digit sequence too long at index 8` |
| `http://[12345::1]/p` | `IPv6 hexadecimal digit sequence too long at index 8` |
| `http://[v7.abc]/p` | `Malformed IPv6 address at index 8` |
| `http://[V7.abc]/p` | `Malformed IPv6 address at index 8` |
| `http://[vz.abc]/p` | `Malformed IPv6 address at index 8` |
| `http://[v.abc]/p` | `Malformed IPv6 address at index 8` |
| `http://[v7.]/p` | `Malformed IPv6 address at index 8` |
| `http://[1.2.3.4]/p` | `Malformed IPv6 address at index 8` |
| `http://[g::1]/p` | `Malformed IPv6 address at index 8` |
| `http://[:1]/p` | `Malformed IPv6 address at index 8` |
| `http://[%eth0]/p` | `Malformed IPv6 address at index 8` |
| `http://[::1:2:3:4:5:6:7:8]/p` | `Malformed IPv6 address at index 8` |
| `http://[1:2:3:4:5:6:7:8:9]/p` | `IPv6 address too long at index 8` |
| `http://[1:2:3:4:5:6:7:1.2.3.4]/p` | `IPv6 address too long at index 8` |
| `http://[1:]/p` | `Expected digits for an IPv6 address at index 10` |
| `http://[::1:]/p` | `Expected digits for an IPv6 address at index 12` |
| `http://[::256.1.1.1]/p` | `Malformed IPv4 address at index 10` |
| `http://[::1.2.3]/p` | `Malformed IPv4 address at index 15` |
| `http://[::1.2.3.400]/p` | `Malformed IPv4 address at index 16` |
| `http://[::1.2.3.4.5]/p` | `Malformed IPv4 address at index 17` |
| `http://[::ffff:1.2.3.999]/p` | `Malformed IPv4 address at index 21` |
| `http://[::1.2.3.4x]/p` | `Expected hex digits or IPv4 address at index 10` |

Accepted, and the check must not fire for them: `[::1]`, `[::]`, `[fe80::1]`,
`[1:2:3:4:5:6:7:8]`, `[1234::1]`, `[1:2:3:4:5:6:1.2.3.4]`, `[::1.2.3.4]`,
`[::ffff:1.2.3.4]`, `[::1%eth0]`, `[::1%zz]`, `[::1%25]`.

**Look at the `Malformed IPv4 address` column: 10, 15, 16, 17, 21 — five
different indices for one reason string.** Nothing about the grammar produces
those numbers. They are wherever `scanIPv4Address`'s unrolled four-byte loop
happened to stop, and the loop reports the value of `q` at the break, which is
sometimes the start of the offending octet (`[::256.1.1.1]` → 10) and sometimes
the position after the last octet it *did* accept (`[::1.2.3]` → 15). This is
the handoff's "messages often cannot be derived, only transcribed", and it is
the reason the fix is a line-for-line transcription of `parseIPv6Reference`,
`scanHexPost`, `scanHexSeq`, `scanIPv4Address`, `takeIPv4Address` and
`scanByte` out of the JDK's own `src.zip`, rather than a re-derivation from
RFC 2373.

**Fixed** as `uri_ipv6_authority_fail`, a pure function over the URI string.
**After: PREDICTED** — 30 rows.

### 4a. Why this refusal is right only inside brackets

The same reading of `parseAuthority` that explains §1 explains why these are
fatal while `http://h:-5/p` is not: the JDK falls back to a **registry-based**
authority when the server-based parse fails, and an authority containing a `]`
is not a legal registry name, so there is no fallback. That asymmetry is the
entire justification for policing bracketed authorities and nothing else. Three
rows would have suggested "malformed host ⇒ throw"; the full family says
"malformed host ⇒ demote, unless it is bracketed".

---

## 5. After the `]` — the port, and the two shapes with no port at all

| input | HotSpot `getMessage()` | CratonVM before |
|---|---|---|
| `http://[::1]]/p` | `Expected port number at index 12` | accepted |
| `http://[::1]x/p` | `Expected port number at index 12` | accepted |
| `http://[::1]:x/p` | `Illegal character in port number at index 13` | accepted |
| `http://[::1]:-5/p` | `Illegal character in port number at index 13` | accepted, `getPort()` = `-5` |
| `http://[::1]:+80/p` | `Illegal character in port number at index 13` | accepted, `getPort()` = `80` |
| `http://[::1]:8x/p` | `Illegal character in port number at index **14**` | accepted |
| `http://[::1]:80x80/p` | `Illegal character in port number at index **15**` | accepted |
| `http://u@[::1]:x/p` | `Illegal character in port number at index **15**` | accepted |
| `http://[::1]:99999999999/p` | `Malformed port number at index 13` | accepted |
| `http://[::1]:2147483648/p` | `Malformed port number at index 13` | accepted |
| `http://[::1]:2147483647/p` | OK, `getPort()` = `2147483647` | OK |
| `http://[::1]:007/p` | OK, `getPort()` = `7` | OK |
| `http://[::1]:/p` | OK, `getPort()` = `-1` | OK |
| `http://[::1]:0/p` | OK, `getPort()` = `0` | OK |

Two rules that a smaller sample would have merged into one:

* the **index moves to the first non-digit** — 13 for `:x`, 14 for `:8x`, 15
  for `:80x80` — because it is `checkChars`, which returns where its scan
  stopped, not where the port began;
* **all-digits-but-out-of-`int`-range is a different reason**, `Malformed port
  number`, and it is reported at the **start** of the port, not at any digit.
  `:99999999999` and `:2147483648` both land on 13.

`:007` → `7` and `:2147483647` → `2147483647` are the boundary rows that keep
the overflow rule from being over-applied.

**Fixed** in the same `uri_ipv6_authority_fail`. **After: PREDICTED** — 10 rows.

---

## 6. What this lane did NOT do

* **It did not move `RJdkBridge1`.** The vector's 197th check needs
  `net_phase_e::uri_parse_authority` and `net_phase_e`'s `getHost`/`getUserInfo`
  to implement registry-based demotion. That file belongs to another lane.
  NOMINATION 1.
* **It measured no "after" state at all.** `cargo build` / `cargo check` /
  `cargo test` are forbidden for this lane, so the binary at
  `target-fcheck/release/cratonvm.exe` does not contain any of §1a–§5. Every
  "after" above is PREDICTED from the oracle plus the transcription, and every
  unit test added in §8 is likewise unrun. Whoever builds next should re-run
  `scratchpad/probe/{P4,P5}.java` on both VMs — the diff should shrink by the
  51 rows named above and by nothing else.
* **It did not touch `url_parse`'s port grammar**, because `java.net.URL` needs
  the loose one (§1a) and all eleven measured URL rows are already green.
* **It did not police non-bracketed hostnames.** `http://a[b]/p` is
  `Illegal character in hostname at index 8` on HotSpot and is still accepted
  here; so is `http://[::1]@h/p` (`Illegal character in user info at index 7`).
  Adding `parseHostname` would put a refusal in front of every reg-name
  authority in the VM, which is a change with a much larger blast radius than
  anything above and wants its own measurement pass.
* **It did not fix `http://[::1]: /p`**, where HotSpot reports
  `Illegal character in authority at index **7**` (the authority's start) and
  we report index 13 (the space). The index comes from `parseAuthority`'s
  post-mortem rather than from the scan, and one row is not enough to pin it.
* **It did not touch `inet_address.rs`.** Nothing in the measured surface
  pointed there; the file is listed as owned, not as needing work.
* **It did not chase `URI.hashCode`.** CratonVM's values differ from HotSpot's
  on essentially every URI (46 rows). `java.net.URI.hashCode` has no specified
  algorithm, `RJdkBridge1` only asserts self-consistency, and that holds. Noted,
  not filed.

---

## 7. NOMINATIONS (outside this lane's two files)

**N1 — `net_phase_e.rs`: the port grammar and registry-based demotion. Blocks
`RJdkBridge1` at check 197.**
`uri_parse_authority` (`:3206`) ends with `p.parse::<i32>().ok()`. It must be
digits-only *and* fit `i32`; and when it is not, the authority is
**registry-based**, which means `getHost` (`:3588`), `getUserInfo` (`:3703`)
and `getPort` (`:3643`) must all answer `null` / `null` / `-1` together while
`getAuthority` keeps the whole literal. The twelve measured rows are the table
in §1. An empty host (`http://:80/p`, `http://u@:80/p`) demotes the same way.
Note `getPort`'s `if p > 0` shortcut over the named `port` field: after this
lane's §1a change the field is correct for authority-less URIs, but the
shortcut still masks a wrong positive port for `http://h:+80/p`, so the
shortcut should be removed or made total in the same change.

**N2 — `net_phase_e.rs`: percent-decoding must skip the inside of `[…]`.**
MEASURED: `new URI("http://[::1%25eth0]/p").getAuthority()` is
`[::1%25eth0]` on HotSpot and `[::1%eth0]` here;
`getSchemeSpecificPart()` diverges the same way (`//[::1%25eth0]/p` vs
`//[::1%eth0]/p`). JDK 25 has
`decode(String s, boolean ignorePercentInBrackets)` and the authority/host
accessors pass `true`. Our `uri_percent_decode` call sites in `getAuthority` /
`getSchemeSpecificPart` need the same flag. (`getHost` is already right.)

**N3 — `net_phase_e.rs:4170`: `URI.create` must relay the wrapped exception's
message, not compose its own.** MEASURED:
`URI.create("http://ho st/")` is
`IllegalArgumentException: Illegal character in authority at index 9: http://ho st/`
on HotSpot and
`Illegal character in URI at index 9: http://ho st/` here. The constructor path
already produces the correct string (`new URI("http://ho st/")` matches
exactly), so `create` should wrap `URISyntaxException.getMessage()` verbatim
instead of building `format!("Illegal character in URI at index {pos}: {s}")`.

**N4 — `net_phase_e.rs:4170`: `URI.create(null)` returns `null` instead of
throwing.** MEASURED: HotSpot throws
`NullPointerException: Cannot invoke "String.length()" because "this.input" is null`;
we return `null` and the caller NPEs later with a different message naming
`URI.create`. `native_uri_init` already throws the right NPE for
`new URI(null)`; `create` needs the same guard.

**N5 — `vm/` (exception plumbing): a `URISyntaxException` thrown from *inside*
real JDK bytecode loses its `input` and `index`.** MEASURED with
`scratchpad/probe/P3.java`:

| construction site | `getInput()` | `getIndex()` |
|---|---|---|
| `new URISyntaxException("theInput","theReason",7)` from app bytecode | `theInput` | `7` |
| thrown by `new URI("http","h","p",null)` (JDK `checkPath`) | **`""`** | `-1` |
| thrown by `native_uri_init` (Rust) | correct | correct |

So five multi-argument-constructor messages are truncated:
`Relative path in absolute URI: ` (HotSpot: `…: http://hp`),
`Expected scheme-specific part: ` (HotSpot: `… at index 5: http:#f`),
`Illegal character in port number: ` (HotSpot: `… at index 9: http://h:-5/p`),
`Expected hostname: ` (HotSpot: `… at index 7: http://:80/p`). The URI objects
those constructors build are otherwise **byte-identical to HotSpot** — the
three-, four-, five- and seven-argument constructors all run real JDK bytecode
and every accessor matches. Only the exception's own fields are lost, which
points at the VM's exception construction path inside JDK frames rather than at
`java.net.URI`.

**N6 — `net_phase_e.rs`: `URI.normalize` removes leading `..` segments it must
keep.** MEASURED: `new URI("http://h/../a").normalize().toString()` is
`http://h/../a` on HotSpot and `http://h/a` here. `URI.normalize` is
RFC-2396-style and deliberately leaves un-resolvable leading `..` in place.

**N7 — `net_phase_e.rs`: `URI.resolve("")` must return the base's *directory*.**
MEASURED: `new URI("http://h/a/b").resolve("")` is `http://h/a/` on HotSpot and
`http://h/a/b` here. This is not RFC 3986 — it is `URI.resolvePath`'s
`if (cn == 0) path = base.substring(0, i + 1)` branch, where `i` is the base
path's last `/`. Transcribe it; it cannot be derived from the RFC.

**N8 — dead code in this lane's own file, left alone deliberately.**
`native_uri_init_3` / `_4` / `_5` / `_7`, `native_uri_create`,
`native_uri_get_scheme_specific_part`, `uri_resolve_path` and
`uri_normalize_path` in `net_uri_inet.rs` are **never registered and never
called** — `--dump-native-registry` lists exactly one `java/net/URI.<init>`
row, the `(Ljava/lang/String;)V` one from `lib.rs:19500`, and `create` is owned
by `net_phase_e.rs:4170`. This is the same shape as commit `8c72d23ca`'s "a fix
that landed in dead code". Deleting them is a `lib.rs`-adjacent decision (they
may be intended for a synthetic-JDK mode), so it is nominated rather than done.
Anyone editing a multi-argument URI constructor should check this first: those
constructors are currently served by **real JDK bytecode**, and it is correct.

---

## 8. What changed, and how it is guarded

`native-builtins/src/net_uri_inet.rs` only:

| change | rows |
|---|---|
| `UriCharFault` + `uri_first_char_fault`; `uri_first_illegal_index` becomes a wrapper | §2, 19 |
| `%` exempted strictly inside the authority's `[…]` | §3, 2 (both were *false refusals*) |
| `UriParseFail` + `Ipv6Scanner` + `uri_ipv6_authority_fail` (transcribed) | §4, 30 |
| port rules after `]` in the same function | §5, 10 |
| `uri_syntax_exception` — picks the 2- or 3-argument `URISyntaxException` ctor | §3's index-less row |
| `uri_store_named` writes the `-1` port sentinel for authority-less URIs | §1a, 3 |

Ten new tests in the existing `#[cfg(test)] mod new2_net_tests`, one per family,
each asserting the transcribed reason **and** index for every oracle row above,
plus the accept-lists that keep the new refusals from over-firing.
**They have not been run** — see §6.

`rustfmt --edition 2021 --check` was run **in place, in the crate tree** and
reports the same three pre-existing deviations as `HEAD` (lines 489, 1924,
1940) and none introduced. Zero CR bytes in both owned files.
