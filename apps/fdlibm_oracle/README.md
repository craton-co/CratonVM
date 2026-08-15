# `fdlibm_oracle` — HotSpot `StrictMath` digest oracle for the fdlibm port

`types/src/fdlibm.rs` is a port of the JDK's `FdLibm` and must be **bit-exact**
with `StrictMath`, not merely close. The hand-picked `*_VECTORS` tables in that
file pin named edges; this oracle covers the space between them.

It hashes result bits over a deterministic corpus and prints one digest per
function. The Rust side rebuilds the identical corpus and digest in
`fdlibm::tests::{unary,binary}_digests_match_hotspot_strictmath`, so ~240k
points per unary function and ~6.2M pairs per binary one cost two `u64`
constants in the repo instead of a vector file.

## Why a digest and not more vectors

The port does signed arithmetic on extracted exponent/mantissa words and relies
on it **wrapping** — which Rust does silently in release and panics on in debug.
Two sites (`atan2`, `IEEEremainder`) shipped with a plain `-` where the C wraps;
22 sibling golden tests stayed green because their vectors never reached the
sign-bit path. The question the tables could not answer was "is the *release*
build, where the wrap is silent, still producing HotSpot's bits everywhere?".
A digest over a corpus that sweeps every exponent and both signs answers it.

## Run

```bash
javac -d . FdlibmOracle.java
java FdlibmOracle                       # digests -> paste into fdlibm.rs
java FdlibmOracle --dump atan2 > hs.txt # per-point, to localize a mismatch
```

Then, on the Rust side:

```bash
cargo test -p cratonvm-types --lib fdlibm            # debug: overflow checks ON
cargo test -p cratonvm-types --lib fdlibm --release  # release: the silent-wrap arm
```

**Both profiles matter and they test different things.** Debug turns every
unintended wrap into a panic; release is where a wrong wrap would quietly
produce wrong bits. A change to this family is not verified until both are green.

## The corpus is duplicated, deliberately

`point(i)` here and `point(i)` in `fdlibm.rs` must enumerate the same sequence:
a structured prefix (every exponent `0..=0x7ff` x 10 mantissas x both signs)
then `splitmix64(i)` reinterpreted as `f64` bits. There is no shared source of
truth across the language boundary, so the digest *is* the cross-check — if you
edit one side and not the other, every digest changes at once, which is the
signature of a corpus change rather than a real regression.

The 10 mantissas are chosen for their **low words**, not their magnitudes:
`lo()` reinterprets the low 32 bits as `i32`, so a difference of two low words
overflows only when they straddle the `i32` boundary. A corpus of "obvious"
mantissas gives low words of `{0, 1, -1}` and cannot reach that path at all.

NaN is canonicalized to `0x7ff8000000000000` on both sides before hashing:
`StrictMath` and the port may each pick a NaN payload, and the existing golden
tables already treat all NaNs as equal.

## Recorded digests (JDK 25, `Eclipse Adoptium jdk-25.0.3.9-hotspot`)

`corpus_size=240960 bin_stride=97`. See the two test functions in
`types/src/fdlibm.rs` for the values in situ.
