# Native registry census

`default-build-baseline.json` is the classification of every native method
registered by the **default** (synthetic-stub-free) build, produced by:

```
cratonvm --dump-native-registry test-infra/native-census/default-build-baseline.json -cp vm-cli/tests/resources HelloWorld
```

Each entry is tagged with a [`NativeKind`](../../native-api/src/registry.rs):

- **intrinsic** — a correct fast-path that matches real bytecode (kept).
- **bridge** — a native the VM genuinely needs (no real bytecode to run; kept).
- **synthetic-stub** — *either* a confirmed fake *or* an as-yet-**unclassified**
  registration. The registry defaults every untagged registration to
  `synthetic-stub` on purpose (conservative: nothing is auto-trusted), so this
  bucket is the **audit backlog**, not a count of known fakes.

## Reading the counts

`register_essential_natives` (the ACC_NATIVE real-native surface) is explicitly
tagged `bridge`. The remaining `synthetic-stub` count is mostly real bridges
(charset, method-handles, classloader, concurrency primitives) that simply have
not been individually classified yet, mixed with a minority of genuine fakes
(e.g. the LambdaMetafactory null-stub, `RunnerClassLoader.close` no-op).

Driving that bucket down — classifying each cluster `bridge`/`intrinsic` or
deleting the fake — is the ongoing audit sweep. See
[../../docs/synthetic-vs-real-explained.md](../../docs/synthetic-vs-real-explained.md).

## What this build already guarantees

The app-specific **"fake main" launcher shims** (Jetty/SonarQube/Liberty/… that
exit rc=0 without running the server) and the **synthetic crypto** keys are
**not present at all** in this census — they are gated behind the default-OFF
`app-stubs` and `legacy-synthetic-crypto` features. The default build cannot
short-circuit a real launcher.
