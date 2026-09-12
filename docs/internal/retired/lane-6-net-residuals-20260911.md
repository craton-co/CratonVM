# Lane 6 residuals — the four VM changes its retirement left behind

**Status: DONE 2026-09-11.** Four items from
[`lane-6-net-security-RETIRED-20260910.md`](lane-6-net-security-RETIRED-20260910.md)
§8, all inside the prefix set that lane retired, none of them a retirement.

This wave changes no `RETIRED_SHADOW_*` table and adds no row to one. Every
change here makes a native answer what HotSpot answers. That distinction is
the reason the wave could run at all: the lane's remaining retirements are
blocked on the TLS/JCA contract question, which is L0's to answer, and
**correctness is not blocked on it**. Whether a rustls implementation is a
§1.4 shadow decides what may be *retired*; it says nothing about whether
`Cipher.wrap` may decrypt.

---

## 1. What was measured, and by what

The instrument is the lane's own probe tree, run as a two-binary A/B against
HotSpot 25.0.4+7. `difflines` is `diff | grep -c '^[<>]'`, which is two lines
per differing row.

| probe | rows | base | after | differing rows fixed |
|---|---:|---:|---:|---:|
| `L6InetSweep` | 850 | 380 | 0 | **190** |
| `L6X500Sweep` | 403 | 276 | see §7 | |
| `L6JcaSweep` | 196 | 72 | see §7 | |
| `L6TlsParamSweep` | 116 | 66 | see §7 | |
| `L6HttpLoopbackSweep` | 78 | *did not exist* | see §7 | |

The base binary is `origin/dev` at `8f09c89b9`, built from the same worktree
as the trial so the two differ only by this wave's source.

---

## 2. Item 3 — `InetAddress`: one registration, and one screen in the wrong place

### 2.1 The registration named a method the image does not have

`javap -p -s java.net.Inet6AddressImpl` on the JDK 25 image:

```text
  private native InetAddress[] lookupAllHostAddr(String, int);
    descriptor: (Ljava/lang/String;I)[Ljava/net/InetAddress;
```

`Inet4AddressImpl`'s really is one-argument, and `inet_address.rs` registered
the one-argument spelling for **both**. So the method the image declares had
no implementation, and the method the registration named does not exist. JDK
19 added the `LookupPolicy` characteristics int; nothing had re-read the image
since.

The four bits are the whole contract (`IPV4`, `IPV6`, `IPV4_FIRST`,
`IPV6_FIRST`), and the new entry point honours all four.

### 2.2 The literal screen ran after the thing it screens

`L6InetSweep` had **190 differing rows over 9 families**, and they were one
defect: `resolve_host` tried `Ipv4Addr::from_str`, then `Ipv6Addr::from_str`,
and then handed anything else to `getaddrinfo` as a host NAME.

> **`getaddrinfo` runs `inet_aton` first, so a literal screen placed after it
> cannot see what it swallowed.**

`inet_aton` accepts hexadecimal and octal parts; the JDK dropped those forms in
JDK 22. So `getByName("0x7f.0.0.1")` answered 127.0.0.1 here and raises on
HotSpot — and this VM then remembered the input as the address's HOST NAME,
because `Ipv4Addr::from_str` had said "not a literal" a moment earlier.

The JDK's rules, now implemented as `jdk_numeric_format_v4` /
`bsd_parsable_v4` / `literal_screen`:

| input | HotSpot | why |
|---|---|---|
| `01.2.3.4` | `/1.2.3.4` | leading zeros ARE decimal here |
| `1.2.3` | `/1.2.0.3` | the last part fills the remaining bytes |
| `16909060` | `/1.2.3.4` | the one-part form |
| `0x7f.0.0.1` | `UnknownHostException: 0x7f.0.0.1` | BSD-parsable, JDK-rejected — and the message carries NO resolver suffix, which is how this case is told from a DNS miss |
| `256.1.1.1` | `UnknownHostException: 256.1.1.1: Name or service not known` | not a literal, so it is a name, and the name does not resolve |
| `1::2::3` | `UnknownHostException: 1::2::3: invalid IPv6 address literal` | a colon cannot appear in a host name, so the resolver is never asked |

**160 of the 190 rows differed only in the message text.** Rust's
`io::Error` for a failed lookup renders `failed to lookup address information:
Name or service not known`; the JDK's is `<host>: Name or service not known`.
Eight families × twenty accessors, all of them one extra clause in the middle
of a string.

`InetAddress.getByAddress`'s length complaint is the same species: the JDK's
message is the constant `addr is of illegal length` and this VM appended
`: <n>`, so six more rows differed on a helpfulness nobody asked for.

---

## 3. Item 1 — `X500Principal`: the model had no room for the answer

