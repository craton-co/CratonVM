# H5-1 — `native-io`'s "bridges in the wrong place" are fabricated receivers, and cannot move

**Status** **FIXED-UNVERIFIED — no binary carrying these changes has been built or run.**
**Date** 2026-08-20
**Lane** H5 (`--jdk-only` completion, wave H)
**Subject** `native-io/src/**`, the P1 *NIO, files, networking* row of
[`docs/jdk-only-runtime-services.md`](runtime-services-blocker-inventory.md)
**Instrument** `javap -p -s` against JDK 25.0.3+9, plus a static parse of every
`register*` call site in `native-io/src` (method in §7)
**Oracle** `C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot` — **not** the
`Eclipse Adoptium` path, see §6.1


> **VERIFIED AGAINST A BINARY 2026-09-02.** §5.1 and §5.2 — this record's own two
> falsifiable predictions — both hold. Binary
> `/data/l7dod-target/debug/cratonvm`, built 2026-09-02 from this tree on
> `azure-host-2`. The record had said "Nothing below has been run" for **13 days**.
>
> **§5.1, the one registry row that must change.** Predicted `kind:
> synthetic-stub`, `kind_chosen: true`, `overwrote` absent. Measured, compatible
> mode:
>
> ```text
> read      ([BII)I  kind=synthetic-stub  chosen=true  overwrote=null  native-io/src/lib.rs:7396
> readBytes ([BII)I  kind=bridge          chosen=true  overwrote=null  native-io/src/lib.rs:7459
> ```
>
> All three predicates hold, and `readBytes` is unchanged as predicted. The site
> is `lib.rs:7396`, not the predicted `6499` — **line drift, not a falsifier**:
> the file is right, and this record's own preamble says to grep the literal
> rather than trust either number. Under `--jdk-only` the three `read` rows are
> ABSENT, which §5.1 names in advance as the expected refusal.
>
> **§5.2, the strict-mode report.** Predicted: the triple disappears from
> `violations[]` filtered to `native-shadows-bytecode`. Measured on a
> `--jdk-only-report`: **61 shadow violations, of which FileInputStream
> contributes ZERO**, and `read([BII)I` is absent from the list entirely. The 9
> `FileInputStream` rows that remain are all `synthetic-native-registered` — a
> different `kind`, and the expected strict refusal of a synthetic stub.
>
> **What was NOT run, so nobody reads more into this than it says.** §5.3's
> 102-vector arm was not run for this note; the falsifier there is a delta
> ("the count falls by MORE than 1"), and a delta needs a before-binary this note
> does not have. What it can say is that the corpus HAS been run against this
> code since — 218 vectors, compatible against strict, in
> [`P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md`](P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md),
> with zero strict-only failures. §5.4's warning stands untouched: the maps in §2
> and §3.4 come from a static parser and are NOT verified by this note.
>
> **§3.2's subject was measured independently the same day, and it is real.** The
> mechanism this record names — "this crate fabricates instances of abstract
> classes" — was found live in `com.sun.net.httpserver`, where `HttpServer
> .create()` returned an instance of the ABSTRACT public class and
> `createContext()` returned a context whose every accessor threw
> `AbstractMethodError` on both shipping arms. Four defects, fixed and verified in
> [`the-httpserver-family-four-defects-20260902.md`](the-httpserver-family-four-defects-20260902.md).
> That is corroboration of §3.2's thesis from outside this record, not a
> verification of §3.4's map.


> **VERIFIED AGAINST A BINARY 2026-09-02.** §5.1 and §5.2 — this record's own two
> falsifiable predictions — both hold. Binary
> `/data/l7dod-target/debug/cratonvm`, built 2026-09-02 from this tree on
> `azure-host-2`. The record had said "Nothing below has been run" for **13 days**.
>
> **§5.1, the one registry row that must change.** Predicted `kind:
> synthetic-stub`, `kind_chosen: true`, `overwrote` absent. Measured, compatible
> mode:
>
> ```text
> read      ([BII)I  kind=synthetic-stub  chosen=true  overwrote=null  native-io/src/lib.rs:7396
> readBytes ([BII)I  kind=bridge          chosen=true  overwrote=null  native-io/src/lib.rs:7459
> ```
>
> All three predicates hold, and `readBytes` is unchanged as predicted. The site
> is `lib.rs:7396`, not the predicted `6499` — **line drift, not a falsifier**:
> the file is right, and this record's own preamble says to grep the literal
> rather than trust either number. Under `--jdk-only` the three `read` rows are
> ABSENT, which §5.1 names in advance as the expected refusal.
>
> **§5.2, the strict-mode report.** Predicted: the triple disappears from
> `violations[]` filtered to `native-shadows-bytecode`. Measured on a
> `--jdk-only-report`: **61 shadow violations, of which FileInputStream
> contributes ZERO**, and `read([BII)I` is absent from the list entirely. The 9
> `FileInputStream` rows that remain are all `synthetic-native-registered` — a
> different `kind`, and the expected strict refusal of a synthetic stub.
>
> **What was NOT run, so nobody reads more into this than it says.** §5.3's
> 102-vector arm was not run for this note; the falsifier there is a delta
> ("the count falls by MORE than 1"), and a delta needs a before-binary this note
> does not have. What it can say is that the corpus HAS been run against this
> code since — 218 vectors, compatible against strict, in
> [`P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md`](P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md),
> with zero strict-only failures. §5.4's warning stands untouched: the maps in §2
> and §3.4 come from a static parser and are NOT verified by this note.
>
> **§3.2's subject was measured independently the same day, and it is real.** The
> mechanism this record names — "this crate fabricates instances of abstract
> classes" — was found live in `com.sun.net.httpserver`, where `HttpServer
> .create()` returned an instance of the ABSTRACT public class and
> `createContext()` returned a context whose every accessor threw
> `AbstractMethodError` on both shipping arms. Four defects, fixed and verified in
> [`the-httpserver-family-four-defects-20260902.md`](the-httpserver-family-four-defects-20260902.md).
> That is corroboration of §3.2's thesis from outside this record, not a
> verification of §3.4's map.


> **VERIFIED AGAINST A BINARY 2026-09-02.** §5.1 and §5.2 — this record's own two
> falsifiable predictions — both hold. Binary
> `/data/l7dod-target/debug/cratonvm`, built 2026-09-02 from this tree on
> `azure-host-2`. The record had said "Nothing below has been run" for **13 days**.
>
> **§5.1, the one registry row that must change.** Predicted `kind:
> synthetic-stub`, `kind_chosen: true`, `overwrote` absent. Measured, compatible
> mode:
>
> ```text
> read      ([BII)I  kind=synthetic-stub  chosen=true  overwrote=null  native-io/src/lib.rs:7396
> readBytes ([BII)I  kind=bridge          chosen=true  overwrote=null  native-io/src/lib.rs:7459
> ```
>
> All three predicates hold, and `readBytes` is unchanged as predicted. The site
> is `lib.rs:7396`, not the predicted `6499` — **line drift, not a falsifier**:
> the file is right, and this record's own preamble says to grep the literal
> rather than trust either number. Under `--jdk-only` the three `read` rows are
> ABSENT, which §5.1 names in advance as the expected refusal.
>
> **§5.2, the strict-mode report.** Predicted: the triple disappears from
> `violations[]` filtered to `native-shadows-bytecode`. Measured on a
> `--jdk-only-report`: **61 shadow violations, of which FileInputStream
> contributes ZERO**, and `read([BII)I` is absent from the list entirely. The 9
> `FileInputStream` rows that remain are all `synthetic-native-registered` — a
> different `kind`, and the expected strict refusal of a synthetic stub.
>
> **What was NOT run, so nobody reads more into this than it says.** §5.3's
> 102-vector arm was not run for this note; the falsifier there is a delta
> ("the count falls by MORE than 1"), and a delta needs a before-binary this note
> does not have. What it can say is that the corpus HAS been run against this
> code since — 218 vectors, compatible against strict, in
> [`P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md`](P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md),
> with zero strict-only failures. §5.4's warning stands untouched: the maps in §2
> and §3.4 come from a static parser and are NOT verified by this note.
>
> **§3.2's subject was measured independently the same day, and it is real.** The
> mechanism this record names — "this crate fabricates instances of abstract
> classes" — was found live in `com.sun.net.httpserver`, where `HttpServer
> .create()` returned an instance of the ABSTRACT public class and
> `createContext()` returned a context whose every accessor threw
> `AbstractMethodError` on both shipping arms. Four defects, fixed and verified in
> [`the-httpserver-family-four-defects-20260902.md`](the-httpserver-family-four-defects-20260902.md).
> That is corroboration of §3.2's thesis from outside this record, not a
> verification of §3.4's map.


> **VERIFIED AGAINST A BINARY 2026-09-02.** §5.1 and §5.2 — this record's own two
> falsifiable predictions — both hold. Binary
> `/data/l7dod-target/debug/cratonvm`, built 2026-09-02 from this tree on
> `azure-host-2`. The record had said "Nothing below has been run" for **13 days**.
>
> **§5.1, the one registry row that must change.** Predicted `kind:
> synthetic-stub`, `kind_chosen: true`, `overwrote` absent. Measured, compatible
> mode:
>
> ```text
> read      ([BII)I  kind=synthetic-stub  chosen=true  overwrote=null  native-io/src/lib.rs:7396
> readBytes ([BII)I  kind=bridge          chosen=true  overwrote=null  native-io/src/lib.rs:7459
> ```
>
> All three predicates hold, and `readBytes` is unchanged as predicted. The site
> is `lib.rs:7396`, not the predicted `6499` — **line drift, not a falsifier**:
> the file is right, and this record's own preamble says to grep the literal
> rather than trust either number. Under `--jdk-only` the three `read` rows are
> ABSENT, which §5.1 names in advance as the expected refusal.
>
> **§5.2, the strict-mode report.** Predicted: the triple disappears from
> `violations[]` filtered to `native-shadows-bytecode`. Measured on a
> `--jdk-only-report`: **61 shadow violations, of which FileInputStream
> contributes ZERO**, and `read([BII)I` is absent from the list entirely. The 9
> `FileInputStream` rows that remain are all `synthetic-native-registered` — a
> different `kind`, and the expected strict refusal of a synthetic stub.
>
> **What was NOT run, so nobody reads more into this than it says.** §5.3's
> 102-vector arm was not run for this note; the falsifier there is a delta
> ("the count falls by MORE than 1"), and a delta needs a before-binary this note
> does not have. What it can say is that the corpus HAS been run against this
> code since — 218 vectors, compatible against strict, in
> [`P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md`](P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md),
> with zero strict-only failures. §5.4's warning stands untouched: the maps in §2
> and §3.4 come from a static parser and are NOT verified by this note.
>
> **§3.2's subject was measured independently the same day, and it is real.** The
> mechanism this record names — "this crate fabricates instances of abstract
> classes" — was found live in `com.sun.net.httpserver`, where `HttpServer
> .create()` returned an instance of the ABSTRACT public class and
> `createContext()` returned a context whose every accessor threw
> `AbstractMethodError` on both shipping arms. Four defects, fixed and verified in
> [`the-httpserver-family-four-defects-20260902.md`](the-httpserver-family-four-defects-20260902.md).
> That is corroboration of §3.2's thesis from outside this record, not a
> verification of §3.4's map.


> **VERIFIED AGAINST A BINARY 2026-09-02.** §5.1 and §5.2 — this record's own two
> falsifiable predictions — both hold. Binary
> `/data/l7dod-target/debug/cratonvm`, built 2026-09-02 from this tree on
> `azure-host-2`. The record had said "Nothing below has been run" for **13 days**.
>
> **§5.1, the one registry row that must change.** Predicted `kind:
> synthetic-stub`, `kind_chosen: true`, `overwrote` absent. Measured, compatible
> mode:
>
> ```text
> read      ([BII)I  kind=synthetic-stub  chosen=true  overwrote=null  native-io/src/lib.rs:7396
> readBytes ([BII)I  kind=bridge          chosen=true  overwrote=null  native-io/src/lib.rs:7459
> ```
>
> All three predicates hold, and `readBytes` is unchanged as predicted. The site
> is `lib.rs:7396`, not the predicted `6499` — **line drift, not a falsifier**:
> the file is right, and this record's own preamble says to grep the literal
> rather than trust either number. Under `--jdk-only` the three `read` rows are
> ABSENT, which §5.1 names in advance as the expected refusal.
>
> **§5.2, the strict-mode report.** Predicted: the triple disappears from
> `violations[]` filtered to `native-shadows-bytecode`. Measured on a
> `--jdk-only-report`: **61 shadow violations, of which FileInputStream
> contributes ZERO**, and `read([BII)I` is absent from the list entirely. The 9
> `FileInputStream` rows that remain are all `synthetic-native-registered` — a
> different `kind`, and the expected strict refusal of a synthetic stub.
>
> **What was NOT run, so nobody reads more into this than it says.** §5.3's
> 102-vector arm was not run for this note; the falsifier there is a delta
> ("the count falls by MORE than 1"), and a delta needs a before-binary this note
> does not have. What it can say is that the corpus HAS been run against this
> code since — 218 vectors, compatible against strict, in
> [`P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md`](P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md),
> with zero strict-only failures. §5.4's warning stands untouched: the maps in §2
> and §3.4 come from a static parser and are NOT verified by this note.
>
> **§3.2's subject was measured independently the same day, and it is real.** The
> mechanism this record names — "this crate fabricates instances of abstract
> classes" — was found live in `com.sun.net.httpserver`, where `HttpServer
> .create()` returned an instance of the ABSTRACT public class and
> `createContext()` returned a context whose every accessor threw
> `AbstractMethodError` on both shipping arms. Four defects, fixed and verified in
> [`the-httpserver-family-four-defects-20260902.md`](the-httpserver-family-four-defects-20260902.md).
> That is corroboration of §3.2's thesis from outside this record, not a
> verification of §3.4's map.


> **VERIFIED AGAINST A BINARY 2026-09-02.** §5.1 and §5.2 — this record's own two
> falsifiable predictions — both hold. Binary
> `/data/l7dod-target/debug/cratonvm`, built 2026-09-02 from this tree on
> `azure-host-2`. The record had said "Nothing below has been run" for **13 days**.
>
> **§5.1, the one registry row that must change.** Predicted `kind:
> synthetic-stub`, `kind_chosen: true`, `overwrote` absent. Measured, compatible
> mode:
>
> ```text
> read      ([BII)I  kind=synthetic-stub  chosen=true  overwrote=null  native-io/src/lib.rs:7396
> readBytes ([BII)I  kind=bridge          chosen=true  overwrote=null  native-io/src/lib.rs:7459
> ```
>
> All three predicates hold, and `readBytes` is unchanged as predicted. The site
> is `lib.rs:7396`, not the predicted `6499` — **line drift, not a falsifier**:
> the file is right, and this record's own preamble says to grep the literal
> rather than trust either number. Under `--jdk-only` the three `read` rows are
> ABSENT, which §5.1 names in advance as the expected refusal.
>
> **§5.2, the strict-mode report.** Predicted: the triple disappears from
> `violations[]` filtered to `native-shadows-bytecode`. Measured on a
> `--jdk-only-report`: **61 shadow violations, of which FileInputStream
> contributes ZERO**, and `read([BII)I` is absent from the list entirely. The 9
> `FileInputStream` rows that remain are all `synthetic-native-registered` — a
> different `kind`, and the expected strict refusal of a synthetic stub.
>
> **What was NOT run, so nobody reads more into this than it says.** §5.3's
> 102-vector arm was not run for this note; the falsifier there is a delta
> ("the count falls by MORE than 1"), and a delta needs a before-binary this note
> does not have. What it can say is that the corpus HAS been run against this
> code since — 218 vectors, compatible against strict, in
> [`P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md`](P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md),
> with zero strict-only failures. §5.4's warning stands untouched: the maps in §2
> and §3.4 come from a static parser and are NOT verified by this note.
>
> **§3.2's subject was measured independently the same day, and it is real.** The
> mechanism this record names — "this crate fabricates instances of abstract
> classes" — was found live in `com.sun.net.httpserver`, where `HttpServer
> .create()` returned an instance of the ABSTRACT public class and
> `createContext()` returned a context whose every accessor threw
> `AbstractMethodError` on both shipping arms. Four defects, fixed and verified in
> [`the-httpserver-family-four-defects-20260902.md`](the-httpserver-family-four-defects-20260902.md).
> That is corroboration of §3.2's thesis from outside this record, not a
> verification of §3.4's map.


> **VERIFIED AGAINST A BINARY 2026-09-02.** §5.1 and §5.2 — this record's own two
> falsifiable predictions — both hold. Binary
> `/data/l7dod-target/debug/cratonvm`, built 2026-09-02 from this tree on
> `azure-host-2`. The record had said "Nothing below has been run" for **13 days**.
>
> **§5.1, the one registry row that must change.** Predicted `kind:
> synthetic-stub`, `kind_chosen: true`, `overwrote` absent. Measured, compatible
> mode:
>
> ```text
> read      ([BII)I  kind=synthetic-stub  chosen=true  overwrote=null  native-io/src/lib.rs:7396
> readBytes ([BII)I  kind=bridge          chosen=true  overwrote=null  native-io/src/lib.rs:7459
> ```
>
> All three predicates hold, and `readBytes` is unchanged as predicted. The site
> is `lib.rs:7396`, not the predicted `6499` — **line drift, not a falsifier**:
> the file is right, and this record's own preamble says to grep the literal
> rather than trust either number. Under `--jdk-only` the three `read` rows are
> ABSENT, which §5.1 names in advance as the expected refusal.
>
> **§5.2, the strict-mode report.** Predicted: the triple disappears from
> `violations[]` filtered to `native-shadows-bytecode`. Measured on a
> `--jdk-only-report`: **61 shadow violations, of which FileInputStream
> contributes ZERO**, and `read([BII)I` is absent from the list entirely. The 9
> `FileInputStream` rows that remain are all `synthetic-native-registered` — a
> different `kind`, and the expected strict refusal of a synthetic stub.
>
> **What was NOT run, so nobody reads more into this than it says.** §5.3's
> 102-vector arm was not run for this note; the falsifier there is a delta
> ("the count falls by MORE than 1"), and a delta needs a before-binary this note
> does not have. What it can say is that the corpus HAS been run against this
> code since — 218 vectors, compatible against strict, in
> [`P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md`](P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md),
> with zero strict-only failures. §5.4's warning stands untouched: the maps in §2
> and §3.4 come from a static parser and are NOT verified by this note.
>
> **§3.2's subject was measured independently the same day, and it is real.** The
> mechanism this record names — "this crate fabricates instances of abstract
> classes" — was found live in `com.sun.net.httpserver`, where `HttpServer
> .create()` returned an instance of the ABSTRACT public class and
> `createContext()` returned a context whose every accessor threw
> `AbstractMethodError` on both shipping arms. Four defects, fixed and verified in
> [`the-httpserver-family-four-defects-20260902.md`](the-httpserver-family-four-defects-20260902.md).
> That is corroboration of §3.2's thesis from outside this record, not a
> verification of §3.4's map.


> **VERIFIED AGAINST A BINARY 2026-09-02.** §5.1 and §5.2 — this record's own two
> falsifiable predictions — both hold. Binary
> `/data/l7dod-target/debug/cratonvm`, built 2026-09-02 from this tree on
> `azure-host-2`. The record had said "Nothing below has been run" for **13 days**.
>
> **§5.1, the one registry row that must change.** Predicted `kind:
> synthetic-stub`, `kind_chosen: true`, `overwrote` absent. Measured, compatible
> mode:
>
> ```text
> read      ([BII)I  kind=synthetic-stub  chosen=true  overwrote=null  native-io/src/lib.rs:7396
> readBytes ([BII)I  kind=bridge          chosen=true  overwrote=null  native-io/src/lib.rs:7459
> ```
>
> All three predicates hold, and `readBytes` is unchanged as predicted. The site
> is `lib.rs:7396`, not the predicted `6499` — **line drift, not a falsifier**:
> the file is right, and this record's own preamble says to grep the literal
> rather than trust either number. Under `--jdk-only` the three `read` rows are
> ABSENT, which §5.1 names in advance as the expected refusal.
>
> **§5.2, the strict-mode report.** Predicted: the triple disappears from
> `violations[]` filtered to `native-shadows-bytecode`. Measured on a
> `--jdk-only-report`: **61 shadow violations, of which FileInputStream
> contributes ZERO**, and `read([BII)I` is absent from the list entirely. The 9
> `FileInputStream` rows that remain are all `synthetic-native-registered` — a
> different `kind`, and the expected strict refusal of a synthetic stub.
>
> **What was NOT run, so nobody reads more into this than it says.** §5.3's
> 102-vector arm was not run for this note; the falsifier there is a delta
> ("the count falls by MORE than 1"), and a delta needs a before-binary this note
> does not have. What it can say is that the corpus HAS been run against this
> code since — 218 vectors, compatible against strict, in
> [`P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md`](P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md),
> with zero strict-only failures. §5.4's warning stands untouched: the maps in §2
> and §3.4 come from a static parser and are NOT verified by this note.
>
> **§3.2's subject was measured independently the same day, and it is real.** The
> mechanism this record names — "this crate fabricates instances of abstract
> classes" — was found live in `com.sun.net.httpserver`, where `HttpServer
> .create()` returned an instance of the ABSTRACT public class and
> `createContext()` returned a context whose every accessor threw
> `AbstractMethodError` on both shipping arms. Four defects, fixed and verified in
> [`the-httpserver-family-four-defects-20260902.md`](the-httpserver-family-four-defects-20260902.md).
> That is corroboration of §3.2's thesis from outside this record, not a
> verification of §3.4's map.


> **VERIFIED AGAINST A BINARY 2026-09-02.** §5.1 and §5.2 — this record's own two
> falsifiable predictions — both hold. Binary
> `/data/l7dod-target/debug/cratonvm`, built 2026-09-02 from this tree on
> `azure-host-2`. The record had said "Nothing below has been run" for **13 days**.
>
> **§5.1, the one registry row that must change.** Predicted `kind:
> synthetic-stub`, `kind_chosen: true`, `overwrote` absent. Measured, compatible
> mode:
>
> ```text
> read      ([BII)I  kind=synthetic-stub  chosen=true  overwrote=null  native-io/src/lib.rs:7396
> readBytes ([BII)I  kind=bridge          chosen=true  overwrote=null  native-io/src/lib.rs:7459
> ```
>
> All three predicates hold, and `readBytes` is unchanged as predicted. The site
> is `lib.rs:7396`, not the predicted `6499` — **line drift, not a falsifier**:
> the file is right, and this record's own preamble says to grep the literal
> rather than trust either number. Under `--jdk-only` the three `read` rows are
> ABSENT, which §5.1 names in advance as the expected refusal.
>
> **§5.2, the strict-mode report.** Predicted: the triple disappears from
> `violations[]` filtered to `native-shadows-bytecode`. Measured on a
> `--jdk-only-report`: **61 shadow violations, of which FileInputStream
> contributes ZERO**, and `read([BII)I` is absent from the list entirely. The 9
> `FileInputStream` rows that remain are all `synthetic-native-registered` — a
> different `kind`, and the expected strict refusal of a synthetic stub.
>
> **What was NOT run, so nobody reads more into this than it says.** §5.3's
> 102-vector arm was not run for this note; the falsifier there is a delta
> ("the count falls by MORE than 1"), and a delta needs a before-binary this note
> does not have. What it can say is that the corpus HAS been run against this
> code since — 218 vectors, compatible against strict, in
> [`P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md`](P4A-a-corpus-under-jdk-only-for-the-first-time-20260829.md),
> with zero strict-only failures. §5.4's warning stands untouched: the maps in §2
> and §3.4 come from a static parser and are NOT verified by this note.
>
> **§3.2's subject was measured independently the same day, and it is real.** The
> mechanism this record names — "this crate fabricates instances of abstract
> classes" — was found live in `com.sun.net.httpserver`, where `HttpServer
> .create()` returned an instance of the ABSTRACT public class and
> `createContext()` returned a context whose every accessor threw
> `AbstractMethodError` on both shipping arms. Four defects, fixed and verified in
> [`the-httpserver-family-four-defects-20260902.md`](the-httpserver-family-four-defects-20260902.md).
> That is corroboration of §3.2's thesis from outside this record, not a
> verification of §3.4's map.

Commits: `45d6649ae` (H5-A), `be4c2fbd5` (comment corrections), `d2c3c2258`
(this record, pre-merge draft), `fb67a921b` (merge of the branch tip
`59e5fd8d0`), plus the update commit that carries this text.

> **Base and merge.** This lane's worktree was cut at **`26e4b5db4`** while
> `claude/jdk-only-mode-handoff-09b48c` was already at `59e5fd8d0`. Everything
> in §2 and §3 was measured at `26e4b5db4` and then **re-checked against the
> merged tree**; §9 says what the gap contained and what of it touches this
> lane. The short answer: `native-io/` changed by exactly one comment block
> (`process.rs`, +33 lines, H3-1), no registration in this crate moved, and the
> census stands. The gap did, however, supply a *stronger* proof for §1.3 and a
> replacement for a number in §6.4.

> **Line numbers.** `native-io/src/lib.rs` line numbers below are
> `26e4b5db4`'s unless a `HEAD` number is given alongside. This lane's own two
> commits are comment-only but they are ~110 lines of comment, and the shift is
> **not uniform** — five separate insertion points. Grep the literal, do not
> trust either number: `[window≠absence]` applies to line numbers too.

---

## 0. What moved, in one table

Nothing in this record was measured on a running VM. The **PREDICTED** column
says what a build should show; §5 says what falsifies each.

| Change | Strict (`--jdk-only`) | Compatible | Evidence |
|---|---|---|---|
| H5-A: delete duplicate `FileInputStream.read([BII)I` | **moves** — one `Bridge` shadow retired | **moves** — native yields to real bytecode | `javap`, §1 |
| H5-B: the abstract-API registrations | **neither** — nothing changed | **neither** | §3: they cannot move; the reason is not the one the row gives |
| H5-C: making registrar categories explicit | **neither** — nothing to do | **neither** | §4: all 29 registrars are already explicit |
| Comment corrections (`be4c2fbd5`) | **neither** | **neither** | comment-only |

**One triple's worth of strict-mode movement is the honest total.** Do not let
§2's census, which is large, read as though it were a fix.

---

## 1. H5-A — the duplicate `FileInputStream.read([BII)I` registration

### 1.1 Both sites, as they stood at `26e4b5db4`

`native-io/src/lib.rs`, both inside `register_io_natives`:

```text
lib.rs:6499   registry.register("java/io/FileInputStream", "read", "([BII)I",
                                native_fis_read_bytes);      <- SyntheticStub block
lib.rs:6584   registry.register("java/io/FileInputStream", "read", "([BII)I",
                                native_fis_read_bytes);      <- ambient Bridge
```

and, in a `#[cfg(feature = "synthetic-jdk")]` block that also requires
`!registry.drops_real_layout_synthetic()` at run time, a **third**:

```text
lib.rs:6437   registry.register("java/io/FileInputStream", "read", "([BII)I",
                                native_fis_read_bytes);      <- ambient Bridge
```

The ambient is `Bridge` because `register_io_natives` opens with
`set_category(NativeKind::Bridge)` (lib.rs:6155) and the `SyntheticStub` block
saves and restores around itself (6491 / 6533). So the last write is `Bridge`,
and that is what the slot carries.

### 1.2 Which one is correct

```console
$ JDK="$(dirname "$(dirname "$(command -v javap)")")"; echo "$JDK"
/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot
$ javap -p -s java.io.FileInputStream | grep -A1 'read(byte\[\], int, int)'
  private native int readBytes(byte[], int, int) throws java.io.IOException;
    descriptor: ([BII)I
  public int read(byte[], int, int) throws java.io.IOException;
    descriptor: ([BII)I
```

`public int read(byte[], int, int)` carries a `Code` attribute and is **not**
`ACC_NATIVE`; the only native bulk read is the *private* `readBytes([BII)I`,
which this file bridges separately with `register_with_kind(..., Bridge)`.
Contract §1.5 defines a bridge as what an `ACC_NATIVE` method binds to. There is
nothing here to bind to, so `Bridge` was wrong on the merits and the
`SyntheticStub` registration is the correct one.

The full nine `ACC_NATIVE` methods on JDK 25's `FileInputStream` are `open0`,
`read0`, `readBytes`, `skip0`, `available0`, `length0`, `position0`,
`isRegularFile0`, `initIDs` — every one of them separately and correctly
registered `Bridge` further down the same function. The seven triples in the
`SyntheticStub` block are the **public** surface, all concrete bytecode.

### 1.3 What changes and what does not

**The old comment at lib.rs:6572 gave a reason for keeping the line that is
false.** It said *"silently demoting it would also change which of the two
callbacks wins, so this must be resolved with `overwrote` + `invocations`, not
by deleting a line."* Both registrations name the **same callback**,
`native_fis_read_bytes`. There is no second callback. Deleting the line changes
the slot's `kind` and nothing else.

- **Callback: unchanged.** `native_fis_read_bytes` before and after.
- **Kind: `Bridge` → `SyntheticStub`.** Explicitly stated (`set_category`
  inside the block), so `category_chosen` is true and nothing downstream can
  ambient-downgrade it further.
- **Compatible mode: changed, and the mechanism is verified rather than
  assumed.** `java/io/FileInputStream` is one of the twelve names in
  `real_protected_stub_class_common`
  (`vm/src/runtime/interpreter/native_override.rs:6904`, reached from
  `synthetic_stub_kind_should_yield_to_real_bytecode` at :6795), the list of
  classes **both** dispatch paths yield to real bytecode for a `SyntheticStub`.
  So the tag change does not merely *permit* the real `read([BII)I` bytecode to
  run — an existing, named list makes it run. That bytecode's only real work is
  `invokevirtual readBytes:([BII)I`, which is the genuine bridge, so the
  behavioural delta is the real wrapper's bounds checking and its
  `IndexOutOfBoundsException` messages.

  The list is **not** new — it is present at `26e4b5db4` too, and there is a
  `#[cfg(test)] REAL_PROTECTED_STUB_CORPUS` beside it whose stated purpose is
  that deleting a class from it fails a test. I did not look for it until after
  the merge, and the first draft of this section said the tag change merely
  "permits" the real bytecode to run. It does more than permit it. Recorded
  because the understatement was mine, not a record's.
- **Strict mode: changed.** `SyntheticStub` is not `allowed_in(JdkOnly)`
  (`native-api/src/registry.rs:5384`), so the registration is refused and one
  `native-shadows-bytecode` row disappears.
- **The odd-one-out argument.** After the delete, `read([BII)I` carries the same
  tag as its six public-surface siblings (`<init>(String)`, `read()`,
  `read([B)`, `available()`, `skip(J)`, `close()`). It was the only one of the
  seven that differed, and it differed *only* because of the duplicate line.

No other crate registers this triple. `native-builtins/src/lib.rs:9408`
registers `FileInputStream.<init>(Ljava/io/FileDescriptor;)V` and `:14810`
registers `registerNatives()V`; neither touches `read`.

---

## 2. The duplicate-registration census (H5-A item 4), in full

Measured on `26e4b5db4`, before any change in this lane. Method and coverage
in §7. **Two triples in list A were already documented as deliberate; the rest
were not.**

### 2.A Same triple registered more than once **in the same function**

| Triple | Sites | Reading |
|---|---|---|
| `java/io/FileInputStream.<init>(Ljava/lang/String;)V` | lib.rs:6417, 6492 | 6417 is `#[cfg(feature="synthetic-jdk")]` + `!drops_real_layout_synthetic()`; 6492 is the unconditional `SyntheticStub` block. Same callback (`native_fis_open0`). Later one wins, and it is the right one. **Benign.** |
| `java/io/FileInputStream.read()I` | 6436, 6498 | same shape, same callback `native_fis_read`. **Benign.** |
| `java/io/FileInputStream.read([B)I` | 6443, 6505 | same shape, two *separate but textually identical* closures. **Benign, and duplicated source.** |
| `java/io/FileInputStream.read([BII)I` | 6437, 6499, **6584** | **THE DEFECT.** §1. Fixed in `45d6649ae`; now 6437 + 6499 only. |
| `java/io/FileInputStream.available()I` | 6464, 6525 | same shape, callback `native_fis_available`. **Benign.** |
| `java/io/FileInputStream.skip(J)J` | 6470, 6531 | **Benign.** |
| `java/io/FileInputStream.close()V` | 6471, 6532 | **Benign.** |
| `java/nio/channels/SelectableChannel.register(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;` | nio_selector.rs:4170, 4197 | 4170 is a standalone row; 4197 is inside the eight-class `for c in [...]` loop that includes `java/nio/channels/SelectableChannel`. Same callback. **Benign but redundant — delete 4170 or drop the class from the loop.** |
| `java/util/Scanner.hasNext()Z` | lib.rs:7735, 7843 | 7735 is the Scanner predicate; 7843 is the "Interface dispatch: Iterator" row, registered on `java/util/Scanner` rather than on `java/util/Iterator`. Same callback. **Benign, and the reason the file's own "2 on abstract Readable/Iterator methods" comment is wrong.** |

### 2.B Same triple registered in **more than one function** in the crate

Registration order inside `register_io_natives` decides these. The relevant
order is: `register_socket_channel_real` (6223) → `register_async_socket_real`
(6227) → `register_pipe_real` (6231) → … → `register_phase92_io_completeness`
(7376, which calls `register_async_file_channel` / `register_watch_service` /
`register_datagram_channel`) → **`nio_native::register_t16_channel_overrides`
(7380, whose last statement is `net::register_sun_nio_ch_net`)** →
`register_datagram_channel` again (7385).

| Triple | Sites | Winner | Reading |
|---|---|---|---|
| **`sun/nio/ch/UnixDispatcher.close0(Ljava/io/FileDescriptor;)V`** | lib.rs:6694 (`native_fd_close0`), net.rs:4178 (`net_close`) | **net.rs** | **The only cross-function duplicate whose two sites carry DIFFERENT callbacks.** lib.rs's registration is dead and its 10-line comment describes a fix that no longer runs. Already measured in-tree 2026-08-17 (`--dump-native-registry`, `owns_slot: false`, note at lib.rs ~2080) — and that note cites `net.rs:4188`, which has drifted to 4178. Comment corrected in `be4c2fbd5`; the line is left in place, see §10.N3. |
| **`java/io/RandomAccessFile.getFilePointer()J`** | lib.rs:15314 (`native_raf_get_file_pointer`), random_access_file.rs:579 (`native_getFilePointer`) | **random_access_file.rs**, always | Different callbacks, and **`CRATONVM_REAL_RAF` cannot reach this triple.** `register_io_extras_natives` (called at lib.rs:7424 `HEAD`) registers its RAF block only `if !real_raf_enabled()`; `register_random_access_file_natives` is called six lines later (7430) and registers `getFilePointer` **unconditionally**. So the synthetic `getFilePointer` is dead in *both* settings of the flag, and the diagnostic gate's own comment — "Default (unset) = synthetic" — is wrong for this one method. `getFilePointer()J` is `public native long` on JDK 25, so the surviving `Bridge` is correct on the merits; the defect is that a flag silently does not cover one of the ten methods it claims to. `[flag≠mode drops it]`. **§10.N4.** |
| `java/io/Reader.read(Ljava/nio/CharBuffer;)I` | lib.rs:7064, lib.rs:12002 (`register_string_rw_natives`, `SyntheticStub`) | 12002 | 7064 is ambient `Bridge`; 12002 is an explicit `SyntheticStub` — and per the "no-opinion must not overwrite an adjudicated one" rule in `register_inner`, an explicit choice DOES win, so the slot ends `SyntheticStub`. Undocumented but correct. |
| 9 × `java/nio/channels/AsynchronousChannelGroup.*` | async_socket.rs:3536–3558, nio_native.rs:1799–1815 | nio_native (t16) | Documented at nio_native.rs:1749 — *"called by `register_io_natives` AFTER phase-92 so our entries win"*. Deliberate. |
| 4 × `java/nio/channels/AsynchronousFileChannel.{open,isOpen,size,close}` | lib.rs:21283–21403, nio_native.rs:1770–1778 | nio_native (t16) | Same deliberate layering. |
| 4 × `java/nio/channels/AsynchronousSocketChannel.{open×2,isOpen,close}` | async_socket.rs:3385–3398, nio_native.rs:1782–1795 | nio_native (t16) | Same. |
| 8 × `java/nio/channels/DatagramChannel.*` | lib.rs:23366–23494, nio_native.rs:1834–1861 | **lib.rs** — `register_datagram_channel` is called AGAIN at 7385, after t16 | Documented at lib.rs:7382 and at lib.rs:24741. Deliberate re-application. |
**That is the whole of list B: 27 triples. Twenty-five are deliberate,
documented last-wins layering. Two are not: `UnixDispatcher.close0` and
`RandomAccessFile.getFilePointer`, and both of those are cases where a comment
elsewhere in the tree describes behaviour the registration order overrules.**

**Correction to my own census, kept because the next person will write the same
parser.** A first pass reported eight further list-B rows, all
`java/util/Scanner.*` duplicated between `register_scanner_natives` and
`register_nio_natives`. They do not exist. `register_nio_natives` registers on a
loop variable `c` bound by `for c in &["java/nio/ByteBuffer", …]`, and the
parser resolved `c` against a **file-global** `let c = "java/util/Scanner"`
hundreds of lines earlier instead of a function-scoped binding. **A census that
resolves an identifier out of scope invents duplicates, and it invented eight of
them** — every one of which looked like a serious finding. The fix is in §7.1(b).

### 2.C Residual blind spot

15 call sites resolve to no static triple. About a third are the regex matching
`.register(` inside comment prose (file_channel.rs:497, watch.rs:203 and 1164,
nio_selector.rs:2388, lib.rs:24290 — not code). The genuine gaps are:

- `direct_buffer.rs:2057–2058` — the `dbb_wide_accessors!` macro's generated
  `java/nio/DirectByteBuffer` accessors.
- `nio_native.rs:1172–1174` — the `register_fd_native` table helper.
- `nio_native.rs:1283` — `FD_UNIX`, a const declared outside this crate's files.
- `nio_selector.rs:4424` — the netty-epoll `(name, sig)` tuple loop.
- `socket_channel.rs:5381` — the `client_socket` `(m, d, cb)` tuple loop.
- `async_socket.rs:3571` — `&format!("()L{cls};")` for `sun/nio/ch/Iocp`.
- `lib.rs:22456, 22463` — two `java/nio/file/Path.register` rows whose class
  literal sits on the line after the paren.

None of them can be *ruled out* as duplicates; they were not examined.

---

## 3. H5-B — the family map, and why the row's prescription is wrong

### 3.1 The row says

> *the rest are "bridges in the wrong place" — registered on the abstract public
> API (`java.nio.channels.Pipe`, `AsynchronousSocketChannel`, `DatagramChannel`)
> instead of the `sun.nio.ch.*Impl` classes where the JDK declares its natives.*

The observation is right. **The prescription — move them down to the `*Impl` —
would break every one of these families, and not for a
`class-not-loaded` reason.**

### 3.2 The mechanism: this crate fabricates instances of abstract classes

`native-io` allocates objects whose class name **is** the abstract public class,
and then registers the natives that serve those objects on that same name. The
registration is not misplaced relative to the receiver; the *receiver* is
misplaced relative to the JDK.

```text
lib.rs:24297          try_alloc_synthetic(ctx, "java/nio/channels/DatagramChannel", DC_NUM_FIELDS)
socket_channel.rs:1398, 4766, 4905
                      alloc_obj(ctx, "java/nio/channels/SocketChannel", SC_OBJECT_SLOTS)
socket_channel.rs:4334
                      alloc_obj(ctx, "java/nio/channels/ServerSocketChannel", SC_OBJECT_SLOTS)
async_socket.rs:611, 2099
                      alloc_obj(ctx, "java/nio/channels/AsynchronousSocketChannel", N_FIELDS)
async_socket.rs:2023  alloc_obj(ctx, "java/nio/channels/AsynchronousChannelGroup", 1)
lib.rs:22786, 23401   try_alloc_synthetic(ctx, "java/nio/file/Path", 2)
lib.rs:22835          try_alloc_synthetic(ctx, "java/nio/file/WatchService", WS_NUM_FIELDS)
lib.rs:23013          try_alloc_synthetic(ctx, "java/nio/file/WatchKey", WK_NUM_FIELDS)
lib.rs:23120          try_alloc_synthetic(ctx, "java/nio/file/WatchEvent", WE_NUM_FIELDS)
lib.rs:22730          try_alloc_synthetic(ctx, "java/nio/file/WatchEvent$Kind", 1)
plus java/nio/{Char,Int,Long,Float,Double,Short}Buffer
```

Every one of those is abstract or an interface on JDK 25 (`javap`, one line
each):

```text
public abstract class java.nio.channels.SocketChannel …
public abstract class java.nio.channels.ServerSocketChannel …
public abstract class java.nio.channels.AsynchronousSocketChannel …
public abstract class java.nio.channels.AsynchronousChannelGroup
public interface     java.nio.file.Path …
public interface     java.nio.file.WatchService extends java.io.Closeable
public interface     java.nio.file.WatchKey
public interface     java.nio.file.WatchEvent<T>
public abstract class java.nio.CharBuffer …
public abstract class java.nio.IntBuffer …
public abstract class java.nio.channels.Pipe
```

So for `DatagramChannel`, `SocketChannel`, `ServerSocketChannel`,
`AsynchronousSocketChannel`, `AsynchronousChannelGroup`, `Path`, `WatchService`,
`WatchKey`, `WatchEvent` and the six typed buffers, **moving the registration to
the `sun.nio.ch.*Impl` class strands the receiver**: the object's class name is
the abstract one, dispatch keys on it, and every method on the family becomes
`NoSuchMethodError`. `native-io/src/lib.rs:23496` already says so about
`localAddress()` in its own words.

The defect these registrations *are* is therefore one layer up: **the VM
fabricates an instance of an abstract class.** That is `[name≠real]` /
`[Ok≠use]` again. The fix is not to move the natives; it is either to fabricate
under a distinct synthetic name — which this crate already does elsewhere
(`cratonvm/synthetic/ProcessPipeInputStream`, `…ProcessPipeOutputStream`,
`…ProcessExitWaiter`, and `process.rs` explicitly tags those `SyntheticStub`
for exactly this reason) — or to construct the real `*Impl`. Either is a
wave-2 change with its own runs; neither belongs in a comment-and-delete pass.

### 3.3 Pipe: the row is factually wrong about this family

`register_pipe_real` (pipe.rs:1390–1458) registers on **both** the `sun.nio.ch`
`Impl` classes and the abstract ones:

| Class | Rows | JDK 25 verdict |
|---|---|---|
| `sun/nio/ch/SourceChannelImpl` | `read(Ljava/nio/ByteBuffer;)I`, `isOpen()Z`, `close()V`, `configureBlocking(Z)…` | `read` is **concrete bytecode on the Impl**; `isOpen`/`close`/`configureBlocking` are declared on `AbstractInterruptibleChannel` / `AbstractSelectableChannel`, so they read as *absent-method* on the Impl |
| `sun/nio/ch/SinkChannelImpl` | `write(…)I`, `isOpen`, `close`, `configureBlocking` | same shape |
| `java/nio/channels/Pipe$SourceChannel` | `read`, `isOpen`, `close` | **all three ABSENT.** `javap -p` gives this class exactly two members: `<init>(SelectorProvider)` and `validOps()I` |
| `java/nio/channels/Pipe$SinkChannel` | `write`, `isOpen`, `close` | same — exactly two members |
| `java/nio/channels/Pipe` | `open()` concrete-shadow, `source()`/`sink()` abstract | `open()` is a real static factory with bytecode |

`pipe_open` (pipe.rs:1020–1021) allocates `sun/nio/ch/SourceChannelImpl` and
`sun/nio/ch/SinkChannelImpl`, and pipe.rs's own `channel_private_base` doc
records a `--dump-native-registry` measurement confirming those Impl names are
what a real-image receiver carries. **So this family is already "in the right
place", and additionally carries six abstract-class rows that name methods the
abstract class does not declare.**

Those six look deletable. **They were not deleted.** The reason they exist is
stated in the file — *"same implementations, different declared class so
Java-side dispatch lands here either way"* — and a `invokevirtual` whose
constant-pool class is `Pipe$SourceChannel` is a real dispatch shape. Whether
CratonVM's `resolve_step1_native` keys on the receiver's class or on the CP
class decides it, and that is a run, not a source read. §10.N1.

### 3.4 The family map

Distinct triples, `javap -p -s` against JDK 25.0.3+9 (Windows image). `NAT` =
`ACC_NATIVE` (a legitimate §1.5 bridge). `shadw` = concrete bytecode
(§1.4 shadow — **the only column that is strict-mode debt**). `abstr` = declared
abstract. `absM` = the class does not declare that method. `absC` = the image
has no such class.

| Class registered on | NAT | shadw | abstr | absM | absC | Verdict |
|---|---:|---:|---:|---:|---:|---|
| `sun/nio/ch/Net` | **25** | 0 | 0 | 0 | 0 | **BRIDGE, clean.** Right place, right kind. |
| `java/lang/ProcessHandleImpl` | 7 | 0 | 0 | 0 | 0 | **BRIDGE, clean.** |
| `java/lang/ProcessHandleImpl$Info` | 2 | 0 | 0 | 0 | 0 | **BRIDGE, clean.** |
| `jdk/net/WindowsSocketOptions` | 9 | 0 | 0 | 0 | 0 | **BRIDGE, clean.** |
| `sun/nio/ch/IOUtil` | 8 | 0 | 0 | 0 | 0 | **BRIDGE, clean.** |
| `sun/nio/ch/NativeSocketAddress` | 12 | 0 | 0 | 0 | 0 | **BRIDGE, clean.** |
| `java/lang/ProcessImpl` | 10 | 0 | 0 | 1 | 0 | **BRIDGE**, one absent method. |
| `java/io/FileInputStream` | 9 | 8 | 0 | 0 | 0 | Split: 9 real bridges + the 8-row public surface, correctly `SyntheticStub` after H5-A. |
| `java/io/FileOutputStream` | 4 | 8 | 0 | 1 | 0 | Same shape as above; the `SyntheticStub` treatment applied to `FileInputStream` has **not** been applied here. §10.N5. |
| `java/io/RandomAccessFile` | 10 | **17** | 0 | 0 | 0 | Row calls this a "confirmed bridge". It is 10 bridges and **17 shadows**. §4.2. |
| `sun/nio/ch/SocketDispatcher` | 3 | 2 | 0 | 0 | 0 | Mostly bridge. |
| `jdk/internal/misc/Unsafe` | 2 | 2 | 0 | 0 | 0 | Mixed. |
| `java/util/Scanner` | 0 | **37** | 0 | 0 | 0 | Pure shadow, tagged `Bridge`, **strict-mode LIVE**. §4.3. |
| `sun/nio/ch/SocketChannelImpl` | 0 | **27** | 0 | 16 | 0 | Right place, all shadows. |
| `java/nio/HeapByteBuffer` | 0 | 26 | 0 | 26 | 0 | Right place (concrete class), all shadows. |
| `java/nio/ByteBuffer` | 0 | 24 | **22** | 6 | 0 | **Abstract class.** Cannot move: the crate fabricates typed buffers. |
| `java/io/File` | 0 | 20 | 0 | 0 | 0 | Concrete class, all shadows. |
| `java/nio/file/Files` | 0 | 20 | 0 | 0 | 0 | Static utility, all shadows. |
| `java/nio/CharBuffer` | 0 | 18 | 10 | 5 | 0 | Abstract; fabricated. |
| `java/io/DataInputStream` | 0 | 15 | 0 | 3 | 0 | Concrete; `Bridge`-tagged, **strict-mode LIVE**. §4.3. |
| `java/io/DataOutputStream` | 0 | 14 | 0 | 1 | 0 | Same. |
| `sun/nio/ch/ServerSocketChannelImpl` | 0 | 12 | 0 | 15 | 0 | Right place. |
| `sun/nio/cs/StreamEncoder` | 0 | 12 | 0 | 0 | 0 | `SyntheticStub`-tagged → dropped under `--jdk-only`. |
| `sun/nio/cs/StreamDecoder` | 0 | 10 | 0 | 0 | 0 | Same. |
| `java/io/StringWriter` | 0 | 12 | 0 | 0 | 0 | `SyntheticStub` (string_rw). |
| `java/util/jar/JarFile` | 0 | 10 | 0 | 4 | 0 | `Bridge`, concrete class. |
| `java/util/zip/ZipFile` | 0 | 10 | 0 | 2 | 0 | Same. |
| `java/io/ByteArrayOutputStream` | 0 | 11 | 0 | 2 | 0 | `Bridge`, concrete. |
| **`java/nio/channels/DatagramChannel`** | **0** | **3** | **12** | **20** | 0 | **ABSTRACT, and the VM fabricates it (lib.rs:24297). CANNOT MOVE.** |
| **`java/nio/channels/SocketChannel`** | **0** | **7** | **15** | **19** | 0 | **ABSTRACT, fabricated (socket_channel.rs:1398/4766/4905). CANNOT MOVE.** |
| **`java/nio/channels/ServerSocketChannel`** | **0** | **5** | **5** | **15** | 0 | **ABSTRACT, fabricated (socket_channel.rs:4334). CANNOT MOVE.** |
| **`java/nio/channels/AsynchronousSocketChannel`** | **0** | **4** | **7** | **4** | 0 | **ABSTRACT, fabricated (async_socket.rs:611/2099). CANNOT MOVE.** |
| **`java/nio/channels/AsynchronousServerSocketChannel`** | 0 | 3 | 4 | 5 | 0 | Abstract; no fabrication site found — **candidate, unproven.** |
| **`java/nio/channels/AsynchronousChannelGroup`** | 0 | 3 | 5 | 0 | 0 | **ABSTRACT, fabricated (async_socket.rs:2023). CANNOT MOVE.** |
| **`java/nio/channels/AsynchronousFileChannel`** | 0 | 2 | 8 | 2 | 0 | Abstract; no fabrication site found — **candidate, unproven.** |
| **`java/nio/file/Path`** | 0 | 5 | 15 | 0 | 0 | **INTERFACE, fabricated (lib.rs:22786/23401). CANNOT MOVE.** |
| **`java/nio/file/WatchService`** | 0 | 0 | 4 | 0 | 0 | **INTERFACE, fabricated (lib.rs:22835). CANNOT MOVE.** |
| **`java/nio/file/WatchKey`** | 0 | 0 | 5 | 0 | 0 | **INTERFACE, fabricated (lib.rs:23013). CANNOT MOVE.** |
| **`java/nio/file/WatchEvent`** | 0 | 0 | 3 | 0 | 0 | **INTERFACE, fabricated (lib.rs:23120). CANNOT MOVE.** |
| `java/nio/channels/Pipe` | 0 | 1 | 2 | 0 | 0 | Abstract factory; `open()` shadows real bytecode. §3.3. |
| `java/nio/channels/Pipe$SourceChannel` | 0 | 0 | 0 | **3** | 0 | Impl rows already exist; these three name absent methods. **Deletion candidate, unproven — §10.N1.** |
| `java/nio/channels/Pipe$SinkChannel` | 0 | 0 | 0 | **3** | 0 | Same. |
| `java/nio/channels/Selector` | 0 | 1 | 9 | 1 | 0 | Interface; `SelectorImpl` rows registered alongside. |
| `java/nio/channels/SelectionKey` | 0 | 2 | 7 | 0 | 0 | Abstract; `SelectionKeyImpl` rows alongside. |
| `java/nio/channels/SelectableChannel` | 0 | 1 | 2 | 0 | 0 | Abstract. |
| `java/nio/channels/spi/SelectorProvider` | 0 | 0 | 2 | 0 | 0 | **Abstract SPI — intercepts every application `SelectorProvider`.** |
| `java/nio/channels/NetworkChannel` | 0 | 0 | 1 | 0 | 0 | **Interface.** |
| `java/nio/file/FileSystem` | 0 | 0 | 1 | 0 | 0 | **Abstract.** |
| `java/io/DataInput` / `java/io/DataOutput` | 0 | 0 | 2 each | 0 | 0 | **Interfaces — intercepts every user implementor.** Already flagged in-tree at lib.rs's `register_data_stream_natives`. |
| **`java/io/Closeable`** | 0 | 0 | **1** | 0 | 0 | **INTERFACE.** `close()V` → `native_scanner_close`. §3.5. |
| **`java/lang/AutoCloseable`** | 0 | 0 | **1** | 0 | 0 | **INTERFACE.** Same callback. §3.5. |
| `java/lang/Process` | 0 | 7 | 6 | 0 | 0 | Abstract; `process.rs` already splits this from the fabricated half. |
| `cratonvm/synthetic/*` (4 classes) | 0 | 0 | 0 | 0 | 25 | **The right pattern** — fabricate under a name the image does not own, tag `SyntheticStub`, and strict mode drops it. |
| `sun/nio/fs/Unix*`, `sun/nio/ch/EPoll*`, `KQueuePort`, `EventFD`, `UnixDispatcher`, `io/netty/*`, `jdk/net/LinuxSocketOptions` | 0 | 0 | 0 | 0 | 81 | **Platform-conditioned absence, not absence.** These are Linux/netty classes; the oracle here is a Windows image. `register()`'s `no_image_receiver` re-tag already demotes `Bridge`→`SyntheticStub` for a receiver no supported image declares — check that list before treating any of these as a finding. |

Full per-triple output is reproducible in one command; §7.

### 3.5 The one that should worry a reader most

`register_scanner_natives` ends with:

```text
lib.rs:7847   registry.register("java/io/Closeable",      "close", "()V", native_scanner_close);
lib.rs:7848   registry.register("java/lang/AutoCloseable", "close", "()V", native_scanner_close);
```

under `set_category(Bridge)`, i.e. **admitted under `--jdk-only`**. Those are the
two most-implemented interfaces in the JDK, and every try-with-resources on an
`AutoCloseable`-typed variable compiles to
`invokeinterface java/lang/AutoCloseable.close:()V`.

A prior wave already added an exact-class receiver guard inside
`native_scanner_close` (lib.rs:5826): non-`java/util/Scanner` receivers return
early. **But the early return is `Ok(None)`, which is a completed void call, not
a fall-through.** `MethodCallResult` has no "decline, run the bytecode" value.
So for any receiver that does reach this native through the bare interface, the
`close()` is silently swallowed and the real `close()` never runs — the
`[decline masks]` shape. The guard's own comment concedes the bound is a
premise: *"Interface natives only serve receivers whose resolved declaring class
IS the interface"* is a claim in a comment, not a compile-time link
(`[comment≠link]`). §10.N2.

---

## 4. H5-C — the nine verdicts, re-checked

### 4.1 The prescription is already satisfied

> *make each registrar's category **explicit rather than ambient** (`set_category`
> save/restore, or `with_category`)*

**There is nothing to do.** All 29 functions in `native-io/src` that call
`register` / `register_with_kind` already open with an explicit
`set_category(...)` and close with `set_category(__prev_cat)`. Audit output
(one row per registrar, generated in §7.3) shows no `*** NONE ***`.
`register_string_rw_natives` additionally uses `with_category`;
`register_process_natives`, `register_t16_channel_overrides` and
`register_io_natives` nest a second scope with their own save/restore.

The crate header's claim that registrations end up `Bridge` *"through a callee
that never sets a category of its own"* is stale. Corrected in `be4c2fbd5`.

`register_as` is confirmed absent from this tree — `grep -rn "register_as("
native-api/src/ native-io/src/ vm/src/` returns nothing (a bare `register_as`
grep matches `register_async_socket_real`, which is what makes the claim look
false at a glance). The P0 row that prescribes it prescribes a symbol that has
never existed here.

### 4.2 The four "confirmed bridges"

| Entry point | Row says | Measured | Verdict |
|---|---|---|---|
| `sun_nio_ch_net` (net.rs:4034) | bridge | `sun/nio/ch/Net`: **25 NAT, 0 shadow**. The registrar also covers `MulticastSocket` (6 shadow), `UnixDispatcher` (absent class), `jdk/net/*SocketOptions` | **CONFIRMED for its core.** Tag `Bridge`, correct. |
| `process` (process.rs:5117) | bridge | `ProcessImpl` 10 NAT, `ProcessHandleImpl` 7 NAT, `$Info` 2 NAT — **and** `java/lang/Process` 7 shadow + 6 abstract, `cratonvm/synthetic/*` 25 absent-class | **CONFIRMED, and already correctly split**: process.rs tags the fabricated half `SyntheticStub` with a written §1.5 argument. Best-in-crate example. |
| `random_access_file` (random_access_file.rs:550) | bridge | `java/io/RandomAccessFile`: **10 NAT, 17 shadow** | **PARTLY WRONG.** Only 10 of 27 are bridges; 17 shadow concrete bytecode and are tagged `Bridge`. Note the crate registers `getFilePointer()J` in two places (§2.B). |
| `nio_natives_real` (nio_native.rs:1182) | bridge | `sun/nio/ch/IOUtil` 8 NAT, `FileKey` 1 NAT, `FileChannelImpl` 0 NAT / 1 shadow / 9 absent-method, `NativeThread` 2 shadow / 2 absent | **PARTLY WRONG.** Bridge-shaped at its core, with an absent-method tail this record did not chase. |

### 4.3 The five "confirmed stubs" — and the trap in the trap

`HANDOFF-20260819.md` §1 is right that `SyntheticStub` is not
`allowed_in(JdkOnly)` and that retiring a stub moves strict mode by zero. **The
inverse matters more here: a registrar whose *verdict* is "stub" but whose
*registered tag* is `Bridge` is not dropped by strict mode at all.**

| Entry point | Row says | Registered tag | Adjudication | Strict mode |
|---|---|---|---|---|
| `stream_decoder` (stream_decoder.rs:949) | stub | **`SyntheticStub`** | 10 shadow, 0 NAT | **dropped.** Row correct. |
| `stream_encoder` (stream_encoder.rs:896) | stub | **`SyntheticStub`** | 12 shadow, 0 NAT | **dropped.** Row correct. |
| `string_rw` (lib.rs:11892) | stub | **`SyntheticStub`** | `StringWriter` 12, `StringReader` 9, `CharArray*` 14, … | **dropped.** Row correct. |
| `scanner` (lib.rs:7692) | stub | **`Bridge`** | 37 shadow + 2 interface, 0 NAT | **LIVE.** Row misleading. |
| `data_stream` (lib.rs:12674) | stub | **`Bridge`** | 25 shadow + 4 interface, 0 NAT | **LIVE.** Row misleading. |

Both `Bridge` tags are *deliberate and evidenced*: each registrar carries a
"RETAG ATTEMPTED 2026-08-19 AND REVERTED" block naming the vector that broke
(`RJdkIntrinsics3: findWithinHorizon(String, 0) expected "42", got null` for
scanner; `RDataInputFastPull: skipped.next = -19` for data_stream). Both revert
notes are correct and neither was disturbed. **What was wrong is the prose above
them**, which still said the `Bridge` came from inheritance — fixed in
`be4c2fbd5`.

`watch` (watch.rs:823) is a sixth `SyntheticStub` registrar the row does not
name.

---

## 5. VERIFICATION PLAN

Nothing below has been run. Build the three arms as usual and diff.

### 5.1 The one registry row that must change

`--dump-native-registry`, both `--real-jdk` (compatible) and `--jdk-only`:

| Row | Before | After (PREDICTED) |
|---|---|---|
| `java/io/FileInputStream.read([BII)I` | `kind: bridge`, `kind_chosen: true`, `overwrote: synthetic-stub`, site `native-io/src/lib.rs:6584` | `kind: synthetic-stub`, `kind_chosen: true`, **`overwrote` absent**, site `native-io/src/lib.rs:6499` |
| `java/io/FileInputStream.readBytes([BII)I` | `kind: bridge`, `acc_native: true` | **unchanged** |
| every other `java/io/FileInputStream.*` row | — | **unchanged** |

**Falsifier:** if the after-row is anything other than `synthetic-stub` at
lib.rs:6499, the `prior_chosen_kind` reasoning in §1.3 is wrong and the change
must be reverted. If the row is *absent* from a `--jdk-only` dump, that is the
expected refusal, not a failure.

### 5.2 The strict-mode report

`cratonvm --jdk-only --jdk-only-report r.json`, union across the `--jdk-only`
corpus, `violations[]` filtered to `kind == "native-shadows-bytecode"`:

- **PREDICTED:** the triple `java/io/FileInputStream.read([BII)I` disappears
  from the list entirely (the registration is refused, so it can no longer
  shadow). Total count falls by exactly 1 per vector that saw it.
- **Falsifier:** the count falls by more than 1, or any other
  `java/io/FileInputStream` row changes outcome. Either means the delete had a
  reach this record did not predict.

### 5.3 Vectors

Run the full 102-vector arm, not the 36. The I/O vectors named in the brief are
the ones that touch this code:

| Vector | Expectation | Why |
|---|---|---|
| `RDataInputFastPull` | **must stay green** | Exercises `register_data_stream_natives`, untouched. If it moves, something other than the FIS delete moved. |
| `RFileTimes` | **must stay green** | The vector that caught the `FileOutputStream` real-layout defect; it does `new FileOutputStream` + `write` + `close`, and reads `lastModified`. Closest thing to a `FileInputStream` public-surface test. |
| `RNioNoFollow`, `RFsSingleton` | **must stay green** | `java.nio.file` paths, untouched. |
| `RChannelInterrupt`, `RSocketChannelInterrupt`, `RJdkNet` | **must stay green** | `sun.nio.ch` / `Net`, untouched. |
| anything reading a file through `InputStream.read(byte[],int,int)` | **watch this one** | The only behavioural surface of H5-A: the real wrapper's bounds check now runs before `readBytes`. A new `IndexOutOfBoundsException` where there was none, or a changed exception *message*, is the expected shape of a regression. |

**A green 102-vector arm does not close §3 or §4.** Those are maps, and the
corpus has no vector that instantiates an application subclass of
`DatagramChannel` or closes a non-Scanner `AutoCloseable` through the bare
interface — which is precisely the population §3.5 is about.

### 5.4 What a reader should re-derive rather than trust

Every number in §2 and §3.4 comes from a static parser plus `javap`. Re-run
§7 before quoting any of it. Three counts in this record's own subject row were
measured stale this week; assume the same of these.

---

## 6. Where existing records and rows are WRONG about the tree

Backed by a grep or a `javap` line each. This is the highest-value section.

**6.1 The JDK path 66 records cite does not exist on this host.**

```console
$ ls "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
ls: cannot access ...: No such file or directory
$ command -v javap
/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot/bin/javap
$ grep -rl "Eclipse Adoptium" docs/known-issues/jdk-only/ | wc -l
66
```

`HANDOFF-20260819.md`'s header is one of the 66, and so is
`native-io/src/lib.rs`'s ByteBuffer layout comment. Same JDK *version*
(25.0.3+9), different vendor build and different path. Resolve it:
`JDK="$(dirname "$(dirname "$(command -v javap)")")"`.

**6.2 The P1 row's "registered on the abstract public API instead of the
`sun.nio.ch.*Impl` classes" prescribes a move that would break the families.**
§3.2. The receivers *are* the abstract classes, by fabrication, at 13 named
allocation sites.

**6.3 The P1 row names `java.nio.channels.Pipe` as an example of the wrong
placement. Pipe registers on both.** pipe.rs:1409/1438 register the
`sun/nio/ch/{Source,Sink}ChannelImpl` rows; pipe.rs:1427/1449 add the abstract
ones. §3.3.

**6.4 The P1 row's counts are low, and `H1-1` explains why every count in this
directory is.** Row: 86 `ACC_NATIVE`, 307 shadowing, 105 abstract, 91 absent
methods, 54 absent classes. Re-derived from source + `javap`: **106 / 656 /
222 / 367**, on **18** absent classes (106 registrations). The 54-vs-18 gap is
the largest and I cannot reconcile it — the row may be counting registrations,
or a different image set. **Neither figure should be quoted without saying
which.**

`H1-1` (merged from the branch tip while this lane was running) found the
mechanism behind the general problem: `--jdk-only-report`'s observation sink
capped at 256 entries and only a boolean said so, and the strict census over
104 vectors reports **1403** `native-won` shadows where the P0/P1 rows quote
943. **Every shadow count in `docs/known-issues/jdk-only/`, including the 307
in my own row and the 656 in this record, is a floor of unknown depth.** My 656
is a floor for a different reason — it is a *static* count that cannot see which
registrations a given boot reaches — so the two numbers are floors of different
things and must not be subtracted.

**6.5 The P1 row calls `random_access_file` a confirmed bridge.** It is 10
`ACC_NATIVE` and **17 shadows** (§4.2).

**6.6 The P1 row calls `scanner` and `data_stream` confirmed stubs.** Both are
registered `Bridge` and are therefore **live under `--jdk-only`** (§4.3). The
"stub" is the verdict, not the tag, and the distinction is the whole of whether
a family is strict-mode debt.

**6.7 `native-io/src/lib.rs`'s own comment at the duplicate said the two
registrations had different callbacks.** They had the same one,
`native_fis_read_bytes` (§1.3). That false premise is why the line survived two
census passes.

**6.8 `register_scanner_natives`'s header said "2 on abstract
`Readable`/`Iterator` methods".** This crate registers **nothing** on
`java/lang/Readable` or `java/util/Iterator` — the two abstract targets are
`java/io/Closeable` and `java/lang/AutoCloseable` (§3.5). Fixed in `be4c2fbd5`.

**6.9 `register_scanner_natives` and `register_data_stream_natives` both said
their `Bridge` was inherited.** Both set it explicitly (lib.rs:7692, 12674).
Fixed in `be4c2fbd5`.

**6.10 The H5-C brief's premise ("make each registrar's category explicit rather
than ambient") is satisfied throughout this crate already.** §4.1.

**6.11 `native-io/src/lib.rs`'s `CRATONVM_REAL_RAF` gate says "Default (unset) =
synthetic".** It is not, for `getFilePointer()J` — see §2.B. One of the ten
methods the gate lists is served by the `Bridge` in `random_access_file.rs` in
both settings of the flag, because that registrar runs six lines later and
registers unconditionally.

**6.12 A caution about my own numbers, not someone else's.** The absent-class
column is measured against a **Windows** JDK image. `sun/nio/fs/UnixWatchService`,
`sun/nio/fs/PollingWatchService`, `sun/nio/fs/UnixNativeDispatcher`,
`sun/nio/ch/EPoll`, `EPollPort`, `EPollSelectorProvider`, `EventFD`,
`KQueuePort` and `UnixDispatcher` are **present** on a Linux JDK 25 image.
Reading them as "absent classes" on this host is `[false everywhere=absence]`.

**6.13 A census that resolves an identifier out of scope invents duplicates.**
My first parser pass reported eight `java/util/Scanner` triples duplicated
across `register_nio_natives`; they are `java/nio/*Buffer` rows reached through
a loop variable named `c`. Recorded in §2.B because the next person will write
the same parser.

---

## 7. Method, so it is reproducible

Scripts live in the session scratchpad and are worth re-creating rather than
hunting for; each is under 120 lines.

**7.1 The parser.** Regex `\.(register|register_with_kind)\s*\(` over
`native-io/src/*.rs`; for each hit, match three arguments that are either string
literals or identifiers; resolve identifiers against (a) `for X in [ … ]` loop
lists in the enclosing top-level function, expanding to every element,
(b) function-scoped `let`/`const` string bindings, (c) module-level
`const`/`static` string bindings. **The scoping rule in (b) is load-bearing —
see §6.13.** 1,113 call sites, 1,098 resolved, expanding to 1,493 registrations
over 1,457 distinct triples.

**7.2 The adjudicator.** For each distinct `(class, method, descriptor)`, run
`javap -p -s <dotted class>` once per class (cached), pair each member line with
its following `descriptor:` line, and classify: `native` in the modifier list →
`ACC_NATIVE`; `abstract` → abstract; class resolves but no member matches →
absent-method; `javap` errors → absent-class.

**7.3 The category audit.** For each top-level `fn` span, list every
`.set_category(` / `.with_category(` inside it that is not in a comment; report
spans that register natives with none.

**7.4 `native-io/src/lib.rs` line numbers, base → merged HEAD.** Only the ones
a reader is likely to chase; grep the symbol instead where you can.

| Symbol / literal | `26e4b5db4` | `fb67a921b` |
|---|---:|---:|
| `pub fn register_io_natives` | 6153 | 6181 |
| its `set_category(Bridge)` | 6155 | 6183 |
| the `SyntheticStub` block open / close | 6491 / 6533 | 6519 / 6561 |
| the deleted duplicate `read([BII)I` | 6584 | *(gone)* |
| `"sun/nio/ch/UnixDispatcher"` (the registration) | 6694 | 6760 |
| `fn native_scanner_close` / its receiver guard | 5798 / 5826 | 5798 / 5827 |
| `fn register_scanner_natives` / its `set_category` | 7674 / 7692 | 7753 / 7771 |
| `"java/io/Closeable"` / `"java/lang/AutoCloseable"` | 7847 / 7848 | 7926 / 7928 |
| `fn register_string_rw_natives` | 11879 | 11958 |
| `fn register_data_stream_natives` / its `set_category` | 12649 / 12674 | 12733 / 12758 |
| `try_alloc_synthetic(… "java/nio/file/WatchEvent$Kind" …)` | 22730 | 22733 |
| `… "java/nio/file/Path" …` (two sites) | 22786 / 23401 | 22789 / 23404 |
| `… "java/nio/file/WatchService" …` | 22835 | 22838 |
| `… "java/nio/file/WatchKey" …` | 23013 | 23016 |
| `… "java/nio/file/WatchEvent" …` | 23120 | 23123 |
| `… "java/nio/channels/DatagramChannel" …` | 24297 | 24300 |

`pipe.rs`, `net.rs`, `nio_native.rs`, `socket_channel.rs`, `async_socket.rs`,
`random_access_file.rs`, `stream_{de,en}coder.rs`, `watch.rs` and
`nio_selector.rs` were not edited by this lane; their numbers are unchanged.

**7.5 What none of this can see.** Anything decided at run time — which of two
registrations a boot actually reaches, `owns_slot`, `invocations`, and whether
dispatch keys on the receiver's class or the constant-pool class. That is
`--dump-native-registry` and `--jdk-only-report`, and it is why §3.3 and §3.5
end in nominations rather than deletions.

---

## 8. OUT-OF-FILE EDITS REQUIRED

**None.** Every change in this lane is inside `native-io/src/lib.rs` and this
record. No edit is requested in `native-collections/src/lib.rs`,
`native-builtins/src/**`, `native-api/src/**`, or any lane-owned file.

One thing an owner of `native-api/src/no_image_receiver.rs` may want to check,
as information rather than a request: the 18 classes in §3.4's last row are the
population that module exists to demote. If `sun/nio/ch/EPollSelectorProvider`
and friends are **not** in its list, seven `Bridge` registrations on classes no
Windows image declares are reaching `--jdk-only` untagged.

---

## 9. Interaction with the rest of wave H (post-merge re-check)

This lane's worktree was cut at `26e4b5db4`; the branch was at `59e5fd8d0`.
Merged at `fb67a921b`, clean, no conflicts. What the 48-file gap contained and
whether it disturbs anything above:

| Gap content | Touches this lane? |
|---|---|
| **`native-io/` itself** — one comment block in `process.rs` (+33 lines, `H3-1`): "WHERE `java/lang/Runtime.exec` IS *NOT*" | **No registration changed.** `git diff --stat 26e4b5db4 59e5fd8d0 -- native-io/` is `process.rs \| 33 +++`, comment-only. **The §2 census and the §3.4 map stand unchanged**, re-checked after the merge. |
| `H3-1`'s correction that the six `java/lang/Runtime.exec` overloads live in `native-builtins/src/lib.rs`, not `process.rs` | Confirmed against the merged tree, and it agrees with my own map: `java/lang/Runtime` appears in **no** `native-io` registration. My §4.2 verdict on `process` (bridge core + already-split synthetic half) is unaffected — `exec` was never part of the 59 rows that registrar owns. |
| `H2-1` — `native-builtins/src/phases_late/nio_file.rs` FILETIME encoding, and **eight `sun/nio/fs/WindowsFileAttributes` shadows retired** in `native-api/src/retired_shadow.rs` | **No overlap.** `native-io` registers nothing on `sun/nio/fs/WindowsFileAttributes` — its only `sun/nio/fs/*` rows are the four watch-service classes in `watch.rs`, and those are `SyntheticStub`. Checked the other direction too: `retired_shadow.rs` contains **no** `java/io/*` triple, so H5-A's delete is not made redundant by a retirement that already happened. |
| `H2-1` is a `RFileTimes`-shaped change, and `RFileTimes` is in my §5.3 vector list | **Watch this one.** `RFileTimes` exercises `new FileOutputStream(f)` + `write` + `close` **and** `lastModified`. H2-1 changed the attribute half; H5-A changed a `FileInputStream` tag. If `RFileTimes` moves, **do not attribute it to either lane without an isolated build** — this is exactly the `[fix+fix≠]` shape. |
| `H1-1` — the 256-entry observation sink, and the 1403-vs-943 correction | Consumed in §6.4. It does not change any measurement here (mine is static), but it invalidates the row's `307` and my `656` as anything other than floors. |
| merge of `origin/dev` (`d8b40ff8f`) | The orchestrator flagged `nio_selector.rs`, `socket_channel.rs` and `direct_buffer.rs` as touched. **In the range `26e4b5db4..59e5fd8d0` they are not** — the diff for `native-io/` is `process.rs` alone. Those changes were already in `26e4b5db4`. Re-derived rather than taken on trust, per §6's own rule. |
| `vm/src/vm/vm_exec.rs` (+354), `native_override.rs` (+37) | Consolidated `real_protected_stub_class_common` into one definition with tests that the cold path re-inlines no copy of the allowlist. **Strengthens §1.3** — `java/io/FileInputStream` is on that list, at the tip and at the base. |

---

## 10. NOMINATIONS

**N1 — decide whether CratonVM native dispatch keys on the receiver's class or
on the constant-pool class.** Everything in §3.3 and §3.5 hangs on it, and so
does whether ~200 abstract-class registrations in this crate are live or dead
weight. One probe: a `Pipe` opened, its source assigned to a
`Pipe.SourceChannel`-typed local, `read(ByteBuffer)` called, with
`--dump-native-registry` read for `invocations` on
`java/nio/channels/Pipe$SourceChannel.read` vs
`sun/nio/ch/SourceChannelImpl.read`. A non-zero count on the abstract row
answers it in one run.

**N2 — `native_scanner_close`'s guard declines by returning `Ok(None)`, which
completes the call instead of yielding.** A non-`Scanner` receiver arriving via
`java/lang/AutoCloseable.close()V` gets its `close()` swallowed. Either the
guard needs a yield-to-bytecode mechanism, or the two interface registrations
need to go. Probe: a class implementing `AutoCloseable` with a side-effecting
`close()`, used in try-with-resources through an `AutoCloseable`-typed variable,
asserting the side effect happened. This is `[decline masks]`.

**N3 — `sun/nio/ch/UnixDispatcher.close0` is served by `net_close`, not by the
`native_fd_close0` the MulticastSocket fix intended.** The comment is corrected;
the behavioural question is open. Does a `java.net.MulticastSocket.close()`
still release its fd? Probe: open + close a multicast socket, check the fd table.

**N4 — `CRATONVM_REAL_RAF` does not cover `getFilePointer()J`.** The
`if !real_raf_enabled()` block in `register_io_extras_natives` registers ten
synthetic `RandomAccessFile` methods; `register_random_access_file_natives`
runs six lines later and re-registers `getFilePointer` unconditionally, so that
one method is `Bridge`-served in both settings of the flag. The winning
implementation is the *right* one (`getFilePointer` is `public native long` on
JDK 25), which is exactly why nobody noticed. The defect is the gate, not the
callback: a flag whose stated scope is wider than its actual one, which is the
shape `H1-1` and `G89-1` both name. **Probe:** set `CRATONVM_REAL_RAF=1`, take
`--dump-native-registry`, and confirm `getFilePointer` reports
`site: native-io/src/random_access_file.rs` in *both* arms — then decide whether
the synthetic ten should be nine, or whether the RAF registrar should skip this
row when the flag is unset.

**N5 — `java/io/FileOutputStream` has the same public-surface shape
`FileInputStream` has, and has not had the same treatment.** 4 `ACC_NATIVE`
(`initIDs`, `open0`, `write`, `writeBytes`) and 8 shadows, all `Bridge`. The
`SyntheticStub`-block pattern that makes `FileInputStream`'s public surface
yield to real bytecode has no counterpart there. Eight more `Bridge` shadows,
retirable by the same argument as §1 — but each needs its own `javap` check, not
a sweep.

**N6 — the real fix for §3.2 is to stop fabricating instances of abstract
classes.** `process.rs` already shows the pattern: mint under
`cratonvm/synthetic/*`, tag `SyntheticStub`, and strict mode drops the whole
family rather than intercepting the application's subclasses. Thirteen call
sites are listed in §3.2. This is wave-2 scope and it is the actual content of
the P1 row.

**N7 — `java/nio/channels/AsynchronousServerSocketChannel` and
`AsynchronousFileChannel` are abstract with registrations and NO fabrication
site found in this crate.** They are the two families in §3.4 that might
genuinely be movable. If something else allocates them, this record missed it;
if nothing does, the registrations may be reachable only through an
application's own subclass, which is the G88-1 hazard with no upside.
