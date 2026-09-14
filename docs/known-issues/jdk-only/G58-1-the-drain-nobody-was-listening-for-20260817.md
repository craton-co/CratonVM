# G58-1 — the drain nobody was listening for

**Status:** MEASURED (see the banner below; the body is preserved as written). **Provenance:** the "before"
rows in §0 are MEASURED by me at `e7e840264` (`C:/craton/target-rel5`,
`--jdk-only`, diffed against HotSpot 25.0.3+9-LTS); the "after" column is
**PREDICTED** until the next binary runs the vector. Done in-session by the
orchestrator.

This closes **G51-1 N2** (= G44-1 N2), the last of `RSslLiveSession`'s four
recorded row-groups. Together with `G57-1` it accounts for **every remaining
failing row on that vector**.


> **MEASURED 2026-08-17 — the four `drain.conn.*` rows are green, and §6's
> stated risk did not materialise.** Binary from `3fcc8d90f`, `--jdk-only`:
> `RSslLiveSession` is **104 checks, 0 failing**, an empty diff against the
> oracle.
>
> §6 flagged one thing to watch: body drain now recycles the connection view
> for EVERY https carrier, so a currently-green vector that drains a response
> and then asks a session accessor would newly get HotSpot's exception. **The
> full arm is 98 of 99 and no previously-green vector moved.** That was the
> right thing to flag and the right way to settle it — the whole suite, not
> `--only=drain`.
>
> `drain.session.isValid` and `drain.session.getId.length` — the pair that
> separates *recycled* from *destroyed* — are still green, which is what makes
> the recycle correct rather than merely quiet.

---

## 0. The headline

MEASURED at `e7e840264`, `RSslLiveSession`: **104 checks, 8 failing** — up
from the 95/14 G51-1 recorded at `9ae371468`. The six rows that closed in
between were all PREDICTED and are now measured:
`client.sslSession.sameObjectTwice`, `verifier.sameObjectAsGetSSLSession`, and
the four `server.*` rows.

The eight that remain are **exactly two fixes**:

```text
client.peerHost                     = null  WANT localhost   G57-1  (committed, not in this binary)
client.peerPort.isServerPort        = false WANT true        G57-1
attrs.shadow.peerHost               = null  WANT localhost   G57-1
attrs.shadow.peerPort.isServerPort  = false WANT true        G57-1
drain.conn.cipherSuite.raises       = none  WANT java.lang.IllegalStateException   THIS RECORD
drain.conn.cipherSuite.message      = none  WANT connection not yet open           THIS RECORD
drain.conn.sslSession.raises        = none  WANT java.lang.IllegalStateException   THIS RECORD
drain.conn.sslSession.message       = none  WANT connection not yet open           THIS RECORD
```

`CK RSslLiveSession fails=8` against the oracle's `fails=0`. Check counts are
equal on both VMs (104), so nothing is being skipped — these are eight
answers, not eight absences.

## 1. What was missing was the consumer, not the mechanism

`a1cfdb122` (G48-1) built the whole input-side hook: `BaisEvent { Eof, Close }`,
`BaisEventHook`, `install_bais_event_hook`, `dispatch_bais_event`, the three
dispatch sites in `native-io` (`native_bais_read`'s `pos >= count` arm,
`native_bais_read_bytes`'s, and `native_bais_close`, which was a bare
`Ok(None)` no-op), and `native-io`'s own recorder tests.

**Nothing ever installed a hook.** `dispatch_bais_event` has been reached on
every exhausted read in the process since that commit and has returned `Ok(())`
from its `None` arm every time. This record adds the one consumer.

That is worth naming plainly, because it is the second time in this session
that a mechanism was found complete and unreached — `record_local_cert_chain`
existed only in a doc comment (G51-1 §3c). Building the API and building the
caller are separate acts, and a green build proves neither happened.

## 2. The measured contract

HotSpot, `RSslLiveSession`'s `drainTrap` family: once the response body is
fully drained the connection returns to the `KeepAliveCache`, and every
CONNECTION-level accessor throws `IllegalStateException: connection not yet
open` again — the same exception a never-handshaked connection throws. Meanwhile
**the `SSLSession` object the application already holds stays valid**;
`drain.session.isValid` and `drain.session.getId.length` are green today and
must stay green.

That pair is the whole contract. It is the row that separates *recycled* from
*destroyed*, and it is why the observer recycles the CARRIER's view and never
touches the session object.

CratonVM reads the body to completion inside `perform` and hands it over whole,
so the drain is the only observable "the application is done with this
exchange" instant — and it is observable in `native-io` and nowhere else.

## 3. The design, and the two things that constrained it

**Keys, not references.** The observer is handed the stream and nothing else,
and the carrier may by then be unreachable from any root it can see. So
`https_response_streams` maps stream identity to a `NativeObjKey` — two
integers, holding nothing alive, stable across relocation. An `ObjectRef`
stored across an arbitrary span of Java execution would be a vacated
from-space address; a global root would be the leak this change exists to
remove. `forget_https_carrier_session_by_key` and
`https_recycle_carrier_by_key` are the by-key entry points that follow from
that, and both existing by-object functions now delegate to them.