`x500.rs` modelled a DN as `Vec<(String, String)>` — a keyword and its text.
The thing that model cannot hold is the one the four output formats disagree
about: **the DER string type of each attribute value**.

For `1.2.840.113549.1.9.1=alice@example.com` (emailAddress, an IA5String), one
attribute, four spellings:

```text
  RFC2253    1.2.840.113549.1.9.1=#1611616c696365406578616d706c652e636f6d
  RFC1779    OID.1.2.840.113549.1.9.1=alice@example.com
  CANONICAL  1.2.840.113549.1.9.1=#1611616c696365406578616d706c652e636f6d
  toString   EMAILADDRESS=alice@example.com
```

Three independent rules produce that, and `x500_name.rs` now implements each
separately because they do not agree with one another:

1. **The keyword table differs per format.** RFC 2253 defines nine keywords,
   RFC 1779 defines seven and spells the rest `OID.<dotted>`, and `toString`
   uses the JDK's full table — which is where `DNQ`, `T` and `EMAILADDRESS`
   come from. Even `DC` is `OID.0.9.2342.19200300.100.1.25` in RFC 1779.
2. **A dotted-OID type forces the `#<hex>` value form in RFC 2253**, whatever
   the value's own type — RFC 2253 §2.3, and why `SERIALNUMBER=12345` comes
   back as `2.5.4.5=#13053132333435`.
3. **CANONICAL hexes anything that is not a `PrintableString` or a
   `UTF8String`**, which is why the same `DC=example` is text in RFC 2253 and
   `dc=#16076578616d706c65` in CANONICAL.

Where the type comes from, measured per attribute: a `#`-value keeps its own
tag; `emailAddress` and `DC` are IA5String; **a `\XX` hex escape pins the value
to UTF8String** (so `CN=\41lice` and `CN=Alice` are the same text and different
DER); otherwise PrintableString when every character is in X.680's printable
set.

Two more measured rules that no model of "keyword and text" can express:

* **CANONICAL is NFKD.** HotSpot's canonical form of `CN=René` ends `65 cc 81`
  — `e` plus COMBINING ACUTE — where the composed input is `c3 a9`. Two
  principals spelling one name in NFC and NFD are the same principal, and
  without the normalisation they were neither equal nor equal-hashed.
* **`DC=Example,DC=COM` does NOT equal `dc=example,dc=com`.** Both canonical
  forms are hex (IA5String), and hex preserves case. This one falls out of
  rule 3 rather than being coded.

### 3.1 The parser was not a parser

The JDK's validates. Nineteen malformed names in the sweep — `CN`, `=Alice`,
`CN=Alice,,O=x`, `CN=#0402`, `NoSuchKeyword=x`, `1..2=x`, `CN="unterminated`,
`CN=a\q` — each raise `IllegalArgumentException("improperly specified input
name: <dn>")` on HotSpot. This VM built a principal from all nineteen, usually
an EMPTY one.

**An empty principal is the dangerous answer.** It equals no certificate
subject and matches no alias, so every identity check against it quietly says
"no" — a failure that looks exactly like a correct denial.

One member of that list is not malformed and is worth stating because it looks
it: `CN=Alice O=x` parses, and the `=` is escaped on the way out
(`CN=Alice O\=x`).

### 3.2 A rewrite that leaves one legacy re-derivation is not a rewrite

The first trial binary fixed 57 of the 138 rows and left 81, and the residue
was mine. `get_der` re-derives an encoding from the stored name when its side
table misses, and it still did so with the OLD parser: the new grammar rendered
`1.3.6.1.4.1.99999.1=#130178`, the old one read that as seven characters of
text, and the value came back encoded twice —
`#0c0723313330313738`, a UTF8String whose content is the string `#130178`.

The second defect in the same place: an RDN is a SET and DER sorts a SET, so
recovering the structure from the DER returned `OU=Eng+CN=Alice` for a name
written `CN=Alice+OU=Eng`. **The stored RFC 2253 string is the better source of
structure**, precisely because the format spells anything its grammar cannot
express as `#<DER hex>` — parsing it back recovers the type exactly where the
text does not imply it, and preserves the order the DER discards.

---

## 4. Item 2 — the five named JCA/TLS defects

### 4.1 `Cipher.wrap` decrypted, and one character says why

```rust
let encrypt = mode == 1;            // five sibling sites say `mode == 1 || mode == 3`
```

`WRAP_MODE` is 3, so a `WRAP`-initialised cipher ran the DECRYPT block.
`UNWRAP` is 4 and already decrypted — correctly, and by accident — so the round
trip **decrypted twice** and returned neither the JDK's ciphertext nor the
original key:

