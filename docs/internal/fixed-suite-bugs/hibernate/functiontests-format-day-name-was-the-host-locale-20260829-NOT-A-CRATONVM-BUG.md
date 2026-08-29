# `FunctionTests`/`StandardFunctionTests.testFormat` — the day name came from the HOST locale, on BOTH VMs

**Retired 2026-08-29.** Opened the same day as
`functiontests-format-russian-locale-day-name-20260829.md`, which named the
missing measurement precisely ("HotSpot A/B on this exact class/host") and left
it undone. The A/B was run; HotSpot fails **byte-identically**. This is a
missing harness system property, not a CratonVM defect, and the repair is one
line in a table that exists for exactly this species.

## What was open

Two classes failed the H2 complete-suite run with one signature:

```
org.hibernate.orm.test.query.hql.FunctionTests.testFormat
org.hibernate.orm.test.query.hql.StandardFunctionTests.testFormat

java.lang.AssertionError:
Expected: is "Monday, 25/03/1974"
     but: was "понедельник, 25/03/1974"
```

The open page called it "plausible environmental artifact, not yet confirmed
via a HotSpot A/B on this exact host", and listed two things it had not done:
the A/B, and finding which code path resolves the locale.

## The A/B

Four runs, same box, same `common.args`, same class, one execution each
(2026-08-29, Windows 11 host whose OS locale is Russian; JDK 25.0.3+9-LTS
Temurin; CratonVM `dev` at `84a98929e`):

| VM | `-Duser.language=en -Duser.country=US` | result |
|---|---|---|
| real HotSpot | no | **FAIL** `ok=0 failed=1` — `"понедельник, 25/03/1974"` |
| real HotSpot | yes | PASS `ok=1 failed=0` |
| CratonVM | no | **FAIL** `ok=0 failed=1` — `"понедельник, 25/03/1974"` |
| CratonVM | yes | PASS `ok=1 failed=0` |

`StandardFunctionTests.testFormat` behaves identically on both VMs with the
pair supplied.

The two VMs agree in both arms, so nothing about locale resolution differs
between them here, and the second open question — "which code path resolves the
locale for this `format()`" — dissolves: the value is rendered by H2's
`FORMATDATETIME`, running in-process on whichever JVM is hosting it, against
`Locale.getDefault()`. Both JVMs read that from the OS, which is what they are
supposed to do.

## Why the suite was asking for the failure

hibernate-orm's own Gradle build sets the pair on every test JVM:

```groovy
// local-build-plugins/src/main/groovy/local.java-module.gradle
systemProperty 'user.language', 'en'
systemProperty 'user.country',  'US'
```

`apps/hib-suite-runner/common.args` carried `-Duser.timezone=UTC` — the other
half of the same idea — and not the locale pair. So the harness was running the
suite in a configuration upstream never runs it in, on the one host in the fleet
whose OS locale is not English.

This is the **exact species `required-sysprops.tsv` was created for**, and its
own header tells the story of the previous instance: one missing
`-Dhibernate.testing.bytecode.enhancement...` line cost a full 4579-class run
and produced 305 failures against 4, 246 of them identical on real HotSpot.
`common.args` is generated and untracked, so a `-D` the suite needs has no
authoritative home unless it is in that table.

## The repair

Two rows appended to `apps/hib-suite-runner/required-sysprops.tsv`:

```
user.language	en	hibernate-orm's own Gradle build sets it (...local.java-module.gradle:260); without it every date-format assertion is rendered in the HOST OS locale ...
user.country	US	the other half of the pair Gradle sets (...:261); language alone leaves the region-dependent parts of a format pattern on the host default
```

`run-hib.sh sysprops` now reports `state: 4 (3 injected)` and names both.
`apps/` is `.gitignore`d in this repo, so that table is not carried in git and
this page is the durable record of what it must contain — a fresh host that
reports these two classes red should add the two rows before reading anything
else.

## What did NOT need changing

CratonVM. No VM-side code was touched for this finding. In particular
CratonVM honours `-Duser.language`/`-Duser.country` correctly: it fails without
them and passes with them, exactly as HotSpot does.

## The row this leaves behind

`docs/known-issues/hibernate/hibernate-and-hibernate-reactive-not-cratonvm-bugs.md`
carries the public one-line verdict, beside the other host artifacts of the same
family (the Tomcat `Content-Language` assertion, the localized socket-exception
text). This page is the measurement.
