# Hibernate connection-provider and proxy serialization cluster - fixed

| | |
|---|---|
| **Status** | FIXED and retired 2026-07-12. |
| **Affected scope** | Five documented classes plus four directly linked EOF serialization residuals. |
| **Root cause** | `DataInputStream` native read-ahead violated `ObjectInputStream` block-data cursor ownership. |

## Failure mechanism

`ObjectInputStream$BlockDataInputStream` uses its embedded `DataInputStream`
when a primitive spans an empty block buffer. CratonVM's `DataInputStream`
native fetched an 8 KiB bulk chunk and parked all but the first byte in a
native-side buffer. The shared block-data stream had already advanced past the
custom `readObject` payload, so the following block-data read encountered
`TC_ENDBLOCKDATA` and threw `EOFException`.

The Hibernate session serialization hook exposed this as
`SerializationException: could not deserialize` while the linked
entity-manager and entity-entry tests surfaced the direct `EOFException`.
The serialized wire bytes were identical to HotSpot; only the reader cursor
was corrupted.

## Fix

`../../../../native-io/src/lib.rs` now makes `DataInputStream` read one byte at a time
from its wrapped stream. It no longer prefetches bytes into a private native
buffer, preserving the shared cursor contract required by
`ObjectInputStream$BlockDataInputStream`.

The native regression `data_input_single_byte_read_does_not_prefetch_from_shared_stream`
guards the no-read-ahead behavior.

## Verified on CratonVM with real JDK 25

Documented classes:

```
connections.AggressiveReleaseTest          12/12
connections.BasicConnectionProviderTest     7/7
connections.CurrentSessionConnectionTest   12/12
connections.SuppliedConnectionTest          7/7
proxy.ProxyTest                            16/16
```

Directly linked EOF residuals:

```
jpa.ejb3configuration.EntityManagerFactorySerializationTest  3/3
jpa.ejb3configuration.EntityManagerSerializationTest         1/1
jpa.serialization.EntityManagerDeserializationTest            1/1
engine.spi.EntityEntryTest                                    5/5
```

The focused Java session-factory wire probe also completes a custom
`writeUTF`, `writeBoolean`, `writeUTF` round trip successfully.
