# C6-3 — a native registered to serve the VM's OWN instance also hijacks the application's

**2026-08-12, lane C6.** A short record, because the lesson is short and the two
witnesses are both written up elsewhere. It exists so the next person who
registers a native "for the object we fabricate" is asked one question first.

## The shape

CratonVM routinely fabricates an instance of a **real JDK class** to stand in
for something it implements natively, then registers natives on that class so
the fabricated object answers correctly. **Registration is per CLASS.** The
application constructs instances of that same class too, and those have real
bytecode, real constructor-assigned state, and a contract the native does not
implement.

The native cannot tell the two apart, because it was never asked to.

## Witness A — no discriminator: `java.util.SimpleTimeZone` (OPEN)

`register_tzdb_offset_natives_for(registry, "java/util/SimpleTimeZone")`
(`lib.rs:20489`) exists so the `SimpleTimeZone` that `alloc_synth_timezone`
fabricates for `TimeZone.getTimeZone(id)` answers from tzdb. Its four bodies all
read the receiver's `ID` field and resolve it.

Every `new SimpleTimeZone(rawOffset, id)` the application builds is captured by
the same four natives — and for that object the `ID` is, by contract, an
**opaque label** that contributes no offset. `getRawOffset()` returns the wrong
number; `getOffset(long)` returns the wrong number; `inDaylightTime(Date)`,
which is *not* registered and therefore runs real bytecode comparing the
hijacked `getOffset` against the real `rawOffset` field, returns a wrong
**boolean** on an object that simultaneously reports it has no DST rule.

Full write-up, measurements and nominations: `C6-2-simpletimezone-id-resolved-instead-of-rawoffset.md`.

## Witness B — a discriminator that works: the `HttpURLConnection` carrier

The same setup exists for `java/net/HttpURLConnection` and its `sun.net.www`
siblings, and it does **not** misbehave, because the natives ask the object
which state model applies before doing anything else
(`http_url_connection.rs:316`):

```rust
fn is_real_carrier(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    matches!(ctx.get_field(this, HUC_CONN_ID), Value::Object(Some(_)))
}
```

Field 0 holds an `Int(-1)` conn-id on a carrier this VM's own `<init>` native
built, and a `java/net/URL` object on one that came from anywhere else. Every
accessor branches on it and uses an identity-keyed side table for the second
case rather than its own slot map. The file's comments record what it cost to
learn that — a real carrier misread through the synthetic slot map made
`getResponseCode()` answer `-1` having sent no request at all.

**Two files in this workspace use two entirely different `HUC_*` slot maps for
what looks like the same object** (`net_phase_e.rs:6571` starts `HUC_URL = 0`,
`HUC_METHOD = 1`; `http_url_connection.rs:106` starts `HUC_CONN_ID = 0`,
`HUC_URL_STR = 1`, `HUC_METHOD = 2`). That is not a bug, and this lane checked
before reporting it as one: `is_real_carrier` routes objects built by
`URL.openConnection()` away from the second map entirely. **Without that guard
it would be a silent field-crossing.** The guard is the only thing standing
between those two maps.

## The question to ask at the registration site

> Does this native answer from the RECEIVER'S OWN STATE, or from something it
> resolves externally (an id, a name, a side table, a registry)?

If the second, and the class is one an application can construct, the native
needs a **per-object discriminator** — a slot or a side-table entry written at
fabrication time — and an arm that runs the real bytecode for everything else.
`ctx.invoke_virtual_bytecode_only(...)` is the escape hatch and is already used
for exactly this in the timezone code itself (`lib.rs:20031`).

Registering on a superclass does not avoid the problem, it widens it. It also
introduces a second failure this lane could not resolve without a build: when a
subclass registration is removed, the **superclass** registration may start
capturing that subclass's receivers through the abstract base — see
`C6-2` NOMINATION 3B, which flags it as unresolved rather than guessing.

## What this record does NOT claim

This lane looked for a wider family and did not find one worth naming.
`register_tzdb_offset_natives_for` is the only class-fan-out registration helper
of this shape in `native-builtins` (one grep, three call sites, all timezone). A
`--dump-native-registry` census of natives that own a slot while shadowing a real
method **that has a Code attribute** returns 2,620 rows, but that population is
overwhelmingly legitimate intrinsics (`Math` 69, `StringBuilder` 55,
`ArrayList` 32) and is **not** a defect list. Shadowing real bytecode is normal
here; answering from a resolved external value instead of the receiver is the
narrow thing that goes wrong, and it is not visible in the dump.

**So this is a review heuristic backed by two witnesses, not a census result.**
Anyone turning it into a sweep should grep for native bodies that read a
name/id-shaped field off the receiver and resolve it, not for shadowing.
