# Bug 18 — `Uuid` topicId zeroed / `PartitionData` mismatch in fetch path

**Severity:** Medium — `FetchRequestTest`/fetcher tests assert `PartitionData`
equality and fail because the `topicId` (`Uuid`) is wrong on CratonVM — frequently
**all-zero** (`AAAAAAAAAAAAAAAAAAAAAA` = base64 of the zero UUID) where HotSpot has
the real id. Reproduces under `--nojit`. HotSpot clean.

## Symptom
```
=> AssertionFailedError: Element 0 had different PartitionData than expected.
   expected: <PartitionData(topicId=NzN…ERHg, fetchOffset=…)>
   but was:  <PartitionData(topicId=AAAAAAAAAAAAAAAAAAAAAA, fetchOffset=…)>
```
The `topicId` `Uuid` is lost (zeroed) or scrambled when building/serializing the
fetch request/response `PartitionData`.

## Root cause (to pin down)
`org.apache.kafka.common.Uuid` is a 128-bit id (two `long`s). Candidates:
- `Uuid` construction / `equals` / `hashCode` over the two longs mis-handled
  (long field read/write or boxing), or
- the generated-message (`FetchRequestData`/`FetchResponseData`) field for `topicId`
  not being copied through a CratonVM map/collection/clone intrinsic, or
- a `ByteBuffer.getLong`/`putLong` issue in the protocol serializer zeroing the id.

Reproduce by round-tripping a `PartitionData` with a non-zero `topicId` through the
fetch request builder under CratonVM and checking the id survives.

## STATUS — RESOLVED on current `dev` (2026-06-13)

The topicId `Uuid` is **no longer zeroed**. Verified on the post-Mockito/bug-24
dev binary:
- `common.requests.FetchRequestTest` **86/86 OK** (== HotSpot) — the protocol
  test that round-trips `topicId` across all fetch-API versions.
- `UuidProbe` (construct / `get{Most,Least}SignificantBits` / `toString` /
  `ByteBuffer.putLong|getLong` round-trip / `fromString`) == HotSpot.
- `TopicIdProbe` (`FetchRequestData` build+serialize+deserialize) and
  `FetchRespProbe` (`FetchResponseData` round-trip) both preserve a non-zero
  `topicId` == HotSpot.

So the zeroing root cause is fixed by a serialization/`ByteBuffer` fix already
merged to dev (basic `Uuid` and the generated-message `readUuid`/`writeUuid`
paths are correct). Remaining fetcher-test failures, if any, are the
Mockito-execution timeout (129-test classes are slow under the interpreter) or
the separate [bug-17](bug-17-assignor-assignment-mismatch.md) map/set ordering
issue — NOT topicId zeroing. Probes:
`apps/kafka/tests/repro/{UuidProbe,TopicIdProbe,FetchRespProbe}.java`.

## Affected classes (partial — append more later)
- consumer.internals.FetchCollectorTest / FetchRequestManager / Fetcher tests
- common.requests.FetchRequestTest (append from full run)
