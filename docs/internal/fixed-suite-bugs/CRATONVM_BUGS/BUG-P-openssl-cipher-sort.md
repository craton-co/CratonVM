# BUG-P — `ArrayList.retainAll`/etc. saw zero elements from a LinkedHashMap-backed set (exposed by BUG-O)

**Test:** `org.apache.tomcat.util.net.openssl.ciphers.TestOpenSSLCipherConfigurationParserOnly`
(`testDefaultSort02`). HotSpot: PASS. **Status: FIXED.**

> **ROOT CAUSE + FIX.** Not actually a cipher-data/sort-stability bug. The
> [BUG-O](BUG-O-linkedhashset-insertion-order.md) change backs `LinkedHashSet`
> with a `LinkedHashMap`, whose entries live in the insertion-order *overlay*,
> not the bucket array. `collect_collection_elements` (used by
> `ArrayList.retainAll`/`removeAll`/`addAll` and the new-from-collection
> constructors) only collected a set's elements when the backing map's slot-0
> was a bucket *array* — so it returned **empty** for a LinkedHashMap-backed
> set. `OpenSSLCipherConfigurationParser.moveToEnd` does
> `movedCiphers.retainAll(ciphers)`; with `retainAll` seeing an empty backing
> it dropped everything, so `moveToEnd` never moved RSA to the end and the
> default cipher sort came out wrong. (It "passed" before BUG-O only because the
> hash-ordered `LinkedHashSet.toString()` compared order-independently.)
> **Fix:** `collect_collection_elements` detects a LinkedHashMap backing
> (`is_lhm_receiver`) and collects its keys in insertion order. Verified:
> `ArrayList.retainAll(LinkedHashSet)` correct, `defaultSort` correct,
> `TestOpenSSLCipherConfigurationParserOnly` 5/5. This unblocks BUG-O for merge.
>
> ---
> *Original (incorrect) hypothesis below, kept for the record.*

## Symptom

`testDefaultSort02` (a `ComparisonFailure` over the default-sorted cipher list)
passes on HotSpot but fails on CratonVM **once [BUG-O](BUG-O-linkedhashset-insertion-order.md)
is fixed** (it passed before, with the buggy hash-ordered LinkedHashSet). The
cipher parser feeds a `LinkedHashSet`-ordered cipher collection into a stable
sort, so the BUG-O ordering change altered the sort's tie-breaking. Since
HotSpot uses an insertion-ordered LinkedHashSet *and* passes, CratonVM must have
a **separate** difference in the cipher path.

## Likely cause / next steps

The remaining divergence is in the cipher sort itself, candidates:
- `Collections.sort`/`List.sort` not stable in CratonVM (tie order differs), or
- the cipher strength/encryption tables or the comparator producing a slightly
  different key.

Capture the expected-vs-actual cipher sequence (the `ComparisonFailure` message
is empty at the console because the lists are long — dump
`OpenSSLCipherConfigurationParser.parse("DEFAULT")` directly and diff against
HotSpot), find the first divergent pair, and check whether they are a comparator
tie (→ sort-stability bug, a general fix) or a wrong strength value (→ cipher
table data).
