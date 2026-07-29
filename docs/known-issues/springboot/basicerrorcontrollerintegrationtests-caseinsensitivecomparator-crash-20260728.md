# `BasicErrorControllerIntegrationTests`: `String$CaseInsensitiveComparator.apply()` NoSuchMethodError → fatal internal error

**Status: OPEN — found 2026-07-28**, `craton-rerun-20260728` (1500s-timeout
rerun of the 2026-07-23 residual, see
`apps/spring-boot-suite-runner/RESULTS-20260728.md`). Was `FAIL` on
07-23, `CRASH` (process-fatal) on this rerun.

## Symptom

**Module:** `module/spring-boot-webmvc`

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/String$CaseInsensitiveComparator.apply(Ljava/lang/Object;)Ljava/lang/Object;" caller="org/springframework/http/client/JdkClientHttpRequest.lambda$buildRequest$0(Ljava/net/http/HttpRequest$Builder;Ljava/lang/String;Ljava/util/List;)V @pc=15"
[cratonvm] main-vm run() returned Err: Error in thread "main" internal error: checkcast: not an object reference
[cratonvm] main-vm run() Err (debug): Error in thread "main" internal error: checkcast: not an object reference
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard2/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.error.BasicErrorCon-957a1d0f4289.err.log`

This is the same test class as
[`http-parse-url-query-only-authority-split-regression-20260723.md`](http-parse-url-query-only-authority-split-regression-20260723.md)
(a `http_parse_url` fix that was silently reverted by an intervening
merge, causing this class to FAIL with a "bad port" error on 07-23). **This
is a different symptom, not a recurrence** — the error path this run
never mentions `http_parse_url`/port-parsing at all; it fails much later,
inside a real `JdkClientHttpRequest` HTTP client call
(`org.springframework.boot.testsupport.web.servlet.error` integration
test exercising a real Tomcat + real `java.net.http.HttpClient` round
trip), on a completely different mechanism.

## Root cause — not confirmed, strong hypothesis

`java.lang.String.CASE_INSENSITIVE_ORDER` is a real, named
`Comparator<String>` instance (`String$CaseInsensitiveComparator`, whose
single abstract method is `compare(String, String)`), not a lambda. The
crash shows something invoking `.apply(Object)` — the `Function<T,R>`
single-abstract-method signature — on that instance instead. `Comparator`
and `Function` are different functional interfaces; a real JDK would never
produce this call shape from ordinary bytecode, since `CaseInsensitiveComparator`
was never made to implement `Function`.

`vm/src/vm/vm_object.rs:1236-1251` (`pre_init_string_statics`) documents
that `java/lang/String`'s real `<clinit>` — which is what actually
constructs and assigns `CASE_INSENSITIVE_ORDER` — is **not run**;
CratonVM only pre-seeds a handful of specific static fields
(`COMPACT_STRINGS`, `LATIN1`, `UTF16`) to avoid cascading class loads, per
the function's own doc comment. `CASE_INSENSITIVE_ORDER` is not among
them. Two candidate mechanisms, neither confirmed:

1. `CASE_INSENSITIVE_ORDER` genuinely never gets initialized/assigned by
   any path, and something downstream substitutes a synthetic/adapter
   object in its place that was built assuming a `Function`-shaped SAM
   (a `Comparator`-vs-`Function` mixup in whatever code paths CratonVM's
   lambda/method-reference machinery uses when it needs *some* callable
   for a comparator-typed slot).
2. A real `CaseInsensitiveComparator` instance does exist and reaches the
   caller correctly, but the specific call site (a JDK 25
   `java.net.http.HttpClient` internal header-name comparison, reached via
   `JdkClientHttpRequest.lambda$buildRequest$0`) is itself invoked through
   an invokedynamic/method-handle bootstrap that CratonVM resolved to the
   wrong target interface method (`apply` instead of `compare`).

Either way, the resulting `NoSuchMethodError` should be a normal catchable
Java exception, not a fatal process abort — the **`checkcast: not an
object reference` internal error immediately after** is the second,
compounding bug: something in the exception-unwind or subsequent-statement
path performs a `checkcast` against a value that isn't a valid object
reference at all (likely a stale/garbage stack slot left behind by the
failed native dispatch), crashing the whole VM instead of propagating a
normal `NoSuchMethodError` up through Java exception handling. Neither
mechanism traced to an exact file:line this session — needs a standalone
repro (`String.CASE_INSENSITIVE_ORDER` used via a real `java.net.http.HttpClient`
request, or more narrowly, passed to any API expecting a `Function`) plus
a JIT/interpreter dispatch trace.

## Affected classes

- `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests`
