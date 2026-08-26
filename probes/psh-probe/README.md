# `psh-probe` — the `ParameterizedSslHandlerTest` stall harness

The netty class this exists for stalls roughly one whole-class run in 163 on
CratonVM and never on HotSpot, and the stall is a Java-level hang with no
exception, no failure and no output. These are the tools that make it
diagnosable. See
`docs/known-issues/netty/parameterizedsslhandlertest-residual-stalls-20260824.md`
for what they found.

## What each piece is

* **`io/netty/handler/ssl/PshProbe.java`** — a Java-side watchdog. It registers
  each awaited netty operation, reports one that has been outstanding for 15 s
  (channel open/active/registered, the owning event loop's `pendingTasks()`,
  and every reactor's stack), and halts the JVM with exit 97 at two minutes.
  It is what turns "the run is slow" and "the run is hung" into two different
  exit codes on a shared host whose load average swings between 18 and 148 —
  a fixed wall-clock watchdog cannot tell them apart and repeatedly did not.

* **`overlay.py`** — generates an instrumented copy of netty's
  `ParameterizedSslHandlerTest` into an overlay directory. Every insertion is a
  registration or a print; `PshProbe.await` calls exactly the
  `syncUninterruptibly()` the test called, so the sequence of netty operations
  is unchanged. Put the overlay directory FIRST on the classpath in a PRIVATE
  copy of the argfile, never in the shared fixture — this host runs many
  sessions at once.

* **`hangloop.sh`** — the load-proof loop. JUnit's `@Timeout` disabled so a
  hang stays a hang, the VM watchdog NOT armed, `PshProbe` deciding. Takes
  `craton` or `hotspot`, so the control is the same command.

* **`huntloop.sh`** — the same loop with `CRATONVM_DBG_CCE_BT=1` and
  `CRATONVM_DBG_VACATED_FRAMES=1` armed, stopping on the first catch. Both are
  terminal-path only, so a healthy run pays nothing.

* **`triage.sh`** — reads a result directory and separates a real stall from a
  merely slow run: the `[WAIT-CENSUS]` `waited_ms` (a stall reads hundreds of
  thousands, a healthy mid-test wait reads single digits), whether one netty
  operation stayed outstanding, and whether tests were still completing.

* **`zerocell-ab.sh`** — the JIT vs `--nojit` A/B behind `probes/JitZeroCellProbe.java`,
  which is why a `private volatile Object` can dump as `Int(0)` without
  anything being wrong.

## Setup

```bash
cd apps/netty-suite-runner
./gen-openssl-args.sh -o /data/nres/ossl.args      # OpenSsl.isAvailable must be true
# prepend your overlay dir to the -cp line of YOUR copy of the argfile
python3 psh-probe/overlay.py                       # writes the instrumented test source
javac -cp "$(sed -n 2p /data/nres/ossl.args)" -d <overlay-dir> \
      psh-probe/io/netty/handler/ssl/PshProbe.java <generated-test-source>
```

The scripts carry absolute host paths for the Azure box they were written on;
they are kept verbatim rather than generalised, because the paths are part of
the reproduction and a rewritten script is an unrun script.
