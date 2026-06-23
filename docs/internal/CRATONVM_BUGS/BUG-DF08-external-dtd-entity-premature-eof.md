# Bug DF08 — `java.util.Vector` / `java.util.Stack` are entirely broken (add/push silently no-op) → external DTD "Premature end of file"

> **✅ FIXED 2026-06-18** (worktree `C:/craton/CratonVM-tcfull`, branch
> `tomcat-fullsuite-triage`, `native-collections/src/lib.rs`). **The real root
> cause is far broader than mbeans:** `java.util.Vector` and its subclass
> `java.util.Stack` were **completely broken** — every `add`/`addElement`/`push`
> silently no-op'd (`size()` stayed 0). The Vector/Stack natives are registered
> against the shared `native_al_*` (ArrayList) implementations, but `al_slots`
> hardcodes **ArrayList's** field layout `(elementData, size)`, while Vector has a
> **different** layout `(elementData, elementCount, capacityIncrement)` — Vector
> isn't an ArrayList. So the natives read/wrote the wrong slots and the
> `al_is_arraylist_layout` guard (Vector ∉ ArrayList) rejected the receiver →
> every mutation discarded. **Fix:** receiver-class-aware slot resolution
> (`al_slots_for`) that resolves `java/util/Vector.elementData`/`elementCount` for
> Vector/Stack receivers, plus `al_is_list_layout` accepting Vector; threaded
> through `al_state`/`al_set_data`/`al_set_size`/`removeIf`/`replaceAll`. ArrayList
> path is byte-identical (fallback returns the same slots).
>
> This surfaced as the mbeans "Premature end of file" because **Xerces'
> `XMLEntityManager.fEntityStack` is a `java.util.Stack`**: `fEntityStack.push(parent)`
> at the start of the external DTD subset no-op'd → `endEntity()` popped `null` →
> `fCurrentEntity = null` → `throw END_OF_DOCUMENT_ENTITY` (a static `EOFException`)
> → `DTDDriver.dispatch` reports "PrematureEOF". (Not JIT — `--nojit` identical.)
>
> **Verification:** `VectorProbe`/`StackProbe` now correct (`add`/`push`/`pop`/
> nested all work); `ResolverProbe`/`MbeansProbe` external-DTD + real
> `mbeans-descriptors.xml` SAX-parse OK; ArrayList/HashMap regression-clean
> (`AlSanity`, `jakarta.el.TestArrayELResolver` 36/36). Repros in `.tooling/drv/`.
>
> **Impact beyond mbeans:** any code using `Vector`/`Stack` was silently broken
> (legacy collections, Xerces entity/DTD parsing, etc.) — a high-value general fix.

**Severity:** ~~Low–Medium~~ **Medium-High** (general `Vector`/`Stack` corruption,
masked because most code uses ArrayList/ArrayDeque). Non-fatal for mbeans (Tomcat
logs and continues; JMX
MBean descriptors and any external-DTD-validated descriptors don't load).
**Status on CratonVM:** FAIL (parse aborts). **HotSpot:** PASS.
**Run date:** 2026-06-18
**Binary:** dev `13e8c761` (worktree `C:/craton/CratonVM-tcfull`).
**Surfaced by:** every embedded-server start logs
`MbeansDescriptorsDigesterSource ... SAXParseException: Premature end of file`
reading `*/mbeans-descriptors.xml` (which has a `<!DOCTYPE … PUBLIC …>`); also
breaks any XML with an external DTD / external general entity.

## Root cause — narrowed (deep dive, isolated repros in `.tooling/drv/`)

Step-by-step isolation (HotSpot OK / CratonVM FAIL at each tightening):

1. **Not the file/stream read.** `mbeans-descriptors.xml` reads back full (3563
   bytes) via `URL.openStream`, `FileInputStream`, byte-by-byte — but SAX parse
   of it fails (`MbeansProbe`).
2. **Not `Pattern`/general SAX.** A simple inline XML parses fine via
   InputStream/Reader/InputSource (`SaxProbe`).
3. **It's the DOCTYPE's external subset.** A valid *and* an empty external DTD
   both fail; even via a custom `EntityResolver` returning a `StringReader` /
   `ByteArrayInputStream` (no URL opening at all) — so it's **not** URL/stream
   opening (`EmptyDtdProbe`, `ResolverProbe`).
4. **It's specifically a *second* (external) entity.** An **internal** DTD subset
   (scanned from the document entity) parses fine; an **external** DTD subset and
   an **external general entity** both fail (`IntDtdProbe`).
5. **The reads are byte-identical to HotSpot.** Logging the external entity's
   `Reader` (`LogReaderProbe`) shows the *exact same* sequence on both VMs:
   `read(cb,0,64)=41` (the DTD content) then `read(cb,0,8192)=-1` (EOF). So the
   bytes are delivered correctly to Xerces on CratonVM too.

Therefore the divergence is **not** I/O — it is a CratonVM intrinsic divergence
in Xerces' end-of-external-entity bookkeeping. The failure chain (JDK 25 Xerces):
`XMLEntityScanner.load` hits EOF on the external entity → EOF branch (line ~1714)
sets `entityChanged`, and when `changeEntity` is true calls
`fEntityManager.endEntity()`. `endEntity()` restores the parent via
`fCurrentEntity = (fEntityStack.size()>0 ? fEntityStack.pop() : null)` (line
1526/1570). Back in `load`, `if (fCurrentEntity == null) throw
END_OF_DOCUMENT_ENTITY` — a static `EOFException` — which `XMLDocumentScannerImpl
$DTDDriver.dispatch` catches at line 1227 and reports as **"PrematureEOF"**.

So on CratonVM, when the external subset ends, the parent document entity is
**not** restored as `fCurrentEntity` (the `fEntityStack` is effectively empty /
the pop yields null) → the static `EOFException` fires where HotSpot cleanly ends
the external subset. The single push site is `setupCurrentEntity`
(`fEntityStack.push(fCurrentEntity)` at line 873, guarded by `fCurrentEntity !=
null`); the pop is in `endEntity` (1526). Since the reads are identical, the bug
is in how CratonVM executes this push/pop / `fCurrentEntity`-restore bookkeeping
(an intrinsic on the entity-stack or a field/control-flow divergence), **not** in
the stream layer.

## Reproduce

```powershell
cd C:\craton\CratonVM\apps\tomcat
$exe="C:\craton\CratonVM-tcfull\target\release\cratonvm.exe"
# Minimal: any external DTD subset
& $exe -cp .tooling\drv ResolverProbe        # CratonVM: Premature end of file; HotSpot: OK
& $exe -cp .tooling\drv IntDtdProbe          # internal subset OK, external subset FAIL
```

## Next step (requires VM instrumentation)

Black-box isolation is exhausted — the reads are byte-identical, so pinpointing
the exact divergent intrinsic needs an **instrumented VM build** that logs the
`fEntityStack` push/pop and `fCurrentEntity` restore across `startEntity`/
`endEntity` (or traces the static-`EOFException` throw site). Candidate areas:
`java.util.Stack`/ArrayList push-pop on the entity stack, `ScannedEntity` field
bookkeeping, or end-of-entity control flow. Not a one-line fix.
