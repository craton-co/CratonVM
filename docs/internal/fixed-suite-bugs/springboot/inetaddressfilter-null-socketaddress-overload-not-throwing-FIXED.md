# Fixed: `InetAddressFilterTests.whenNull()` skipped the default-method null check

**Fixed: 2026-07-18**

## Symptom

`InetAddressFilterTests.MatchesSocketAddressTests.whenNull()` expected
`InetAddressFilter.matches((InetSocketAddress) null)` to throw
`IllegalArgumentException("'address' must not be null")`, but CratonVM called
the lambda implementation of `matches(InetAddress)` directly and returned a
value instead.

## Root cause

Lambda-proxy dispatch was keyed by method name plus argument count and runtime
assignability. That was insufficient for a functional interface which has a
same-named default overload of its SAM: a null reference is assignable to both
`InetAddress` and `InetSocketAddress`, so the VM could not distinguish the
default `matches(InetSocketAddress)` from the abstract
`matches(InetAddress)` SAM.

The loss occurred in all three lambda-dispatch entry points: the interpreter,
shared native-context virtual dispatch, and the JIT virtual-call helper.

## Fix

Lambda dispatch now receives and compares the bytecode call-site descriptor.
Only an exact `(name, descriptor)` match with the lambda's SAM enters the
lambda body. Same-named descriptors fall through to normal interface/default
method dispatch, where the default method performs its required validation and
may then call the SAM itself.

`regression-suite/src/RLambdaDefaultOverload.java` covers both the null
rejection and the non-null default-to-SAM delegation.

## Validation

- `cargo check -p cratonvm-cli`
- `RLambdaDefaultOverload`: passed with JIT and `--nojit`
- Spring Boot 4.1.0-SNAPSHOT
  `org.springframework.boot.http.client.InetAddressFilterTests`: 76/76 passed
  with JIT and `--nojit`

The focused Spring Boot runs emitted existing JUnit harness field-layout guard
warnings but finished with `SBRUNNER_RESULT tests=76 failed=0 aborted=0
skipped=0 containersFailed=0` in both modes; they are unrelated to this fixed
dispatch behavior.
