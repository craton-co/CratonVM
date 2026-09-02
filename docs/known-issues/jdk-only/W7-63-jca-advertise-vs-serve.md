# The JCA provider chain advertises algorithms it will not serve — and serves names it never advertised

**Status: FIXED in source 2026-08-12. VERIFIED AGAINST A BINARY 2026-08-30.
§8's OPEN LIST CLOSED 2026-09-02 — see §8, §8a, §8b and §8c.**

> **Three of §8's six open items had stopped being true before anyone worked
> them**, and each cost one command to check. The `KeyGenerator` default sizes,
> the ML-DSA `getProvider()` NPE and `SUN.getServices()` were all fixed
> elsewhere and had aged into falsehoods on this page. That is §4a's lesson
> turned on this record itself: an open list is a hypothesis with a date on it,
> and `apps/probes/W763Residuals` asks all six in one run.
>
> Of the three that were live: the nine advertised-and-refused `Signature`
> names turned out to be **twenty-six** — nobody had asked SunEC the same
> question, and seventeen of its twenty names failed the same way. All
> twenty-six work now, verified on the signature BYTES. And `--synthetic-jdk`,
> which §8 records as never built by any lane, builds — the first run of it
> refuted this page's claim about `Collections.unmodifiableSet` and found
> `ArrayList` missing `RandomAccess`.
>
> ~~What is left is five `SunTls*` `KeyGenerator` services, and the reason is
> structural rather than clerical (§8c).~~ **Closed the same day** — the
> structural change the bullet named was made, and this VM now advertises
> **335 of HotSpot's 335** services across the five providers while advertising
> nothing HotSpot does not. §8f. The advertise-versus-serve ledger this page is
> named for is at zero; what remains on the open list is one declined trade
> (`LinkedList` is not a `Deque`).
>
> **2026-09-02, later the same day: §8c's second bullet closed too**, and its
> own diagnosis was the thing that was wrong. `unmodifiableList(x) instanceof
> RandomAccess` did not fail because a fabricated class failed to declare a
> marker — it failed because the opcodes and `getClass()` each computed the
> receiver's display class through a different subtype walk, and the opcodes'
> walk was the exception-`catch_type` fallback, which cannot match an interface
> at all. §8d. The bullet had sent the next reader to `native-collections`,
> which was correct throughout.