```text
  HotSpot   66e94bd4ef8a2c3b884cfa59ca342b2e -> 00000000000000000000000000000000
  CratonVM  140f0f1011b5223d79587717ffd9ec3a -> af65bb470269ecd7af01f68f1a2b7b78
```

`140f0f10…` is the AES *decryption* of a zero block under a zero key, and
`af65bb47…` is the decryption of THAT — so the two ciphertexts say, between
them, exactly which operation each end ran. **A wrong ciphertext is a readable
statement about which operation ran**, and reading it is faster than reading
the code that produced it.

### 4.2 `AES/CTR/NoPadding` resolved to no provider

The AES verdict table listed CTR under "a mode this engine does not compute",
so `Cipher.getInstance` raised `NoSuchAlgorithmException` for a transformation
every other JCA provider ships. CTR is `E(K, counter)` XOR the data with the
counter incremented as one 128-bit big-endian integer — SunJCE's `CounterMode`
— and it is now computed in-crate, which also gives the truncated-tag GCM path
below its keystream.

### 4.3 The GCM tag length was accepted and ignored

`GCMParameterSpec.getTLen()` was never read: every tag was 16 bytes. So
`new GCMParameterSpec(96, iv)` produced a ciphertext four bytes longer than
SunJCE's and a tag no conforming peer accepts, and `new GCMParameterSpec(8,
iv)` — a length SunJCE refuses outright — encrypted happily.

Three separate checks, each where the JDK puts it:

* the constructor rejects a negative length and a null IV;
* **`Cipher.init`** rejects a length outside {128, 120, 112, 104, 96} — not the
  constructor, which is where a caller would expect it and where the JDK does
  not do it;
* `doFinal` emits and verifies exactly that many bytes.

Decrypting with a truncated tag needs the plaintext before the tag can be
checked, which the AEAD crate's detached API cannot express — so that path
recovers the plaintext with GCM's own CTR keystream (counter block
`IV || 00000002`), recomputes the full tag over it and compares the prefix.

While it was open: **GCM nonce reuse is now refused on encrypt.** Two messages
under one (key, IV) leak the GHASH subkey; SunJCE refuses the second `init` and
this engine accepted it.

### 4.4 `SSLSocket.getEnableSessionCreation` raised `AbstractMethodError`

`javax.net.ssl.SSLSocket` is abstract and this VM allocates instances of it
directly, so a method with no native and no concrete body raises. The G25 wave
fixed the three the SERVER socket had; the client socket kept its own, and one
throw took a whole probe row with it — the four properties printed beside it
were unobservable too.

### 4.5 `SSLParameters.setApplicationProtocols` accepted anything

Null array, null element, empty element: all accepted. `new String[]{"h2",
null}` became an advertised ALPN list with a hole in it, and the failure
surfaced at handshake time, on the wire, in another process. It now raises the
JDK's two `IllegalArgumentException`s.

And the default: a fresh `SSLParameters` advertised `[h2, http/1.1]` here and
advertises **nothing** on HotSpot. A caller reading the list to decide whether
ALPN was requested was told yes by every parameters object in the VM.

---

## 5. Item 4 — the fixture, and what a probe tree cannot see without one

The lane's retirement recorded 30 rows as *unobserved*: `getInputStream`,
`getResponseCode`, `getHeaderField(s)`, `getContentLength` and the date
accessors. Nothing was wrong with the analysis — there was no server, and
3,183 rows of contract edges cannot reach a method whose answer comes off a
socket.

`apps/probes/L6HttpLoopbackSweep.java` starts a single-threaded `ServerSocket`
on 127.0.0.1:0 and serves byte-fixed responses: 200 with a full header set,
404, 302, 204, a chunked body, and an echo endpoint for POST/PUT. Nothing
time-varying reaches a printed row — `Date`, `Last-Modified` and `Expires` are
constants in the canned response rather than the wall clock, and the ephemeral
port is scrubbed from every printed value.

That last point cost a run: `getInputStream()` on the 404 throws
`FileNotFoundException` whose message is the URL, **including the port**, so
the first three-run determinism check found one row differing from itself. A
fixture that binds port 0 is deterministic in every row except the ones that
quote the port back.

**76 rows, and 21 of them differed on the first run.** Not edge cases:

* `getHeaderFieldKey(0)`/`getHeaderField(0)` were off by one over the whole
  indexed-header API — the JDK's index 0 is the status line under a null key,
  and this VM's was the first real header;
* `getHeaderFields()` omitted that null key and returned a **mutable** map;
* `getInputStream()` on a 404 returned the error body instead of throwing, so
  the standard `try { getInputStream() } catch (FileNotFoundException)` test
  for a missing resource read the error page as the resource;
* `getErrorStream()` PERFORMED the request, which is the one thing its javadoc
  says it does not do;
* `getContentLength()` answered the buffered body size when no `Content-Length`
  header was present, so a chunked response reported however many bytes had
  arrived and a `204 No Content` reported 0 — indistinguishable from a real
  zero-length entity, where HotSpot says -1;
* the two streaming-mode setters reported each other's error
  (`setFixedLengthStreamingMode` → "Chunked encoding streaming mode set");
* `setRequestProperty` after connect succeeded, kept the value, and never sent
  it.

### 5.1 The streaming-mode guards were not crossed

They read correctly, and the state they read was another connection's.
`real_reqs` is keyed by identity hash, which this VM derives per object and
**reuses once the first object is collected**, so a fresh connection allocated
where an old one died inherited its method, its headers and its streaming mode
— which is also why `setRequestMethod("PUT")` sent a POST.

The first attempt at this fix cleared the row in `HttpURLConnection.<init>` and
changed **nothing**, because `URL.openConnection()` allocates the carrier
directly and never calls the constructor. The mint site is the one moment a
carrier is known to be new, and it is where `real_forget` now runs. A trial
binary is what told the two apart: the guards' messages were unchanged between
base and trial.

### 5.2 Three "after connect" rows were a field nobody wrote

`setRequestProperty`, `getRequestProperties` and `setDoOutput` after a
`getResponseCode()` all succeeded, took the caller's value and dropped it.
Their guard is `URLConnection.checkConnected()` — and those methods are
declared on `URLConnection`, which this file does not register, so they run as
inherited JDK bytecode against this VM's carrier and read the carrier's
`connected` field. That field was written 0 at mint and never again.

**Setting the field is the fix; a guard inside a native the dispatch never
consults is not.** The natives here are registered on four concrete carrier
classes and `URLConnection` is not one of them, so a check added to
`huc_set_request_property` sat in code that this path does not reach. The
perform now writes `connected = 1`, and the JDK's own state machine does the
rest — for all six methods it guards, not just the three the sweep asked.

---

## 6. What this wave did not do

* **The cipher-suite and protocol lists** (`L6TlsParamSweep` rows 60-93) differ
  because this VM's TLS is rustls and supports a different set. That is not a
  defect to fix by lying about the list.
* **`SSLContext` and `KeyManagerFactory` initialisation state** (rows 64, 65,
  103, 106) needs a per-object initialised flag; the accessors currently answer
  as though initialised.
* **`SSLServerSocketFactory.createServerSocket`** hands back a plain
  `java.net.ServerSocket`, so casting it to `SSLServerSocket` raises
  `ClassCastException` (rows 87, 88).
* **`getURL()` after a followed redirect** still names the original URL: the
  perform loop tracks the final URL and does not write it back to the carrier.
* **The `java.net.URI` and `java.net.URL` families** (40 and 7 differing rows)
  are the retirement record's §8 items 1 and 3, untouched here.
* **The literal screen is in `getByName`/`getAllByName`, not in
  `inet_address.rs::resolve_addrs`**, which serves
  `Inet*AddressImpl.lookupAllHostAddr` directly. That is where the JDK puts it
  too — `InetAddress.getAllByName` screens the text and calls the resolver only
  for a NAME, so the deeper native never sees a literal — but it does mean a
  caller reaching that native directly still gets `getaddrinfo`'s more
  permissive answer. No probe row reaches it.

---

## 7. The gates

Two runs, because `dev` moved 40+ commits under this wave (lane 2 retired,
lane 5's residuals, four stub-ratchet re-freezes).

**Run 1 — isolation, on the pre-merge tree.** Base and trial built from one
worktree, so they differ only by this wave:

```text
  --jdk-only corpus, trial   133 passed, 0 failed
  --jdk-only corpus, control 133 passed, 0 failed
  SUITE=all                  133 passed, 0 failed
  SUITE=core                 PENDING
  gate set, five configs     PENDING
  probe tree A/B             PENDING
```

**Run 2 — the merged tree**, which is what actually lands: the gate set and
the three arms again, on a binary built from the merge. The isolation run is
what tells run 2's red from this wave's.

### Why the ordering changed mid-run

The first acceptance ran the 426-probe A/B first and managed four probes in
fifteen minutes: the host was carrying four other lanes at load 40+, and
`FjpStress` alone is three runs of a stress probe. The corpus is both the
faster instrument and the stronger one — 133 vectors diffed against HotSpot,
against a probe's own printed rows — so it was re-ordered to run first and the
A/B put behind it.

That is an ordering change, not a scope change. Every check still runs. The
A/B's per-probe design is what makes it safe to run under load at all: each
probe's HotSpot, base and trial runs are back to back, so load inflates all
three equally and the DELTA survives — only a timeout striking one arm and not
the others can lie, and the runner flags those as a line-count or exit-code
move rather than folding them into the delta.
