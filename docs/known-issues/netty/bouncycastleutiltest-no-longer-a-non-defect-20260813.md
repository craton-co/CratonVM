# `BouncyCastleUtilTest` — the archived "not a defect" premise no longer holds

**Status:** OPEN, premise changed (2026-08-13). Found on Windows, commit
`ae2e1d9c8`, isolated (`--shards 1`, `--timeout 180`).

## The claim on record

`docs/internal/fixed-suite-bugs/netty-batch11-inet6-and-sha1-oid-CLOSED-20260812.md`,
cause 6:

> `BouncyCastleUtilTest` reports `found=0 started=0` on **HotSpot as well**.
> Its tests are `@EnabledIf`-gated on a BouncyCastle provider that is not on
> this classpath, so nothing runs on either VM.

That was true when measured (2026-08-12). It is not true now.

## Fresh measurement

```
class                                       status  found  ok  failed
CratonVM (ae2e1d9c8, -XX:+UseZGC)           FAIL    2      0   2
HotSpot 25 (same classpath, same day)       PASS    2      2   0
```

Both VMs now **discover 2 tests** (not 0) — the classpath has BouncyCastle
present, unlike when the archived doc was written. HotSpot passes both;
CratonVM fails both. The archived doc's conclusion ("not a defect, matches
HotSpot's 0/0") was correct for the classpath it measured against and is
simply stale for the current one — this is not a case of "the fix regressed,"
it's "the premise the non-defect verdict rested on changed underneath it."

## Not yet done

No investigation into *why* CratonVM fails both tests — no raw log captured
here beyond the summary counts. Whatever changed to put BouncyCastle on the
classpath (a dependency version bump? a `common.args` change?) should be
identified first, since it may explain other classpath-dependent deltas
noticed the same day (e.g. `SslContextBuilderTest`'s HotSpot baseline moving
from documented 9 ok/9 fail to a fresh 21/21). **Answered 2026-08-13:** that
particular shift was netty-tcnative reaching the classpath, so HotSpot gained an
OpenSSL provider CratonVM refused to load — see the retired
`ssl-cert-validation-residuals` write-up. It does not explain BouncyCastle, so
the question this doc asks is still open for BC.

## Repro

```bash
cd apps/netty-suite-runner
echo io.netty.handler.ssl.util.BouncyCastleUtilTest > /tmp/one.txt
CV_BIN=bin/cratonvm-netty-zgc.exe bash run-netty-suite.sh --list /tmp/one.txt --gc zgc --shards 1 --timeout 180 --out /tmp/repro
bash run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```

## Related

- `docs/internal/fixed-suite-bugs/netty-batch11-inet6-and-sha1-oid-CLOSED-20260812.md`
  — cause 6, the now-stale "not a defect" verdict this doc supersedes.
- the retired `ssl-cert-validation-residuals` write-up — another class
  (`SslContextBuilderTest`) whose HotSpot baseline moved the same way. Its
  cause was found (netty-tcnative on the classpath) and fixed on 2026-08-13;
  BouncyCastle's appearance is still unexplained.