**A table, not a field.** The object handed to Java is a
`java/io/ByteArrayInputStream` with the JDK's own four-field layout; there is
no spare slot and writing one would corrupt a real field — the same rule
`HttpsPeerInfo`'s own comment states for the carrier.

**The association is registered on the BAIS, never on the
`SequenceInputStream`** the truncated path wraps it in: the BAIS is what
`native-io` observes, and the wrapper emits no `BaisEvent` of its own.

**Bounded.** A row is inserted only for a carrier that already has an
`https_peer_info` entry, so plain `http:` requests — and every unrelated
`ByteArrayInputStream` in the process — add nothing. The first `Eof` or
`Close` removes it. A stream neither drained nor closed leaves one two-integer
row, which is strictly less than what this removes: an unrecycled carrier
holds a GC root on an `SSLSession` for the life of the process.

**No registration is added**, so `bridge-ratchet.sh` and the baselines under
`scripts/` do not move. Every design that instead added a `Bridge` over
concrete bytecode — a dedicated response-stream class, a `SequenceInputStream`
sentinel, a second `close()V` registration — would have needed a baseline
refresh; they are enumerated and rejected in G48-1.

## 4. Idempotence is required, not defensive

`BaisEvent::Eof` fires on **every** exhausted read, not on the transition —
its own doc explains why the transition is not observable from inside the read
body (a stream constructed empty is at `pos >= count` from its first read).
A drained-and-then-closed stream therefore produces `Eof` *and* `Close`. The
row is removed by whichever arrives first, so the recycle happens once.

## 5. What is guarded

Two tests, green (`cargo test -p cratonvm-native-builtins --lib`, 11 passed in
the drain/recycle family, 0 failed):

* **`draining_the_response_body_recycles_the_carrier`** — drives the observer
  directly. Asserts the carrier's view is torn down, that the **row survives**
  (`https_ensure_exchanged` reads a missing entry as "never handshaked" and
  would re-issue the request over the network — which is why
  `https_recycle_carrier` flips a flag instead of removing), that the
  association is consumed by the first event, and that a second event is inert.
* **`an_unassociated_stream_recycles_nothing`** — the case that matters most
  in production. **Every** `ByteArrayInputStream` in the process now reaches
  this observer: a plain `http:` body, an application's own buffer, a resource
  read through `URLClassLoader`. Recycling anything for those would tear down
  state they have nothing to do with.

The dispatch sites themselves are `native-io`'s and already have that crate's
own tests; they are deliberately not re-tested here.

## 6. What this record does not claim

The four rows stay **PREDICTED**. A unit test that drives the observer proves
the observer is right; it does not prove `native-io` reaches it for *this*
stream on the live path, which is a question only the vector answers.

The one substantive risk, stated rather than buried: this makes body drain
recycle the connection view for **every** https carrier, not only
`RSslLiveSession`'s. If a currently-green vector drains a response and then
asks a session accessor, it will now get HotSpot's exception where it
previously got an answer. That is the correct behaviour by §2's measurement,
but it is a behaviour change with a blast radius wider than the vector that
motivated it, and the full three-arm suite — not `--only=drain` — is what
settles it.

## 7. NOMINATIONS

**N1 — the residual coercion stores now have names.** Running
`RSslLiveSession` at `e7e840264` prints the ELEVEN survivors G56-1 predicted,
and they are no longer anonymous: `class_id=600 index=4`, `class_id=604
index=1`, `class_id=616 index=0` (primitive-into-reference **stores**), and
one `class_id=735 index=1` that is the other species —
**`pointer-into-primitive`, an `Object` written into a `Z` descriptor**. That
last one is not in G56-1's census shape at all and has never been triaged.
`CRATONVM_DBG_LAYOUT=1` resolves the ids; this is the smallest remaining
coercion surface and it now fits on one screen.

**N2 — G51-1 N3 and N4, both unchanged and still not taken.**
`TlsServerStreamEntry` should carry the accepted peer address
(`rustls_server_accept_within` drops it as `_peer`); and the SSLEngine
session's post-handshake endpoint has no row on any vector, so the vector work
comes first.

**N3 — 23 tests fail in `native-builtins --lib` and predate this session.**
Measured while attributing G55-1, by running the suite at clean `e7e840264` in
a separate worktree and diffing the failing NAMES, not the counts: the two sets
are **identical** — same 23 tests, nothing introduced and nothing fixed. The
counts alone were misleading (25, then 24, then 23 across three runs of the
same code), and that variance is itself the finding: **this suite is
order-dependent.** They cluster — five in `deprecated_verify`, seven in
`panama`/`panama_libffi`, two in `nio_file`'s glob translation, two in
`net_phase_e`'s header table — and several read as order-dependent rather
than genuinely broken (`is_default_hostname_verifier(None)` failing, a
`logmanager` NPE, an empty header table) — consistent with shared
process-global state across parallel test threads, which is what the varying
count independently suggests. Nobody has looked at this population. It is the
largest unexamined correctness surface currently in the tree that no vector
covers, and the order-dependence has to be settled first, because until it is
no run of this suite means anything.
