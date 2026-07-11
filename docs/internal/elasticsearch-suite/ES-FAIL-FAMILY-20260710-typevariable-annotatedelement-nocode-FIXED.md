# ES FAIL family - TypeVariable AnnotatedElement no-Code breaks RestClient Mockito tests

Status: RESOLVED

Observed in:
- Run: `esfull-20260710-083851`
- Host: local Windows box
- CratonVM binary: `C:\craton\cratonvm-targets\es-full-local-20260710-083851\release\cratonvm-es-full-local-20260710-083851.exe`
- Suite mode: `craton` / JIT on
- Stopped partial run totals: 367 recorded classes, 69 PASS, 283 FAIL, 15 HANG, 0 CRASH

Family count:
- 3 FAIL rows:
  - `client/rest org.elasticsearch.client.RestClientMultipleHostsTests`
  - `client/rest org.elasticsearch.client.RestClientSingleHostTests`
  - `client/rest org.elasticsearch.client.RestClientTests`

Primary user-visible signal:
```text
org.mockito.exceptions.base.MockitoException:
Mockito cannot mock this class: class org.apache.http.impl.nio.client.CloseableHttpAsyncClient.
```

Root signal under Mockito/ByteBuddy:
```text
Caused by: java.lang.AbstractMethodError:
method java/lang/reflect/AnnotatedElement.getDeclaredAnnotations()[Ljava/lang/annotation/Annotation; has no Code attribute
```

Focused CratonVM proof:
- Run: `esprobe-nocode-annotatedelement-20260710`
- Class: `client/rest org.elasticsearch.client.RestClientMultipleHostsTests`
- Result: FAIL, 6.133s, 10 failures.
- With `CRATONVM_DBG_NOCODE=1`, stderr records:
```text
[DBG_NOCODE] method java/lang/reflect/AnnotatedElement.getDeclaredAnnotations()[Ljava/lang/annotation/Annotation; has no Code attribute | recv_cid=520 recv_class=java/lang/reflect/TypeVariable
```

HotSpot control:
- Run: `esprobe-hotspot-annotatedelement-20260710`
- Same class: PASS, 1.1s.

Evidence:
- Craton stdout: `C:\craton\esfull-20260710-083851\results\esprobe-nocode-annotatedelement-20260710\jit-annotatedelement\logs\client_rest.org.elasticsearch.client.RestClientMultipleHostsTests.out.log`
- Craton stderr: `C:\craton\esfull-20260710-083851\results\esprobe-nocode-annotatedelement-20260710\jit-annotatedelement\logs\client_rest.org.elasticsearch.client.RestClientMultipleHostsTests.err.log`
- HotSpot result: `C:\craton\esfull-20260710-083851\results\esprobe-hotspot-annotatedelement-20260710\hotspot-annotatedelement\results.tsv`

Secondary fallout:
- The same failing test rows also print:
```text
java.lang.NullPointerException: Cannot invoke "java.util.concurrent.locks.ReentrantLock.lock()" because "mainLock" is null
```
- Treat that as teardown fallout until proven otherwise. The repeated root in every Mockito failure is the `TypeVariable` / `AnnotatedElement.getDeclaredAnnotations()` no-Code dispatch.

Interpretation:
- ByteBuddy asks annotation metadata from `java/lang/reflect/TypeVariable` through the `AnnotatedElement` contract.
- CratonVM resolves that call to the interface/abstract method without a usable implementation.
- A tiny direct probe using `String.class` and `String.length()` as `AnnotatedElement` did not reproduce the no-Code error, so this appears specific to the `TypeVariable` implementation path.

Not duplicates:
- This is not a Mockito bug. Mockito is only the first framework to exercise the broken reflection path in this suite slice.
- Keep this as one family doc for the three RestClient unit rows.

## Resolution (2026-07-11)

Fixed by supplying a native `TypeVariable.getAnnotatedBounds()` bridge for both
synthetic `java/lang/reflect/TypeVariable` objects and real
`TypeVariableImpl` objects. The bridge obtains the existing `Type[]` bounds
and wraps every entry in the same `AnnotatedType` implementation used by the
other reflection natives.

Validated on Azure host `20.83.144.174` from isolated worktree
`/data/data/wt-es-typevariable-annotatedelement-20260711-141500`, branch
`codex/es-typevariable-annotatedelement-20260711-141500`, with unique binary:
`/data/data/cratonvm-targets/es-typevariable-annotatedelement-20260711-141500/release/cratonvm-es-typevariable-annotatedelement-20260711-141500-r2`.

Focused JIT-on Elasticsearch run:
`esprobe-typevariable-annotatedelement-20260711-141500-r2`.

- `RestClientMultipleHostsTests`: PASS, 5 tests, 2.233s.
- `RestClientSingleHostTests`: PASS, 9 tests, 2.805s.
- `RestClientTests`: PASS, 10 tests, 3.606s.
- With `CRATONVM_DBG_NOCODE=1`, none of the three target logs contained a
  `TypeVariable`, `AnnotatedElement`, or `getAnnotatedBounds` no-Code marker.

The neighboring `RestClientSingleHostIntegTests` still has separate Basic-auth
assertion failures; it was not part of this three-row TypeVariable family.

Evidence: `/data/data/es-typevariable-annotatedelement-20260711-141500-suite/results/esprobe-typevariable-annotatedelement-20260711-141500-r2/jit-typevariable-annotatedelement-r2/results.tsv`.