> **The verification this page asked for, run at last.** The original status
> said: "This lane could not build or run Rust. Every Rust change below is
> source work backed by in-tree unit tests and by a HotSpot 25 oracle; nothing
> here has been observed on a CratonVM binary. The verification command is in
> §9." Eighteen days later, on a release binary of `dev`:
>
> ```text
> CK RJdkSecurity providerSun=true
> CK RJdkSecurity md2=da853b0d3f88d99b30283a69e6ded6bb shake128=5881092dd818bf5c digests=15
> PASS RJdkSecurity (153 checks)      <- CratonVM
> PASS RJdkSecurity (153 checks)      <- HotSpot 25.0.4+7, byte-identical
> ```
>
> Including the three values this record singled out: MD2's digest, the SHAKE128
> primary that §3 #2's correction was about, and the digest count. `SUITE=core
> ONLY=RJdkSecurity` also passes through the suite.
>
> **§9's expected count is superseded.** It predicted the vector would move from
> 61 checks to 80; it is now 153, because other lanes added to `RJdkSecurity`
> over the eighteen days. A count written as an expectation ages into a
> falsehood the moment a shared vector grows — what survived is the ASSERTIONS,
> and those match.

**The advertise-vs-serve distinction this page established is still live, and
still costs people time.** On 2026-08-30 a lane re-derived it from scratch,
publishing and then retracting a claim that services missing from
`provider.getServices()` must raise `NoSuchAlgorithmException`. Measured on the
same day: **186 services are absent from the enumeration and only 84 of them
actually refuse** — 93 resolve through a path the service map does not
advertise, and a full 2048-bit Diffie-Hellman runs 0-diff against HotSpot on a
`KeyAgreement` type that is not enumerated at all. That is exactly this page's
thesis, arrived at the expensive way; that record is
`jca-provider-population-gap-20260830.md`, retired to `internal/` on
2026-09-02.

> **Both halves of this page's title were closed on 2026-09-02**, and the
> numbers above are superseded. The functional gap went 84 -> 5 and the
> enumeration gap 117 -> 9 (the "186" is a count of enumeration LINES; 62 of
> them differed only in the implementation-class string). Twenty-eight of the
> absent services were the SERVES-AND-DOES-NOT-ADVERTISE half named in this
> title — 22 `SecretKeyFactory`, the unlisted `KeyAgreement.DiffieHellman` that
> ran a full 2048-bit agreement, `Signature.NONEwithRSA`, and four
> `AlgorithmParameters` — and they are advertised now.
>
> The ADVERTISES-AND-WILL-NOT-SERVE half is at zero in both directions: this VM
> advertises nothing HotSpot does not (it advertised five such rows on
> 2026-08-30), and the three serviceability ratchets this page created are
> disjunctions now — advertised implies serviceable, either computed by this
> crate or routed to a REAL implementation class, with the
> `com.sun.crypto.provider.Native` marker explicitly not counting as one.
>
> Two of those ratchets could not have failed: each reads its population out of
> the service registry, but only what the seeders it CALLS have put there, so
> rows from a new seeder were invisible to it. A third stated its answer as a
> hand-written literal of names that "must NOT be advertised" and went stale the
> moment they became implementable. All three are fixed.

> **Second pass, 2026-08-12 (JCA advertise-vs-serve lane).** Six of the seven
> dispositions in §3 were re-read against the tree and are present as written.
> **One was not: #2's ALIAS half never reached the surface it claims.** The
> `put_alias` rows landed and the ratchet went green on them, but
> `MessageDigest.getInstance("SHAKE128")` still raised
> `NoSuchAlgorithmException` in both shipping modes. §3 #2 now carries the
> correction and the fix; §9's row `C.md[SHAKE128].abc … (the ALIAS must
> resolve)` was **false against the source** when it was written.
>
> Also this pass: the record's assertions now have a home in a **scheduled**
> vector. `regression-suite/src/RJdkSecurity.java` gains
> `advertisedVersusServed()` — MD2, both SHAKE primaries, both SHAKE aliases,
> the two advertised-implies-serviceable loops, the `Signature` refusals and the
> unmodifiable set, all against HotSpot's own answers. `RJdkSecurity` is in
> `JDKONLY_CLASSES`; `probes/JcaAdvertisedVsServedProbe.java` is not run by
> `regression-suite/run.sh` and never was. **The expected count moves from
> `PASS RJdkSecurity (61 checks)` to `(80 checks)` in all three arms.**

This record **supersedes and closes the residual halves of two others**:

* `W4-3-security-getalgorithms-short-list.md` — Patches A, B, C, D, F.
* `W7-29-jca-advertise-implement-gaps.md` — residuals 1 to 5.

`W7-55-record-reconciliation.md` §8 recorded that those "are the same five
defects seen from two ends. Fix them once." That reading is close and it is not
a measurement. **The true count is seven.** §1 has the arithmetic.

---

## 1. The population, unified

The reconciliation's claim was five-and-five-are-one-five. Adjudicated
name by name against the tree on 2026-08-12:

| # | defect | W4-3 calls it | W7-29 calls it | shape |
|---|---|---|---|---|
| 1 | `MD2` advertised by `SUN` `MessageDigest`, refused by `getInstance` | Patch B | residual 1 | **advertise-but-refuse** |
| 2 | `SHAKE128-256` / `SHAKE256-512` neither implemented nor advertised | Patch C | residual 2 | *neither* — see below |
| 3 | `Security.getAlgorithms` / `Provider.getServices` answer a mutable set | Patch A | residual 3 | *shape of the answer* |
| 4 | `SUN` `KeyFactory` advertises the `ML-DSA` umbrella, refuses it | Patch F | residual 4 | **advertise-but-refuse** |
| 5 | `Signature.getInstance` accepts every string | — | residual 5 | **serve-but-never-advertise** |
| 6 | `compute_digest` / `digest_length_bytes` default to SHA-256 / 32 | Patch D | — | **serve-but-never-advertise, with WRONG BYTES** |
| 7 | `SunJCE` `KeyFactory` advertises the `ML-KEM` umbrella, refuses it | — | — | **advertise-but-refuse** |

**Four are shared. One is unique to each record. One is in neither.**

* **#6 is in W4-3 only.** W7-29's five residuals do not include it. The reason
  is instructive: W7-29 ran its probes against a real-JDK-mode binary, where
  `md_get_instance` gates on `algorithm_supported` first and the fallback arm is
  unreachable. It is live only in `--synthetic-jdk`, which no lane in this
  campaign has ever built (`W7-55` §7). A defect scoped to a configuration
  nobody runs does not show up in a run.
* **#5 is in W7-29 only.** W4-3's census filed `Signature` / SUN as *"7
  advertised, 7 implemented, none"*. It was read from source and it was looking
  the wrong way down the asymmetry — see §2.
* **#7 is in neither.** The census probe found it: `SunJCE` advertises
  `KeyFactory.ML-KEM` and `key_factory::algo_idx` has arms for
  `ML-KEM-512/768/1024` only. Structurally identical to #4, eleven lines away in
  the same seed function, and invisible to both source reads.

The instrument is what found #7, which is the argument for building it. §7.

## 2. Why a census keeps missing half of this

`Security.getAlgorithms(type)` is enumerable. **The set of names an engine will
ACCEPT is not.** Only the first is a list, so a census that compares two lists
can only ever see defects in one direction.

W7-29 stated this precisely for `CertificateFactory` and it generalises to the
whole family: the advertised set was `[X.509]` on both VMs, had agreed the whole
time, and `getInstance("PKCS7")` still returned a working X.509 parser. No
comparison of the two lists could have found it.

So the family splits four ways, not three, and the fourth is the one that gets
mistaken for the others:

* **advertise-but-refuse** (#1, #4, #7). Loud. `getInstance` throws where the
  advertised set said yes. Harmful because callers enumerate the provider to
  decide what is available, so this makes the enumeration a lie — but it fails
  closed, and the caller finds out.
* **advertise-but-mis-serve** (#6 in synthetic mode; historically the ChaCha20
  and `Mac` defects). Silent, and the worst. `getInstance` succeeds and the
  bytes are wrong. **Only a known-answer vector can see this**, which is why §7
  is built the way it is.
* **serve-but-never-advertise** (#5, and #6 read structurally). The advertised
  list never moves, so no census sees it. Ranges from harmless to catastrophic
  depending on what the default arm does.
* **implement-but-don't-advertise.** The least harmful and the easiest to
  mistake for the others: the code is right, the list is short, and nothing
  breaks except that nobody asks for a name nobody publishes. **This is what #2
  was misfiled as, in both records, and it is not what #2 was.** SHAKE was
  neither implemented nor advertised — internally consistent, a plain
  HotSpot-parity gap, not a member of this species at all. W4-3's residual pass
  got this right in prose (*"the audit's residual, taken literally, asks for the
  wrong change"*) and then the summary lines went on calling it "unadvertised".
  It is fixed here because the task asked for it and because `sha3` was already
  a dependency, not because it was the same defect.

#3 is a fifth thing again: not about which names are in the set but about the
**shape of the set itself**. Kept in the population because it is the same
sentence — "this is a view of platform state" — being not-said.

## 3. Disposition, per defect

### #1 `MD2` — IMPLEMENTED (not de-advertised)

RFC 1319 is short and fully specified, HotSpot 25 carries it, and implementing
closes **three** advertisements rather than one: `SunRsaSign` and `SunMSCAPI`
both advertise `MD2withRSA`, which resolves `MessageDigest.getInstance("MD2")`
internally.

`native-builtins/src/lib.rs`, `real_md2` + the `"MD2"` arm of `compute_digest`.

**This lane could not build, so the transcription was adjudicated before it was
written.** The identical algorithm was expressed in Java and run against
HotSpot's own `MessageDigest.getInstance("MD2")` on ten messages — all matched,
including the three padding boundaries (15 bytes, exactly 16, 17). MD2 pads with
between 1 and 16 bytes and **never zero**, so an exact multiple of 16 takes a
full extra block of `0x10`; that is the case a naive implementation gets wrong
and it is why those three rows exist. The ten vectors are now
`real_md2_matches_hotspot_vectors`, which also drives them through
`compute_digest` — a correct `real_md2` wired to the wrong arm name passes the
direct call and fails the dispatched one.

MD2 is cryptographically broken and is present for parity, not for use. Nothing
in the corpus asks for it. It reaches no TLS or signing path added here.

### #2 SHAKE — IMPLEMENTED, then advertised, in that order

`compute_digest` gains `"SHAKE128256"` / `"SHAKE256512"` (the `-`/`/`-stripping
normalisation collapses the JDK spellings), pinned against the HotSpot and NIST
vectors by `shake_matches_hotspot_vectors`. Only then do the two `put_service`
rows land.

**`SHAKE128` and `SHAKE256` are ALIASES, not services.** HotSpot carries
`Alg.Alias.MessageDigest.SHAKE128 = SHAKE128-256`; measured, `getInstance`
resolves both bare spellings and returns bytes identical to the hyphenated
primaries, while `Security.getAlgorithms("MessageDigest")` lists only the two
primaries. Registering them as services would make the advertised count 17 where
HotSpot answers 15. `every_advertised_sun_message_digest_is_serviceable` asserts
both halves — resolvable through the alias map, absent from the advertised list.

The two normalisations that had to agree for one pair of arm spellings to work
in two files now have their own test
(`shake_normalisations_agree_across_the_two_filters`). W7-29 asked for exactly
that, on the grounds that the agreement is a coincidence of these names and not
a property of the functions. It is.

> **CORRECTION, second pass 2026-08-12 — the alias half above was NOT true when
> it was written, and it is this record's own species one layer in.**
>
> `every_advertised_sun_message_digest_is_serviceable` asserts the aliases with
> `get_service_entry("SUN", "MessageDigest", "SHAKE128").is_some()` — a lookup
> in the provider chain's service map. **`MessageDigest.getInstance` never
> reads that map.** `jca::message_digest::md_get_instance`'s only gate is
> `algorithm_supported`, and `algorithm_supported` had no alias arms — its own
> test pinned the bare spellings as *rejected*, justified in a comment saying
> they are "resolved by the provider chain's alias table before this predicate
> is consulted". Nothing on that path consults it. So the ratchet was green on
> a **proxy for the surface it claims to guard**, and
> `MessageDigest.getInstance("SHAKE128")` raised `NoSuchAlgorithmException` in
> `--real-jdk` and `--jdk-only` alike while `Security.getAlgorithms` was
> already correct.
>
> That comment's second premise was false in the same way: admitting the
> aliases at the gate was said to grow `Security.getAlgorithms("MessageDigest")`
> from 15 to 17. It cannot. The advertised set is built by
> `algorithms_for_service` from the SERVICE rows, which `algorithm_supported`
> does not reach in either direction. **A guard justified by a stated premise is
> only as good as the premise, and both of this one's were checkable in the same
> file.**
>
> Fixed here, in `native-builtins/src/jca/message_digest.rs`:
> `canonical_algorithm` resolves the two `Alg.Alias` spellings onto their
> primaries and runs *before* all three name-keyed tables —
> `algorithm_supported`, `digest_length_bytes`, and the `compute_digest` call in
> `md_digest` / `md_digest_into`. The caller's own spelling is still what
> `md_get_instance` stores, so `getAlgorithm()` echoes `SHAKE128` as HotSpot
> does; only the tables see the canonical form.
> `the_shake_aliases_resolve_but_are_not_separate_algorithms` replaces the two
> rows deleted from `algorithm_supported_rejects_unknown` and asserts **both**
> halves — the alias serves, and it is still not a separate advertised
> algorithm — so the pair cannot drift back. The Java-side cover is
> `RJdkSecurity.advertisedVersusServed()`, which compares the alias's bytes
> against the primary's rather than merely asking whether it resolved.
>
> **Not done, and the reason:** the synthetic-mode door is still half shut for
> the aliases. `crate::compute_digest` has arms for `SHAKE128256` /
> `SHAKE256512` only, so a `--synthetic-jdk` caller now passes
> `native_md_get_instance`'s gate (it shares `algorithm_supported_public`) and
> would meet an `IllegalArgumentException` at `digest()` instead of a
> `NoSuchAlgorithmException` at `getInstance`. `native-builtins/src/lib.rs` is
> outside this lane's ownership; the one-line repair is to fold `SHAKE128` →
> `SHAKE128256` and `SHAKE256` → `SHAKE256512` into `compute_digest`'s `upper`
> immediately after it is computed. The two shipping modes are unaffected —
> both go through `md_digest`, which canonicalises.

### #3 the mutable set — WRAPPED

`wrap_unmodifiable` in `provider_chain.rs`, applied at the tail of both
`security_get_algorithms` and `provider_get_services_native`, while the set is
still pinned (the Java round trip can move it), on every path including the
empty ones, per call and never cached.

HotSpot's `Collections$EmptySet` vs `Collections$UnmodifiableSet` split for
`""` / `"Foo."` / unknown-engine-type is **not** reproduced: both are immutable
and both are size 0, which is the entire observable contract.

### #4 `ML-DSA` `KeyFactory` — DE-ADVERTISED

**This is a stop-advertising fix and it changes what
`Security.getAlgorithms("KeyFactory")` returns.** `SUN` no longer lists
`ML-DSA`. The three parameter-set names stay.

Adding the umbrella arm was declined rather than deferred. W7-29 **ran** the
three parameter-set names that already resolve and found the objects broken one
accessor in: `KeyFactory.getInstance("ML-DSA-44").getProvider()` raises
`NullPointerException: Cannot enter synchronized block because "this.lock" is
null`, where HotSpot answers `SUN version 25`. Widening a surface that is
already broken is not a fix.

`Signature` **keeps** advertising `ML-DSA`, because `signature::algo_idx`
genuinely carries `SIG_MLDSA`. The two engines disagreed about one name; the fix
makes each engine truthful about **itself**, which is the invariant, rather than
making the two agree with each other, which is not.

### #5 `Signature.getInstance` accepts everything — GATED

`signature::signature_name_is_offered`, checked before a receiver is allocated,
refusing with `throw_no_such_algorithm_public` in HotSpot's measured wording
(`<name> Signature not available`).

**W7-29's prescription — gate on `find_service_provider("Signature", algo)` —
would have been a regression, and this is the record's second instance of the
`W7-55` §6 shape.** The service registry is seeded with friendly names only,
while `signature::algo_idx` deliberately also carries the signature-algorithm
OIDs (`1.2.840.113549.1.1.11` and neighbours) because X.509 `cert.verify()`
resolves `Signature.getInstance(signatureAlgorithm.getId())` **by OID**. A
registry-only gate refuses every one of those at `getInstance` and breaks
certificate verification outright — the same failure `algo_idx`'s OID arms were
added to fix, reintroduced one layer up.

So the gate is a **disjunction**: a name this engine has an index for, OR a name
some provider in the live chain advertises. That closes both directions at once
and can only fail in one, which is what the ratchet asserts.

W7-29 also asked for the `getAlgorithm()` sentinel `"Unknown"` to be deleted as
unreachable. **It is still reachable and was left in place.** Nine `SunRsaSign`
names (`MD2withRSA`, `SHA3-*withRSA`, `SHA512/224withRSA` and friends) are
advertised, have no `algo_idx` arm, and are therefore admitted by the second
disjunct with `idx == -1`. Deleting the sentinel on the strength of the
prescription would have been a bug.

Those nine still fail at `sign()`/`verify()` with the checked
`SignatureException`. **That is not this record's species and is deliberately
not closed here:** the name is real, the advertisement is truthful, and the
failure is closed and catchable. It is an ordinary unimplemented-algorithm gap.
`every_advertised_signature_name_is_offered_by_get_instance` is written to
assert only the direction this record owns, and says so in its doc comment, so
it does not go red for a gap this lane did not open.

The 42-versus-64 advertised gap must **not** be closed by widening the seed
list. After the gate, 22 of HotSpot's names would each become a
`NoSuchAlgorithmException`, which is the truthful answer.

### #6 the wrong-digest defaults — FAIL CLOSED

`compute_digest`'s `_ => Ok(real_sha256(data))` becomes an `Err`, and
`digest_length_bytes`'s `_ => 32` becomes `Option::None`.

The pairing is the point. A caller asking for an unimplemented digest got 32
bytes of SHA-256 **and** a `getDigestLength()` of 32 corroborating it. Two
independent-looking observations agreeing because they shared one wrong default
is what makes this species so hard to see from inside — it is the same
self-corroboration that let the `Mac` engine report `getMacLength() == 32` for
`HmacSHA224`.

The synthetic-mode door is shut too. `native_md_get_instance` carried an `if`
with an **empty body** and a comment asserting the JDK surfaces the failure
lazily. It does not: HotSpot throws at `getInstance`, measured. Worse, the
literal list that empty `if` tested had drifted — no SHA-224, no SHA-512/224, no
SHA-512/256, no SHA3-224, all of which `compute_digest` implements — so the
check it was not performing would have been wrong in the other direction as
well. It now calls `message_digest::algorithm_supported_public`, the same
predicate the real-JDK path uses. One predicate, two doors.

**There was a THIRD digest-length table, and adding MD2 and SHAKE would have
made it worse rather than better.** `native_md_get_digest_length` in
`lib.rs` — the synthetic-mode `getDigestLength()` — carried its own `match`
ending `_ => 32`, with no arms for MD2, SHA-224, SHA-512/224, SHA-512/256 or
SHA3-224, and a `-`-only normalisation that let `SHA-512/256` reach the default
and report 32 **by accident rather than by arm**. Since `getInstance` now
*admits* MD2 and the SHAKEs, that table would have reported 16-byte MD2 as 32
and 64-byte SHAKE256-512 as 32 — a newly implemented digest contradicting its
own length, which is precisely the defect being closed. It now calls
`digest_length_bytes_public`. One table, every door, and
`every_supported_algorithm_computes_and_has_a_length` now covers this surface
transitively because there is nothing else left for it to disagree with.

Same reasoning applied to §3 #3's surface: the `--synthetic-jdk`
`Security.getAlgorithms` override in `phases_early` deliberately shadows the
registry-backed registration in that mode, so it now wraps through
`wrap_unmodifiable_public` too. Applying the wrapper only to the shadowed twin
would have been a fix invisible in the one mode that code path serves.

### #7 `ML-KEM` `KeyFactory` — DE-ADVERTISED

Same disposition and same reasoning as #4. `SunJCE` no longer lists `ML-KEM`;
`ML-KEM-512/768/1024` stay.

Worth one line on its own: that service row carries a **real JDK class name**
(`com.sun.crypto.provider.ML_KEM_Impls$KF`). That is not evidence the row is
serviceable. `kf_get_instance` intercepts natively and never reaches
`build_jca_impl`, so the class name is documentation. W4-3's census used "carries
a real JDK class name" as its filter for *"serviceable by construction; they are
not the risk"* — that filter is unsound for any engine whose `getInstance` is
natively intercepted, which is most of them.

## 4. What stopped being advertised, explicitly

**Exactly two names.** Both are knowing divergences from HotSpot in the
under-advertising direction, and **something may enumerate them**, so they are
listed here rather than left to the diff:

| type | provider | name | HotSpot 25 | CratonVM before | CratonVM after |
|---|---|---|---|---|---|
| `KeyFactory` | `SUN` | `ML-DSA` | advertised, served | advertised, **refused** | not advertised, refused |
| `KeyFactory` | `SunJCE` | `ML-KEM` | advertised, served | advertised, **refused** | not advertised, refused |

Note the *before* column: `getInstance` already refused both. Nothing that
worked stops working — what changes is only that
`Security.getAlgorithms("KeyFactory")` stops naming two algorithms this VM will
not hand over. In both cases the alternative was to keep advertising a name
`getInstance` refuses. **A missing algorithm is far better than a wrong one, and
better than a lie about a missing one.**

Neither name is asked for by anything in the tree, checked before removal: no
Java source under `regression-suite/`, `vm/tests/resources/` or `probes/`
mentions either, `find_service_provider("Signature", ..)` has no callers at
all, and these `KeyFactory` rows are reached only through `kf_get_instance`.

`Signature`'s 42-versus-64 under-advertisement is pre-existing and untouched;
no `Signature` name was removed.

Two names started being advertised: `SHAKE128-256`, `SHAKE256-512`, both now
implemented. `MessageDigest` goes 13 → 15, matching HotSpot exactly.

## 5. Compatible-mode exceptions taken

Compatible mode (`--real-jdk`) is contractually frozen except for genuine
HotSpot-parity bug fixes. Three deliberate exceptions were already taken in this
area earlier in the campaign (`Mac` now raises `NoSuchAlgorithmException`;
`Cipher` refuses unimplemented transformations; `CertificateFactory.getInstance`
validates its type). This branch takes **three more**, each stated:

1. **`Signature.getInstance` refuses unknown names.** From "returns an object
   that fails much later with the wrong exception type" to HotSpot's own
   `NoSuchAlgorithmException` at HotSpot's own point. Every in-tree Java caller
   asks for a real algorithm and is unaffected. This is the same shape as the
   `Mac` and `CertificateFactory` exceptions and is justified the same way.
2. **`Security.getAlgorithms` / `Provider.getServices` return an unmodifiable
   set.** A caller that mutates the result now sees exactly what it would see on
   HotSpot. Smallest of the three.
3. **`KeyFactory.getInstance("ML-DSA")` / `("ML-KEM")` were already refused; what
   changes in Compatible mode is that they are no longer *advertised*.** This is
   a divergence from HotSpot rather than a convergence, and it is the one
   exception here that does not reduce to parity. It is taken because the
   alternative is a lie, and it is reversible the moment someone implements the
   umbrella arms.

`compute_digest`'s fail-closed default is structurally all-modes but reachable
only in `--synthetic-jdk`, since both real-JDK `getInstance` paths gate first.

## 6. The ratchet at `provider_chain.rs:4043`

`every_advertised_sunjce_cipher_is_serviceable` is **untouched and unweakened**.
Nothing in this branch changes the `SunJCE` `Cipher` seed list, and W4-3's Patch
E — which would have deleted four names from it — remains **DEAD**, marked in
place, and was not applied.

Three ratchets are added beside it, in its shape:

* `every_advertised_sun_message_digest_is_serviceable` — both directions, plus
  the alias half.
* `every_advertised_signature_name_is_offered_by_get_instance` — one direction,
  for the reason in §3 #5, plus an anti-vacuity block proving the gate can say
  no.
* `every_advertised_key_factory_name_is_serviceable` — both directions, plus
  both umbrellas pinned absent-here-present-there.

Each iterates **every provider** in the service map rather than a hardcoded
list. The first draft of the `KeyFactory` one checked `SUN`, `SunRsaSign` and
`SunEC`; it would have missed `SunJCE`'s `ML-KEM` entirely, which is the defect
in this record that neither prior record found. A ratchet over a subset of the
population has a hole in exactly the place nobody is looking.

Each also asserts a minimum row count **before** looping, because the seed map
is process-global and a `reset_service_state_for_tests` race would otherwise
leave the loop iterating nothing and passing. W6-5-vacuous-tests.md is the
campaign's catalogue of that failure mode.

## 7. The instrument

`probes/JcaAdvertisedVsServedProbe.java`, oracle transcript in
`probes/JcaAdvertisedVsServedProbe.expected.txt` (HotSpot 25, Windows,
2026-08-12, 415 lines).

For **every** algorithm the provider chain advertises across sixteen engine
types, it requests the algorithm for real and prints what came back — bytes
where the engine can be driven deterministically without key material, the
exception verbatim otherwise. Four sections, because §2's four shapes need
four different questions asked:

* **A** walks the advertised set. Catches advertise-but-refuse. Cannot catch
  mis-serve — a mis-serving engine prints a healthy `OK`.
* **B** probes 22 names no provider carries. Catches serve-but-never-advertise,
  the direction a list-versus-list census structurally cannot see.
* **C** is known-answer vectors. The only section that can catch a wrong
  algorithm.
* **D** prints the shape of the answer itself.

Two traps designed around, both already paid for in this campaign:

* **A round trip cannot catch a wrong algorithm.** Encrypt-then-decrypt, or
  hash-then-compare-to-itself, through the same wrong primitive succeeds. §C
  therefore prints raw hex against published vectors and round-trips nothing.
* **Comparing two refusals reports `true`.** A probe asking "do X and Y agree"
  that gets `NoSuchAlgorithmException` from both compares one exception name
  with itself and answers `true` — which reads exactly like the defect it is
  hunting. `sameBytes` answers `n/a` unless both sides produced real bytes,
  following `sameCipher` in `probes/CryptoTrioProbe.java`.

The anti-vacuity row is `C.md.fallbackEqualsSha256`, and on the oracle it reads
`n/a`. HotSpot refuses `NO-SUCH-DIGEST`, so nothing is produced, so the
comparison against real SHA-256 must decline to answer. If it ever reads `true`,
the digest engine discarded the name and served SHA-256 under it — defect #6,
observed. If it reads `false` with two real hex strings, something served
`NO-SUCH-DIGEST` with bytes of its own, which is worse. `n/a` is the only
correct answer for a VM that refuses the name, and `C.mac.224equals256 = false`
is the same guard on the `Mac` engine reading `false` rather than `n/a`
*because* both sides produced bytes — which is what makes `false` meaningful
there and would make it meaningless on the digest row.

**The oracle has advertise-but-refuse rows of its own**, and this must be known
before any CratonVM diff is read as a defect: SunJCE advertises 14
`HMACPBESHA*` / `PBEWITHHMACSHA*` `Mac` names and `Mac.getInstance` answers all
14 with `java.security.ProviderException: Could not construct MacSpi instance`.
"Advertised and not serviceable" is therefore not by itself proof of a CratonVM
bug. It has to be checked per name against the oracle, which is the whole
reason the transcript is committed rather than the verdicts.

One incidental correction the oracle supplies: `A.KeyGenerator[AES] keylen=32`.
JDK 25's SunJCE AES default is **256-bit**, not the 128 W4-3's residual pass
assumed when it noted `KeyGenerator` returns "a hard-coded 128-bit default".
`keylen` is printed on every `KeyGenerator` row precisely because an engine that
stores the algorithm string and never reads it again answers every name with one
size, and the length is the only observation that can see it. That
`KeyGenerator` gap is real, is out of scope here, and is left recorded.

## 8. What this record does NOT close

> **CLOSED 2026-09-02.** All six bullets were asked directly, on a binary,
> before any of them was worked. Three were already fixed and had aged into
> falsehoods; two were closed by this pass; one is narrowed to a named
> structural residual. The measurements are in "§8, remeasured" immediately
> below, and `apps/probes/W763Residuals` is the one command.
>
> The pattern is this page's own §4a, one level up: **three of the six bullets
> were claims about VM behaviour that had stopped being true**, and each cost
> one command to check. A record's open list is a hypothesis with a date on it.

* `KeyGenerator` reads its algorithm string once and never again; every name
  succeeds and yields the same default size. `DESede` gives 16 bytes where
  SunJCE gives 24. W4-3 recorded it without a patch; still open, and the probe's
  `keylen` column is now the instrument for it.

  > **STALE — fixed before this pass.** `keygen_default_bits`,
  > `keygen_byte_len` and `des_set_odd_parity` are in the tree and their own
  > comments record the measurement. Every `KeyGenerator` default is identical
  > to HotSpot: `AES` 32 bytes, `DESede` **24** with odd parity per byte, `DES`
  > 8, `HmacSHA512` 64, `Blowfish` 16.

* Nine `SunRsaSign` `Signature` names are advertised, admitted, and fail at
  `sign()` — §3 #5. Ordinary unimplemented-algorithm gap.

  > **CLOSED, and it was twenty-six names rather than nine.** All nine RSA names
  > sign and verify byte-identically to HotSpot; see "The twenty-six signature
  > names" below. The count was nine because nobody asked SunEC the same
  > question: seventeen of its twenty `Signature` names failed the same way,
  > fourteen of them after `getInstance` had accepted the name.

* `KeyFactory.getInstance("ML-DSA-44").getProvider()` NPEs; `Signature`'s
  ML-DSA objects return a null provider. W7-29 found both; neither is an
  advertise-versus-serve gap.

  > **STALE — fixed before this pass.** All four `ML-DSA` names answer `SUN`
  > from both engines, matching HotSpot.

* `SUN.getServices()` answers 35 rows where HotSpot answers 65. A milder
  under-advertisement, untouched. Do not read it as evidence the §3 #3 wrapper
  landed badly.

  > **STALE — `SUN` matches HotSpot exactly.** The under-advertisement had moved
  > to the other providers by 2026-09-02 and is closed too: across `SUN`,
  > `SunRsaSign`, `SunJCE`, `SunEC` and `SunJSSE` this VM now answers **330 of
  > HotSpot's 335** services, advertising nothing HotSpot does not. The five are
  > the `SunTls*` `KeyGenerator` KDFs — see "What is still open" below.

* **`Collections.unmodifiableSet` is the IDENTITY function in
  `--synthetic-jdk`, so §3 #3 is inert there.**

  > **REFUTED by building the mode.** In `--synthetic-jdk`, on the same probe:
  > `identityOfSource=false`, `mutate=UnsupportedOperationException`, and the
  > view's class is `java.util.Collections$UnmodifiableSet` — the same as
  > HotSpot's. §3 #3 is inert in no mode, and the vacuous-green trap this bullet
  > warns of (a probe reading `class=java.util.HashSet add=SUCCEEDED`) cannot
  > happen.

  Found while wiring `wrap_unmodifiable`. The name has three registrations and
  registration is last-write-wins. The rest of this bullet's detail — the three
  registrars, the `SyntheticStub` window, and the three-arm correction of
  2026-08-12 — is preserved verbatim below, because the mechanism it describes
  is real even though its verdict for `--synthetic-jdk` no longer is.

  * `native-collections`'s `register_collections_extras_natives` →
    `native_collections_unmodifiable_set`, a genuine read-only view. Live in
    real-JDK and `--jdk-only`.
  * `phases_early::register_collections_extras_natives` **and**
    `phases_early::register_core_stdlib_extras` → `native_return_first_arg`.
    Both are reached only from `lib::register_synthetic_overrides`, which runs
    after the essential registrars, so in `--synthetic-jdk` the identity wins.

  This is the same species as everything else in this record — an API whose
  entire contract is a refusal, quietly not refusing — and it is broader than
  the JCA: `unmodifiableList`, `unmodifiableMap` and `unmodifiableCollection`
  are bound the same way in the same two registrars. Out of scope here, and it
  needs a `--synthetic-jdk` build, which no lane has made. **It is also a
  vacuous-green trap:** a probe run under `--synthetic-jdk` reports
  `D.getAlgorithms[MessageDigest] class=java.util.HashSet add=SUCCEEDED`, which
  reads exactly like "§3 #3 never landed" and is not that.

  > **Corrected and completed, second pass 2026-08-12.** The row above says the
  > `native-collections` binding is "live in real-JDK and `--jdk-only`". It is
  > live in **real-JDK only**: those six `unmodifiable*` factories sit in their
  > own `r.set_category(NativeKind::SyntheticStub)` window inside
  > `register_collections_extras_natives`, and `SyntheticStub` is the one kind
  > `--jdk-only` drops at registration
  > (`native-builtins/tests/stub_ratchet.rs`'s strict siblings assert **zero**
  > surviving `SyntheticStub` rows). So §3 #3 has **three** arms, not two, and
  > the middle one was never stated:
  >
  > * `--real-jdk` — the native wins; `wrap_unmodifiable` gets a fabricated
  >   `cratonvm/internal/UnmodifiableSet` whose `add` is `native_unmod_throw`.
  >   Immutable, but `getClass().getName()` is not HotSpot's.
  > * `--jdk-only` — the registration is dropped, so real
  >   `java.util.Collections` bytecode runs and the view is the genuine
  >   `Collections$UnmodifiableSet`. Immutable, and byte-for-byte HotSpot's.
  > * `--synthetic-jdk` — identity, as the row above says.
  >
  > The middle arm is worth stating because the alternative reading is a live
  > hazard rather than a quibble: **had those rows survived into `--jdk-only`,
  > the fix would have been silently inert there.** `alloc_unmod_wrapper` calls
  > `try_alloc_synthetic`, which strict-mode policy refuses for a
  > `cratonvm/internal/*` class with a catchable `NoClassDefFoundError` — and
  > `wrap_unmodifiable`'s deliberate `_ => set` fallback would have swallowed it
  > and returned the plain mutable `HashSet`, on the one path with no way to
  > tell that from "the wrapper was never applied". Anyone retagging that window
  > away from `SyntheticStub` re-opens §3 #3 in strict mode without touching a
  > line of this record's code.

* `--synthetic-jdk` has never been built by any lane, so #6's live half has
  never been observed — only reasoned about.

  > **BUILT AND RUN, 2026-09-02.**
  > `cargo build -p cratonvm-cli --bin cratonvm --features synthetic-jdk`,
  > then `--synthetic-jdk` at runtime. It builds clean and runs the probes.
  > And the first run found something no amount of reasoning had: **`ArrayList`
  > did not implement `RandomAccess`**, because the synthetic interface table
  > grouped it with `LinkedList`, which deliberately lacks that marker.
  >
  > `Collections.unmodifiableList` picks its view class BY that marker, so it
  > handed back `$UnmodifiableList` where HotSpot gives
  > `$UnmodifiableRandomAccessList` — and `Collections.binarySearch`,
  > `reverse`, `shuffle` and `fill` each branch on it to choose indexed access
  > over an iterator, so every one of them silently took the linked-list path
  > over an `ArrayList`. Fixed, with `Cloneable`, which all four declare and
  > none carried.

## 8a. §8, remeasured — one command, six answers

`apps/probes/W763Residuals`, run on both VMs with a fixed RSA key pair so the
deterministic signatures are comparable, and diffed on stdout. Every row below
is identical to HotSpot 25.0.3 unless said otherwise.

| §8 bullet | asked as | 2026-09-02 |
|---|---|---|
| `KeyGenerator` defaults | `getInstance(a).generateKey().getEncoded().length` | identical, incl. `DESede`=24 |
| nine `SunRsaSign` names | sign + verify + the signature BYTES | identical, all 13 PKCS#1 v1.5 names |
| ML-DSA `getProvider()` | `KeyFactory`/`Signature` `.getProvider().getName()` | identical, all four names |
| `SUN.getServices()` | per-provider service counts | identical for all five (SunJCE 189 vs 194 when this row was written; **194 vs 194** since §8f) |
| `unmodifiableSet` | identity, mutation, class, in all three modes | identical |
| `--synthetic-jdk` never built | it is now | builds, runs, one defect found |

The one differing row was the five `SunTls*` services, and §8f closed it. **No
row of this table differs now**, and nothing else in the record's open list
survives.

## 8b. The twenty-six signature names

§3 #5's disposition — "the name is real, the advertisement is truthful, and the
failure is closed and catchable ... an ordinary unimplemented-algorithm gap" —
was a defensible call about nine names. Two things were wrong with it as a
place to stop.

**It was a policy about one provider, and the same shape held for another
nobody counted.** `JcaResolveAll` asks the services HotSpot has and this VM does
NOT enumerate. A name advertised HERE and failing at `sign()` is invisible to
it — which is precisely the nine. Asking SunEC the same way found seventeen
more:

```text
of SunEC's twenty Signature names, THREE worked
  NONEwithECDSA, SHA1withECDSA, SHA224withECDSA           fail at sign()
  all four SHA3-*withECDSA                                fail at sign()
  all ten inP1363Format twins                             fail (4 not advertised)
```

**And none of the twenty-six needed cryptography.** PKCS#1 v1.5 over RSA is a
digest, a `DigestInfo` whose only per-name inputs are an OID and a length, and
block type 1 — and every one of the nine digests was already in this tree,
MD2 by *this record's own §3 #1*, which implemented it precisely because
"`SunRsaSign` and `SunMSCAPI` both advertise `MD2withRSA`". ECDSA is not
computed here at all: the three names that worked were already the platform's
own SPI, and the other seventeen are the same drive against a sibling class.

Both families are DERIVED now rather than tabulated — the SPI class is a
function of the caller's spelling, and the seed list comes from the same
function the engine resolves through, so the advertised set and the served set
are one list by construction. That is not a stylistic preference: `seed_sunec_services`
carried sixteen hand-written `(name, class)` pairs where HotSpot has twenty,
and the four missing ones were `SHA3-*withECDSAinP1363Format`. A transcribed
list drifts; a derived one cannot.

Verified on the bytes, not on the absence of an exception:

```text
RSA    all 13 PKCS#1 v1.5 signatures byte-identical to HotSpot
       (the scheme is deterministic, so a wrong DigestInfo prefix cannot
        hide behind a round trip — which is why the prefixes are generated
        from an OID and a length, with a test asserting the generator
        reproduces RFC 8017's four published blobs)
ECDSA  all 20 names verify=true, and DER vs P1363 encodings distinct
       (ECDSA draws a nonce, so the bytes are not comparable; the encoding
        and the round trip are)
```

Ten of the twenty ECDSA names are `inP1363Format` twins, and they are not a
formatting flag this engine could have applied: the JDK implements the
fixed-width `r || s` encoding by SUBCLASSING, so routing to the class is what
makes the encoding right as well as the signature.

## 8c. What is still open

> **TWO of the three below closed on 2026-09-02**, both kept struck through with
> what they turned out to be. The `RandomAccess` one is worth keeping because
> its own diagnosis was wrong in a way that would have sent the next reader to
> the wrong crate. The `SunTls*` one is worth keeping because its diagnosis was
> RIGHT — it named the engine change required, in one sentence, weeks before
> anyone made it — and the record of a correct deferral is worth as much as the
> record of a wrong one.
>
> **This page's open list is now the single `LinkedList`/`Deque` bullet**, which
> is a declined trade rather than an unfinished job.

* ~~**Five `SunTls*` `KeyGenerator` services.** `SunTlsPrf`, `SunTls12Prf`,
  `SunTlsMasterSecret`, `SunTlsKeyMaterial`, `SunTlsRsaPremasterSecret` — the
  TLS-internal KDFs, which take `TlsKeyMaterialParameterSpec`-family specs this
  engine's two-field synthetic `KeyGenerator` cannot carry. Serving them means
  handing back a real `javax.crypto.KeyGenerator` over the platform's SPI and
  teaching all nine natives registered on that class to recognise a receiver
  they did not build (the `skf_receiver_is_ours` shape). An engine change, not
  a row.~~

  > **CLOSED 2026-09-02, exactly as the bullet described.** The engine change
  > was made: `build_real_key_generator` (the `KeyGenerator` twin of the
  > `SecretKeyFactory`/`Mac`/`KeyFactory` builders, through
  > `javax.crypto.KeyGenerator`'s own `(KeyGeneratorSpi, Provider, String)`
  > constructor, on the engine's refusal path), five rows naming the real
  > platform classes, and `keygen_real_spi` guarding all six `init`/`generateKey`
  > natives.
  >
  > **This VM now advertises 335 of HotSpot's 335 services** across `SUN`,
  > `SunRsaSign`, `SunJCE`, `SunEC` and `SunJSSE` — SunJCE 189 → 194 — and
  > advertises nothing HotSpot does not. §8a's one differing row is gone.
  >
  > Verified on the BYTES, not on a resolve. TLS 1.2's PRF and the
  > master-secret and key-material derivations are deterministic, so four of the
  > five diff byte-for-byte against HotSpot 25.0.3
  > (`apps/probes/JcaSunTlsVectors`); `SunTlsRsaPremasterSecret` draws a nonce,
  > so its length and version bytes are diffed instead. See "8f. The engine
  > change the SunTls bullet asked for" below.

* ~~**`Collections.unmodifiableList(x) instanceof RandomAccess`** is `true` in
  `--jdk-only` and `false` in the two modes where `alloc_unmod_wrapper`
  fabricates the view — the fabricated class does not re-declare the marker its
  own NAME promises. Belongs to whoever owns `native-collections`' wrapper
  minting.~~

  > **CLOSED 2026-09-02, and it was not the wrapper minting.** See
  > "8d. One name that reads like the other" below. `apps/probes/RandomAccessProbe`
  > is now byte-identical to HotSpot on twelve rows in `--jdk-only` and in
  > real-JDK mode, across all three doors (`instanceof`, `checkcast`,
  > `Class.isInstance`) and `getClass()`. `--synthetic-jdk` had a SECOND,
  > unrelated cause for the same symptom, and the sweep run to check that one
  > pair was not a coincidence found five more defects — §8e.

* **`LinkedList` is not a `Deque`** here and is on HotSpot. Deliberately not
  declared: this VM carries most of the deque surface and not all of it, so the
  interface would turn a clean `ClassCastException` into a missing method at
  the point of use.

## 8d. One name that reads like the other

The wrapper minting was right, and so was every table this bullet pointed at.
`cratonvm/internal/UnmodifiableList` is a stamp for BOTH of HotSpot's two
unmodifiable-list classes, and the VM picks between them per instance by asking
whether the wrapped list implements `RandomAccess` — `getClass()` through
`native-builtins`' `getclass_backing_is_random_access`, and the
`instanceof`/`checkcast` opcodes through `typecheck::unmod_backing_reaches`.
Two implementations of one decision, which is the shape that fails.

The opcode side asked through `ClassManager::is_subclass_of_by_name`. That
function walks ONLY the superclass chain: it is the exception-`catch_type`
fallback, and a `catch_type` is never an interface. `RandomAccess` is an
interface, so it answered `false` for every list ever built — `ArrayList`
reaches `AbstractList`, `AbstractCollection`, `Object` and stops. The
`getClass()` side resolved the interface to a `ClassId` and used the
DAG-walking `is_subclass`, so it answered `true`. One object, at one instant:

```text
v.getClass()                      java.util.Collections$UnmodifiableRandomAccessList
RandomAccess.class.isInstance(v)  true
v instanceof RandomAccess         false
(RandomAccess) v                  ClassCastException
```

`display_class_satisfies_target` — the arm that exists precisely to keep the
opcodes agreeing with `getClass()` — was reached, ran, and computed the wrong
display class, so the fix is one function call and not a new mechanism. Two
things made it hard to see and both are worth naming:

* **The disagreement was invisible from either side alone.** `getInterfaces()`
  returned `[RandomAccess]`, `getClass()` named the RandomAccess class, and the
  declared interface `Class` object was `==` `RandomAccess.class` — every
  reflective question answered correctly, because they all run on the display
  class. Only a probe that asks all three doors about ONE object shows it, which
  is why `apps/probes/RandomAccessProbe` prints an `agree=` column.
* **The trap was already written down, on a different call site.** The
  `Path.toString()` branch in `runtime/invokedynamic.rs` carries a paragraph
  explaining that `is_subclass_of_by_name` "can never match an interface like
  `Path` and this branch would silently never fire", added after a concurrent
  commit made exactly this mistake. This is its second occurrence, and the name
  is the whole reason: the function that sounds like the general one is the
  special one. `classloading`'s
  `the_supers_only_name_walk_cannot_see_an_interface_the_dag_walk_finds` now
  asserts the divergence in both directions, so the next reader meets it as a
  test rather than as a comment on an unrelated branch.

The fix moved BOTH halves, which the second measurement forced. Correcting only
the opcode side made `Collections.unmodifiableList(List.of("a", "b"))` — a
wrapper whose backing is itself a stamp — read `instanceof=true isInstance=false`:
a *new* disagreement, where before the pair had been wrong together. Each side
now runs the same structural rule (`typecheck::object_reaches` and
`getclass_object_reaches`), descending slot 0, because an unmodifiable view
carries the marker exactly when the thing it wraps does. Both carry a comment
saying they are a pair; neither may move alone.

## 8e. The sweep that closed §8c's bullet found five more

Fixing one pair is not evidence about the others, so `apps/probes/CollectionViewTypes`
asks 24 collection views and 11 concrete classes about 14 interfaces each — 490
cells, on HotSpot 25.0.3 and on this VM in all three modes, diffed on stdout.

**Real-JDK: 35 of 35 rows now identical.** Three of them were not, and they are
the OPPOSITE defect from §8d's — over-admissions, where this VM says `true` and
HotSpot says `false`:

```text
aConcurrentSkipListSet instanceof List                     CratonVM true   HotSpot false
aConcurrentSkipListMap instanceof Collection/List/Iterable  true    false
aPriorityQueue         instanceof Deque                     true    false
```

`synthetic_implements` has a name-word fallback for classes with no real
interface data, and it reads the word `List` out of `ConcurrentSkipList`**Set**
and `ConcurrentSkipList`**Map** — a skip list is how they are BUILT, not what
they are. This is the family the fallback's own comment already records ("`x
instanceof List` returned true for a HashSet ... which broke JUnit's
`Parameterized$RunnersFactory`"), fixed then for Set-versus-List and not for
these. `PriorityQueue` is a third shape: `Queue` and `Deque` shared one match
arm, and `Deque extends Queue` rather than the reverse.

An over-admission here is worse than a refusal, because it converts a clean
`ClassCastException` at the cast into a `NoSuchMethodError` at the first call —
which is the exact trade §8c's `LinkedList`/`Deque` bullet declines to make. The
arms now exclude a name whose FINAL word contradicts the target. Not a
last-word-wins rule, which is tidier and wrong: `Collections$SetFromMap` ends in
`Map` and is a `Set`.

**`--synthetic-jdk`: every type row that can be measured is identical.** In that
mode the supertype set comes from `class_manager.rs`'s interface table, and
NONE of `Collections$Unmodifiable*`, `ImmutableCollections$*` or
`Arrays$ArrayList` had an arm there — all of them fell to `_ => &[]` and
declared nothing at all. The coarse questions still answered correctly, because
the name-word fallback above reads `List` out of `UnmodifiableList`, so only the
interfaces a name does NOT spell were lost:

```text
unmodifiableList(ArrayList)  Iterable      HotSpot true  CratonVM false
                             RandomAccess          true           false
Arrays.asList                RandomAccess          true           false
                             Serializable          true           false
List.of(..) / Set.of(..)     Iterable              true           false
TreeSet                      SortedSet             true           false
TreeMap                      SortedMap             true           false
```

A `Collection` that is not an `Iterable` is the worst of those: every for-each
through an erased type is a `checkcast java/lang/Iterable`. `TreeSet` and
`TreeMap` are the `ArrayList`/`LinkedList` split of §8's last bullet happening
twice more — a group in that table costs its members exactly the markers that
distinguish them.

### Still open in `--synthetic-jdk`, and not this species

* `Collections.emptyList()`/`emptySet()`/`emptyMap()` hand back a plain
  `ArrayList`/`HashSet`/`HashMap`. Every type answer is right; the CLASS is
  wrong (and so `Cloneable` is `true` where HotSpot says `false`). A
  factory-return question, not a hierarchy one.
* No `(Collection)` copy constructor for `Vector`, `CopyOnWriteArrayList`,
  `ConcurrentSkipListSet`, `ConcurrentSkipListMap`, `ArrayDeque` or
  `PriorityQueue`; no `Collections.unmodifiableSortedMap`; no
  `DayOfWeek.MONDAY`. Method-surface completeness.
* `LinkedList` is not a `Deque` — §8c's third bullet, unchanged and deliberate.

The probe prints an `ERROR` row for each of those rather than dying: its first
version called the factories inline, `unmodifiableSortedMap` raised
`NoSuchMethodError` at row 22 in that mode, and the fourteen control rows below
it — every sorted class, the whole reason they are in the probe — silently
measured nothing. **A probe that stops early does not report less; it reports a
shorter file that still diffs clean.**

## 8f. The engine change the SunTls bullet asked for

`getInstance` resolving is not the bar here, and the record says so twice
already. The five `SunTls*` names are KDFs: routing to a generator and never
initialising it, or initialising it with a spec whose fields were read in the
wrong order, both produce an object that resolves and then hands back the wrong
key. So the acceptance test is the BYTES.

TLS 1.2's PRF is deterministic given (secret, label, seed), and so are the
master-secret and key-material derivations built on it. Four of the five diff
byte-for-byte; `SunTlsRsaPremasterSecret` draws a nonce, so its length and its
two version bytes are what is fixed. `apps/probes/JcaSunTlsVectors`, both VMs,
identical:

```text
SunTlsPrf                762791fa4ef968af841ad66563edc761…
SunTls12Prf              eb621eb7ba5be377fceeb81260c3cc88…
SunTlsMasterSecret       TlsMasterSecret c0dd6ecbed2c4fb4…
SunTlsKeyMaterial        cw=e491f7c8… sw=7eae9609… civ=3a82de1c siv=c60442b3
SunTlsRsaPremasterSecret len=48 version=0003
```

### Three parts, and the one that is easy to get wrong

**The builder.** `build_real_key_generator` is the fourth sibling of
`build_real_secret_key_factory`, `build_real_key_factory` and `build_real_mac`,
through `javax.crypto.KeyGenerator`'s own protected
`(KeyGeneratorSpi, Provider, String)` constructor. Reached only from
`keygen_get_instance_named`'s refusal path — after this engine's own verdict,
never before it, which is the whole safety argument `jdk_service_class` is
written to.

**The guard, which is the part that is easy to get wrong.** Every native on
`javax/crypto/KeyGenerator` used to be able to assume the receiver was this
crate's two-field synthetic (algorithm at slot 0, key size at slot 1). A real
`KeyGenerator`'s slots hold `provider`/`spi`/`algorithm`/`lock` instead, so each
native now asks `keygen_real_spi` first.

That read is **type-checked, and the type check is not defensive
programming — it is the discriminator**. `get_field_by_name` can fall back to a
name→slot mapping, so asking a two-field synthetic for "spi" can return slot 0,
which is a `String`. Handing that to `invoke_virtual` as a `KeyGeneratorSpi` is
the failure; checking that it IS a `KeyGeneratorSpi` both prevents it and
answers the question. The identical unchecked read put a `String` where a
`Provider` belonged in `pbkdf2_get_provider` and killed the caller on
`String.getName()` — this is that lesson applied before it could happen twice.

No side table, which is the one deliberate difference from
`skf_receiver_is_ours`: that one asks "did we build this" through an
identity-keyed registry, this one asks "can I delegate" and answers with the
object to delegate TO. Nothing to keep in step with GC relocation, nothing to
evict.

**The two `AlgorithmParameterSpec` `init` overloads were deliberate no-ops**,
and they are the ONLY route into these five, whose entire input is a spec. They
stay no-ops for a synthetic receiver, for the reason recorded on them (an
`AlgorithmParameterSpec` is an empty marker interface, and the JCE contract lets
a provider ignore parameters it does not recognise) — but on a real receiver
they now forward. Swallowing the spec there would have left the generator
uninitialised and moved the failure into `generateKey()`, which is a worse
answer than the `NoSuchAlgorithmException` this used to give.

### What the ratchet had to become

`every_keygenerator_the_engine_implements_is_advertised` was a biconditional
over HotSpot's own twenty-four names: advertised **iff**
`keygen_default_bits` generates it. Serving the five by ROUTING rather than by
generating makes that predicate too narrow, and narrowing is not conservatism
here — it reds the test for names that work.

It is a disjunction now: advertised iff (this crate generates it **or** the row
names a real platform class). Deleting the old five-name assertion would have
lost what it was protecting, so it was kept and inverted: those five must still
have NO `keygen_default_bits` arm (an arm appearing there would mean someone
fabricated a key where a KDF belongs) and must resolve to a
`com.sun.crypto.provider.Tls*` class that is not the `.Native` marker.

Checked for falsifiability rather than assumed: deleting one seed row reds it
with `SunTlsPrf must be advertised with a real class`.

### The regression surface, measured

The guard runs on every `KeyGenerator` call in the VM, and the all-zero-key
defect that `SecretKeySpec`'s copy shim exists for lives on this exact path. Run
on both VMs and diffed:

| probe | rows | result |
|---|---:|---|
| `JcaSunTlsVectors` | 5 | identical |
| `JcaKeyGeneratorDefaults` | 24 | identical — the row that used to read "identical but the five `SunTls*`" |
| `JcaKeygenScrub` | 5 | identical (the all-zero-key property) |
| `JcaDerivationVectors` | 24 | identical |
| per-provider service counts | 5 | identical, **335 of 335** |
| `cargo test -p cratonvm-native-builtins --lib` | 4197 | passed |

### Scope, stated honestly

`sun.security.internal.spec` is not exported by `java.base`, so the probe needs
`--add-exports java.base/sun.security.internal.spec=ALL-UNNAMED` on both VMs.
The real caller of these five is `sun.security.ssl`'s own handshake, not
application code. What closing this buys is that the JDK's TLS stack can reach
its own KDFs through this VM's provider chain, and that the advertise-versus-
serve ledger this page is named for reaches zero.

## 9. How to verify

Build, then in **both** arms.

> **The `--synthetic-jdk` exclusion below is withdrawn (2026-09-02).** It said
> that mode "is NOT a valid arm for the `D.` rows: `Collections.unmodifiableSet`
> is the identity function there". It is not: measured in all three modes, the
> view is `java.util.Collections$UnmodifiableSet`, `add` raises
> `UnsupportedOperationException`, and the object is not the source. All three
> arms are valid, and `apps/probes/UnmodifiableSetProbe` is the smaller
> instrument for exactly this question.
>
> **Three arms, one build each:**
>
> ```bash
> cargo build --release -p cratonvm-cli --bin cratonvm
> cargo build --release -p cratonvm-cli --bin cratonvm --features synthetic-jdk >       --target-dir target-synth
> cratonvm            --java-home <jdk25> -cp <out> UnmodifiableSetProbe
> cratonvm --jdk-only --java-home <jdk25> -cp <out> UnmodifiableSetProbe
> target-synth/.../cratonvm --synthetic-jdk -cp <out> UnmodifiableSetProbe
> ```
>
> The `synthetic-jdk` build takes its own `--target-dir`: it is a different
> feature set, and sharing one with the default build makes every switch a full
> rebuild.

```
java -cp <out> JcaAdvertisedVsServedProbe > cratonvm-<arm>.txt
diff probes/JcaAdvertisedVsServedProbe.expected.txt cratonvm-<arm>.txt
```

The rows that must move from the pre-change binary:

```
A.MessageDigest.n                     13 -> 15
A.MessageDigest[MD2]                  THREW NoSuchAlgorithmException -> OK len=16 d("")=8350e5a3...
A.MessageDigest[SHAKE128-256]         absent -> OK len=32 d("")=7f9c2ba4...
A.MessageDigest[SHAKE256-512]         absent -> OK len=64 d("")=46b9dd2b...
A.KeyFactory[ML-DSA]                  present and THREW -> absent from the advertised set
A.KeyFactory[ML-KEM]                  present and THREW -> absent from the advertised set
B.Signature[NO-SUCH-SIG]              OK getAlgorithm()=Unknown -> THREW NoSuchAlgorithmException
B.Signature[ML-KEM] / [AES] / [HmacSHA256] / []   same
C.md[MD2].abc                         match=n/a -> match=true
C.md[SHAKE128-256].abc                match=n/a -> match=true
C.md[SHAKE128].abc                    match=n/a -> match=true    (the ALIAS must resolve)
C.md.fallbackEqualsSha256             n/a, and must STAY n/a
D.getAlgorithms[MessageDigest]        class=java.util.HashSet add=SUCCEEDED
                                        -> class=...Collections$UnmodifiableSet add=UnsupportedOperationException
D.SUN.getServices.add                 SUCCEEDED -> UnsupportedOperationException
```

`RJdkSecurity` must run to **`PASS RJdkSecurity (80 checks)`** in all three arms
— 61 before the second pass added `advertisedVersusServed()` — and `RCrypto` /
`RChaCha20Cipher` must be unchanged.

Since that vector is scheduled and the probe is not, the suite is now the
cheaper instrument for everything in §3 except #4 and #7. Those two are knowing
divergences from HotSpot (`KeyFactory` no longer advertising the two umbrellas),
so they cannot be asserted in a fixture that also runs on the oracle; their
ratchet stays `every_advertised_key_factory_name_is_serviceable`, and the
fixture asserts the invariant the removals restore — advertised implies
serviceable — which is true on both VMs by different routes.

## 10. The single falsifying observation

If `C.md.fallbackEqualsSha256` reads `true` on a CratonVM arm after this change,
then some `MessageDigest` path still reaches a SHA-256 default that neither
`getInstance` gate covers, and §3 #6 shut the wrong two doors. That one row is
worth more than the rest of section C put together: every other row can be
satisfied by an engine that computes the right answer for names it knows, and
only this one asks what it does with a name it does not.

If `C.md[SHAKE128].abc` reads `n/a` on a CratonVM arm, the alias correction in
§3 #2 did not take and `getInstance` is still refusing a name the provider chain
advertises an alias row for — the state this record shipped in before its second
pass, and the reason a registry-level assertion is not cover for a
`getInstance`-level claim.

If instead `A.Signature[*]` rows start reading `THREW` for names the oracle
serves, the `signature_name_is_offered` disjunction has lost its second arm —
i.e. `find_service_provider` is answering `None` because the provider chain, not
the service map, is short. The repair in that case is to seed the missing
service, in one place, where `Security.getAlgorithms` will report it too.

## 11. Third pass, 2026-08-12 (record triage, doc-only — nothing built or run)

A source read of today's tree, stated as such. Line numbers are today's.

**§3 #2's remaining residual is DISCHARGED.** The correction block closes with
*"Not done … the one-line repair is to fold `SHAKE128` → `SHAKE128256` and
`SHAKE256` → `SHAKE256512` into `compute_digest`'s `upper`"*. That is applied:
`native-builtins/src/lib.rs:36461-36479`, immediately after the `-`/`/` strip and
before the `match`, with the reasoning and the cross-reference at the site. It is
written as `if`/`else` rather than a `match` arm, and the comment says why (the
`match` form moves `upper` out of an arm while the scrutinee still borrows it —
an `E0505`). So the synthetic-mode door is now shut for the aliases too, and the
`getInstance`-admits-but-`digest()`-refuses window that residual described is
closed in source. **Unbuilt** — no `--synthetic-jdk` binary exists to see it.

**Everything else §3 claims is present**: `canonical_algorithm`
(`native-builtins/src/jca/message_digest.rs:539`) with
`the_shake_aliases_resolve_but_are_not_separate_algorithms` (`:758`),
`shake_normalisations_agree_across_the_two_filters` (`:823`),
`shake_matches_hotspot_vectors` (`:846`), `real_md2_matches_hotspot_vectors`
(`native-builtins/src/lib.rs:43383`), and all three §6 ratchets
(`provider_chain.rs:4355`, `:4421`, `:4463`). Both umbrellas are de-advertised
with the reasoning in place at `provider_chain.rs:1111-1137` and `:1265-1277`.

**§8's three-arm correction: its PREMISE was checked, not taken on trust.** The
middle arm rests on the claim that the six `unmodifiable*` factories carry
`SyntheticStub`. They do, explicitly:
`native-collections/src/lib.rs:51674-51692` opens a
`set_category(NativeKind::SyntheticStub)` window around exactly those six and
says so in a comment that scopes it away from the `empty*`/`singleton*`/
`synchronized*` neighbours in the same registrar. So `--jdk-only` really does
drop them and really does run the JDK's own `Collections` bytecode.

**Independent corroboration of the hazard §8 reasons about, from today's
census.** `P1-BASELINE-20260812.md` records `cratonvm/internal/UnmodifiableMap`
as **P1-B**, one of the nine measured families that block `--jdk-only` — the
strict refusal of a `cratonvm/internal/*` stand-in is therefore observed, not
inferred. It is **not** a counter-example to the arm table above: P1-B's producer
is a different one, `System.getenv()`'s
`try_ensure_synthetic_class("cratonvm/internal/UnmodifiableMap", 2)` at
`native-builtins/src/lang_system.rs:3306`, not the `Collections` registrar. Both
things are true at once, and the pair is the sharpest statement of §8's warning:
the refusal is real and measured, so anyone retagging that window away from
`SyntheticStub` would land `wrap_unmodifiable`'s `_ => set` fallback on a real
refusal and silently hand back the mutable `HashSet`.

**§9's `PASS RJdkSecurity (80 checks)` arithmetic verified statically.**
`regression-suite/src/RJdkSecurity.java` has 77 `check(...)` call sites, and
exactly one of them runs more than once — the four-name `Signature` loop at
`:421-430` — giving 80. `advertisedVersusServed()` (`:346-445`) contributes
19 = 3 (MD2) + 4 (the two SHAKE primaries, length and bytes) + 2 (the two
aliases) + 3 (the advertised-set membership triple) + 1 + 1 (the two
advertised-implies-serviceable loops, one check each) + 4 (`Signature`) + 1
(unmodifiable), over the stated 61. No check sits inside a provider-list-sized
loop, deliberately (`:392-394`). This is arithmetic, not a run.

**Two things the fixture still leaves as prints rather than assertions.**

* **The check count is printed, never asserted** (`:454`). `RJdkProcess` learned
  this lesson in `W7-46` §1 — *a printed integer is evidence only for as long as
  someone is reading it* — and holds `EXPECTED_CHECKS` as a constant it asserts
  before printing. `RJdkSecurity` has no such constant, so a `check` that stops
  running lowers a number in a transcript and nothing fails. Since the count is
  loop-invariant by design (above), the constant is safe here. Nominated with
  this pass.
* **`CK RJdkSecurity … digests=` prints a VM-dependent value** (`:443-444`). It
  is diff-safe only while the advertised `MessageDigest` set is exactly HotSpot's
  15 — the oracle transcript's `A.MessageDigest.n = 15`
  (`probes/JcaAdvertisedVsServedProbe.expected.txt:64`), which this branch's
  13 → 15 was written to match. If either side's provider list drifts by one
  name, this surfaces as an unexplained cross-VM `CK` diff rather than as a named
  assertion. Small, recorded rather than changed.

**Scheduling, per §9's own claim: confirmed.** `RJdkSecurity` is in
`JDKONLY_CLASSES` (`regression-suite/run.sh:119`), so everything in
`advertisedVersusServed()` runs on all three arms. `probes/` is scheduled by
nothing — the string `probes` does not occur in `regression-suite/run.sh` at any
`SUITE=` value — so `JcaAdvertisedVsServedProbe` remains a hand-run instrument,
exactly as §9 says.
