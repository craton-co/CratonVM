# Netty source compilation loses a try-with-resources variable with C1-first disabled

**Status: OPEN.** This is a Java-visible compiler failure, separate from ByteBuf access timing.

On host `64.177.43.150`, HotSpot 25 compiles a fixed 120-source Netty control
into 225 class files. Foundational CratonVM with
`CRATONVM_JIT_CALLEE_DOOR_C1_FIRST=0` instead reports this valid source as an error:

```text
NetUtil.java:195: error: cannot find symbol
    somaxconn = Integer.parseInt(in.readLine());
                                 ^
  symbol: variable in
  location: class SoMaxConnAction
```

`in` is the `BufferedReader` declared by the enclosing try-with-resources
statement. The Java input is valid; changing Netty to accommodate the VM is not a fix.

## Reproduction and controls

The source list contains all 94 buffer main sources and the first 26 common
main sources in sorted path order. Its SHA256 is
`bcf125ef4636d4100f5ffcba07333010fd64d1ba33a44e93068e23c454007e530`.
The historical 120-source list is unavailable, so this is a new explicit control.
Use the same Netty common/buffer classes and JCTools 4.0.7 classpath for every arm:

```sh
CRATONVM_JIT_CALLEE_DOOR_C1_FIRST=0 cratonvm --java-home "$JH" \
  -m jdk.compiler/com.sun.tools.javac.Main -g:none -proc:none \
  -cp "$NETTY_CP" -d "$OUT" @netty-120.sources
```

Both repetitions on the conditional-window candidate fail in foundational mode
with the policy disabled. Both compatible repetitions with that policy disabled,
and both repetitions in each mode with the default enabled policy, succeed with
all 225 class-file SHA256 hashes equal to HotSpot's. The same disabled-policy
failure reproduces on the previously delivered `a0e5b9c57c` runtime; the window
change did not introduce it. The baseline enabled-policy control also matches
all 225 HotSpot class files.

An attempted `CRATONVM_JIT_IR=0` diagnostic is invalid: the launcher explicitly
reports that variable as ignored. It does not isolate either compiler tier.
On reconciled runtime `cbe3f7b297`, the disabled-policy failure remains,
including with the real `CRATONVM_DBG_JIT_SINGLE_PASS_ONLY=1` control.
The enabled policy and `--nojit` each match all 225 HotSpot class files.
This does not isolate an optimizing-IR-only defect. No compiler root cause
has been established; source/package bisection is pending.

The separate startup controls pass 80 processes on reconciled runtime
`cbe3f7b297`, across Spring Boot, Hibernate, JAXB and Quarkus, both VM modes
and both policy settings. These are startup controls, not full framework suites.
A smaller javac diagnostic passed 20 processes and matched both generated class
files on the subsequently rejected scalar-splice build `f2c52646ee`; it is not
acceptance of the final merged source.

Raw inputs, input hashes, exact commands, output hashes and logs are under
`/root/CVM-netty-retire-01a1c7cf-evidence/startup/` on the host. In particular:
`netty-javac-v2-results.json`, `netty-120-inputs.json`, and
`diagnose-javac-v1-results.json` preserve the controls.

## Closure

Repair the shared VM/compiler cause. Keep exact HotSpot class-file parity for
this 120-source input in both modes with both C1-first settings, plus a focused
regression for the isolated cause. Revalidate the reconciled delivered source.
This blocks closing the related architecture proposal's javac validation debt.
