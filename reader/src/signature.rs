// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.8 — JVM generic signature parser (JVMS §4.7.9.1).
//!
//! Parses the strings stored in the `Signature` attribute and exposes
//! a small AST that the VM uses to build runtime
//! `java.lang.reflect.{ParameterizedType, TypeVariable, WildcardType,
//! GenericArrayType}` objects.
//!
//! The grammar (JVMS §4.7.9.1):
//!
//! ```text
//! ClassSignature           ::= TypeParameters? SuperclassSignature SuperinterfaceSignature*
//! TypeParameters           ::= '<' TypeParameter+ '>'
//! TypeParameter            ::= Identifier ClassBound InterfaceBound*
//! ClassBound               ::= ':' ReferenceTypeSignature?
//! InterfaceBound           ::= ':' ReferenceTypeSignature
//! SuperclassSignature      ::= ClassTypeSignature
//! SuperinterfaceSignature  ::= ClassTypeSignature
//!
//! TypeSignature            ::= BaseType | ReferenceTypeSignature
//! BaseType                 ::= 'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z' | 'V'
//! ReferenceTypeSignature   ::= ClassTypeSignature | TypeVariableSignature
//!                            | ArrayTypeSignature
//! ClassTypeSignature       ::= 'L' PackageSpecifier? SimpleClassTypeSignature
//!                              ClassTypeSignatureSuffix* ';'
//! ClassTypeSignatureSuffix ::= '.' SimpleClassTypeSignature
//! SimpleClassTypeSignature ::= Identifier TypeArguments?
//! TypeArguments            ::= '<' TypeArgument+ '>'
//! TypeArgument             ::= '*' | ('+'|'-')? ReferenceTypeSignature
//! TypeVariableSignature    ::= 'T' Identifier ';'
//! ArrayTypeSignature       ::= '[' TypeSignature
//!
//! MethodSignature          ::= TypeParameters? '(' TypeSignature* ')' Result ThrowsSignature*
//! Result                   ::= TypeSignature | VoidDescriptor
//! VoidDescriptor           ::= 'V'
//! ThrowsSignature          ::= '^' (ClassTypeSignature | TypeVariableSignature)
//!
//! FieldSignature           ::= ReferenceTypeSignature
//! ```

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

/// A formal type parameter, e.g. `T:Ljava/lang/Object;` (T extends Object).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeParam {
    pub name: String,
    pub class_bound: Option<TypeSig>,
    pub interface_bounds: Vec<TypeSig>,
}

/// A parsed type signature.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeSig {
    /// A base (primitive or void) type: B, C, D, F, I, J, S, Z, V
    Base(char),
    /// A class type, possibly parameterized: `Ljava/lang/String;` or
    /// `Ljava/util/List<TT;>;`. Inner-class suffixes (`.Inner`) are
    /// absorbed into a flat `$`-joined `name` (matching `Class.getName` for
    /// nested types) for the raw/`resolve()` view, while `owner` carries the
    /// enclosing type as a separate node so type-variable resolvers can walk
    /// the owner chain. `owner` is `Some` only when the signature used the
    /// `ClassTypeSignatureSuffix` form `Outer<...>.Inner<...>` — i.e. the
    /// enclosing class is parameterized in this context (HotSpot reifies such
    /// a signature as a `ParameterizedType` whose `getOwnerType()` is the
    /// enclosing `ParameterizedType`). It stays `None` for the common
    /// `Outer$Inner` form (which `read_class_name` reads as one flat name).
    Class {
        name: String,
        type_args: Vec<TypeArg>,
        owner: Option<Box<TypeSig>>,
    },
    /// A type variable reference: `TT;`
    TypeVar(String),
    /// An array type: `[<TypeSig>`
    Array(Box<TypeSig>),
}

/// A type argument inside a `<...>` block.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeArg {
    /// Concrete type argument
    Exact(TypeSig),
    /// `? extends T` (upper-bounded wildcard)
    Extends(TypeSig),
    /// `? super T` (lower-bounded wildcard)
    Super(TypeSig),
    /// `?` (unbounded wildcard)
    Unbounded,
}

/// Parsed class signature: type params + superclass + interfaces.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassSig {
    pub type_params: Vec<TypeParam>,
    pub super_class: TypeSig,
    pub interfaces: Vec<TypeSig>,
}

/// Parsed method signature: type params + param types + return type + throws.
#[derive(Debug, Clone, PartialEq)]
pub struct MethodSig {
    pub type_params: Vec<TypeParam>,
    pub param_types: Vec<TypeSig>,
    pub return_type: TypeSig,
    pub throws: Vec<TypeSig>,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Maximum nesting depth for type signatures. Nested generics and array
/// dimensions recurse in the parser; an untrusted `Signature` attribute of
/// nothing but `[` (or deeply nested `<...>`) would otherwise overflow the
/// stack. 256 comfortably exceeds anything a real compiler emits.
const MAX_SIG_DEPTH: usize = 256;

struct SigParser<'a> {
    input: &'a [u8],
    pos: usize,
    /// Current recursion depth — incremented when descending into a nested
    /// type signature, decremented on the way back out.
    depth: usize,
    /// Sticky flag: set once any recursive descent (`parse_type_sig`) is
    /// refused because it would exceed [`MAX_SIG_DEPTH`]. The flag is read by
    /// the public `parse_*` entry points so that hostile signatures with
    /// pathological generic nesting (`Lp<Lp<...>;>;`) fail outright instead
    /// of returning a partial parse — the `parse_type_args` loop otherwise
    /// silently `break`s on the inner `None` and the outer
    /// `parse_class_type_sig` returns `Some(...)`.
    depth_exceeded: bool,
}

/// Materialize a parsed identifier byte-slice into an owned `String`.
///
/// Perf: the previous body was `String::from_utf8_lossy(bytes).into_owned()`,
/// which unconditionally walks the slice through `Utf8Chunks` looking for
/// invalid sequences (so it can substitute U+FFFD) and builds a `Cow` before
/// the `into_owned` copy. In this parser the input *always* originates from a
/// `&str` (`SigParser::new` takes `&'a str` and stores `s.as_bytes()`), and
/// the identifier scanners only ever advance over ASCII bytes, so every slice
/// handed here is already valid UTF-8 in the overwhelmingly common case.
/// `str::from_utf8` is a single cheap validation pass with no chunk/Cow
/// machinery; on success we do exactly one allocation+copy (`to_owned`), the
/// same as before but without the replacement-scan overhead. We fall back to
/// the lossy path only on the (here unreachable) malformed-input case so
/// behavior is byte-for-byte identical to the original on every input,
/// valid or not.
#[inline]
fn slice_to_string(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

impl<'a> SigParser<'a> {
    fn new(s: &'a str) -> Self {
        SigParser {
            input: s.as_bytes(),
            pos: 0,
            depth: 0,
            depth_exceeded: false,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let b = self.input.get(self.pos).copied()?;
        self.pos += 1;
        Some(b)
    }

    fn expect(&mut self, ch: u8) -> bool {
        if self.peek() == Some(ch) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.input.len()
    }

    /// Read a Java-style identifier (letters, digits, _$/.). Slashes occur
    /// in package names; dots appear in inner-class suffixes.
    fn read_ident(&mut self) -> String {
        let start = self.pos;
        while let Some(b) = self.peek() {
            match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$' | b'/' | b'.' => {
                    self.pos += 1;
                }
                _ => break,
            }
        }
        slice_to_string(&self.input[start..self.pos])
    }

    fn read_class_name(&mut self) -> String {
        let start = self.pos;
        while let Some(b) = self.peek() {
            match b {
                b';' | b'<' | b'.' => break,
                _ => {
                    self.pos += 1;
                }
            }
        }
        slice_to_string(&self.input[start..self.pos])
    }

    /// Parse `<TypeParam+>`.
    fn parse_type_params(&mut self) -> Vec<TypeParam> {
        let mut params = Vec::new();
        if !self.expect(b'<') {
            return params;
        }
        while self.peek() != Some(b'>') && !self.at_end() {
            if let Some(tp) = self.parse_type_param() {
                params.push(tp);
            } else {
                break;
            }
        }
        self.expect(b'>');
        params
    }

    /// Parse a single type parameter: `Name:ClassBound:InterfaceBound...`.
    fn parse_type_param(&mut self) -> Option<TypeParam> {
        let name = self.read_ident();
        if name.is_empty() {
            return None;
        }
        if !self.expect(b':') {
            return None;
        }
        // Class bound (may be empty if immediately followed by ':' — see
        // `<T::Lfoo/Bar;>` which means "class bound is implicit Object,
        // additional interface bound is foo.Bar").
        let class_bound = if self.peek() != Some(b':') && self.peek() != Some(b'>') {
            self.parse_type_sig()
        } else {
            None
        };
        let mut interface_bounds = Vec::new();
        while self.peek() == Some(b':') {
            self.advance();
            if let Some(sig) = self.parse_type_sig() {
                interface_bounds.push(sig);
            }
        }
        Some(TypeParam {
            name,
            class_bound,
            interface_bounds,
        })
    }

    fn parse_type_sig(&mut self) -> Option<TypeSig> {
        // Bound recursion on untrusted input — nested generics and array
        // dimensions both descend through this function. We *also* latch
        // `depth_exceeded` so callers up-stack of `parse_type_args` (which
        // intentionally `break`s on an inner `None` to handle malformed
        // type-arg lists gracefully) can still distinguish a depth-guard
        // refusal from a normal end-of-list. Without the sticky flag a
        // pathological `Lp<Lp<...>;>;` signature returns `Some(partial)`
        // from the outermost `parse_class_type_sig`, defeating the guard.
        if self.depth >= MAX_SIG_DEPTH {
            self.depth_exceeded = true;
            return None;
        }
        self.depth += 1;
        let result = self.parse_type_sig_inner();
        self.depth -= 1;
        result
    }

    fn parse_type_sig_inner(&mut self) -> Option<TypeSig> {
        match self.peek()? {
            b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => {
                let ch = self.advance()? as char;
                Some(TypeSig::Base(ch))
            }
            b'V' => {
                self.advance();
                Some(TypeSig::Base('V'))
            }
            b'L' => self.parse_class_type_sig(),
            b'T' => self.parse_type_var_sig(),
            b'[' => {
                self.advance();
                let component = self.parse_type_sig()?;
                Some(TypeSig::Array(Box::new(component)))
            }
            _ => None,
        }
    }

    /// Parse `Lpkg/Cls<args>?(.Inner<args>?)*;`.
    fn parse_class_type_sig(&mut self) -> Option<TypeSig> {
        self.expect(b'L');
        let mut full_name = self.read_class_name();
        let type_args = if self.peek() == Some(b'<') {
            self.parse_type_args()
        } else {
            Vec::new()
        };
        // Inner classes: `.Inner<...>` becomes `$Inner` in the flat name —
        // that's the form Class.getName uses for nested types loaded by
        // HotSpot, so it's what `resolve()`/`getRawType()` needs.
        //
        // Each suffix ALSO nests the type built so far as the new node's
        // `owner`, preserving the enclosing class's type arguments. Previously
        // the suffix's own `<args>` were parsed and DISCARDED and the outer
        // `type_args` were (wrongly) left attached to the `$`-joined inner
        // name — so `Outer<X>.Inner<Y>` resolved to `Inner` carrying `X` and
        // no owner, instead of `Inner<Y>` owned by `Outer<X>`. That broke any
        // resolver that binds a type variable declared by an enclosing generic
        // class (Spring's ResolvableType.resolveFromOuterClass,
        // GenericTypeResolver.getTypeVariableMap on inner classes).
        let mut current = TypeSig::Class {
            name: full_name.clone(),
            type_args,
            owner: None,
        };
        while self.peek() == Some(b'.') {
            self.advance();
            let inner = self.read_ident();
            full_name.push('$');
            full_name.push_str(&inner);
            let inner_args = if self.peek() == Some(b'<') {
                self.parse_type_args()
            } else {
                Vec::new()
            };
            current = TypeSig::Class {
                name: full_name.clone(),
                type_args: inner_args,
                owner: Some(Box::new(current)),
            };
        }
        self.expect(b';');
        Some(current)
    }

    fn parse_type_var_sig(&mut self) -> Option<TypeSig> {
        self.expect(b'T');
        let name = self.read_ident();
        self.expect(b';');
        Some(TypeSig::TypeVar(name))
    }

    fn parse_type_args(&mut self) -> Vec<TypeArg> {
        let mut args = Vec::new();
        if !self.expect(b'<') {
            return args;
        }
        while self.peek() != Some(b'>') && !self.at_end() {
            if let Some(arg) = self.parse_type_arg() {
                args.push(arg);
            } else {
                break;
            }
        }
        self.expect(b'>');
        args
    }

    fn parse_type_arg(&mut self) -> Option<TypeArg> {
        match self.peek()? {
            b'*' => {
                self.advance();
                Some(TypeArg::Unbounded)
            }
            b'+' => {
                self.advance();
                let sig = self.parse_type_sig()?;
                Some(TypeArg::Extends(sig))
            }
            b'-' => {
                self.advance();
                let sig = self.parse_type_sig()?;
                Some(TypeArg::Super(sig))
            }
            _ => {
                let sig = self.parse_type_sig()?;
                Some(TypeArg::Exact(sig))
            }
        }
    }

    fn parse_class_sig(&mut self) -> Option<ClassSig> {
        let type_params = if self.peek() == Some(b'<') {
            self.parse_type_params()
        } else {
            Vec::new()
        };
        let super_class = self.parse_type_sig()?;
        let mut interfaces = Vec::new();
        while !self.at_end() {
            if let Some(sig) = self.parse_type_sig() {
                interfaces.push(sig);
            } else {
                break;
            }
        }
        Some(ClassSig {
            type_params,
            super_class,
            interfaces,
        })
    }

    fn parse_method_sig(&mut self) -> Option<MethodSig> {
        let type_params = if self.peek() == Some(b'<') {
            self.parse_type_params()
        } else {
            Vec::new()
        };
        if !self.expect(b'(') {
            return None;
        }
        let mut param_types = Vec::new();
        while self.peek() != Some(b')') && !self.at_end() {
            param_types.push(self.parse_type_sig()?);
        }
        self.expect(b')');
        let return_type = self.parse_type_sig()?;
        let mut throws = Vec::new();
        while self.peek() == Some(b'^') {
            self.advance();
            if let Some(sig) = self.parse_type_sig() {
                throws.push(sig);
            }
        }
        Some(MethodSig {
            type_params,
            param_types,
            return_type,
            throws,
        })
    }
}

fn finish_full_parse<T>(parser: &SigParser<'_>, parsed: Option<T>) -> Option<T> {
    if parser.depth_exceeded || !parser.at_end() {
        None
    } else {
        parsed
    }
}

// ---------------------------------------------------------------------------
// Public parse API
// ---------------------------------------------------------------------------

/// Parse a class signature string.
pub fn parse_class_signature(sig: &str) -> Option<ClassSig> {
    let mut p = SigParser::new(sig);
    let r = p.parse_class_sig();
    finish_full_parse(&p, r)
}

/// Parse a method signature string.
pub fn parse_method_signature(sig: &str) -> Option<MethodSig> {
    let mut p = SigParser::new(sig);
    let r = p.parse_method_sig();
    finish_full_parse(&p, r)
}

/// Parse a field signature string (a single reference type signature).
///
/// Returns `None` if the input is malformed *or* if any nested recursion
/// hit the `MAX_SIG_DEPTH` cap. The depth-guard latches the parser's
/// `depth_exceeded` flag so that hostile signatures whose inner failure is
/// otherwise swallowed by `parse_type_args`' `break`-on-`None` loop still
/// surface as a hard reject (see also the in-crate regression test
/// `deeply_nested_signature_is_rejected_not_overflow`).
pub fn parse_field_signature(sig: &str) -> Option<TypeSig> {
    let mut p = SigParser::new(sig);
    let r = p.parse_type_sig();
    finish_full_parse(&p, r)
}

// ---------------------------------------------------------------------------
// Cached parse API — round-8 HIGH reader finding
// ---------------------------------------------------------------------------
//
// Generic signature parsing is pure: the AST produced for a given
// signature string never changes. Spring's `ResolvableType` (and any
// reflective generics consumer) walks the same signatures over and over
// — e.g. `Ljava/util/List<Ljava/lang/String;>;` re-parsed on every
// `Class.getGenericSuperclass()` probe. Caching the parsed forms keyed
// on the signature string eliminates the per-probe parse cost.
//
// The cache is bounded with a simple FIFO eviction (capacity ~8 K) to
// keep memory cost predictable: the parsed AST is a few hundred bytes
// at most per signature, so 8 K entries ≈ a few MB worst-case. Below
// the cap inserts are O(1); at the cap we evict the oldest entry.

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};

const SIGNATURE_CACHE_CAP: usize = 8192;

/// Parsed forms returned by the cached parse APIs. Mirrors the three
/// grammar entry points (class / method / field) so the cache can be
/// shared across all signature kinds without losing the parsed AST
/// type.
#[derive(Clone)]
pub enum ParsedSignature {
    Class(Arc<ClassSig>),
    Method(Arc<MethodSig>),
    Field(Arc<TypeSig>),
    /// The signature string failed to parse — for the shapes whose flag
    /// is set. Cached so a hot loop of "is this signature valid?" probes
    /// (verifier, JVMTI agents) does not re-walk the parser every call.
    ///
    /// The verdict MUST stay shape-qualified. The three grammars are not
    /// nested: `<T:Ljava/lang/Object;>Ljava/lang/Object;` is a valid
    /// *class* signature and an invalid *field* signature, and
    /// `(I)V` is a valid *method* signature and an invalid class one. A
    /// single unqualified `Invalid` meant whichever entry point probed
    /// the string first poisoned it for the other two, so a later lookup
    /// returned `None` without re-parsing and generic type information
    /// silently vanished depending on call order.
    ///
    /// A signature that is garbage under all three grammars (notably a
    /// depth bomb, which `finish_full_parse` rejects for every shape)
    /// accumulates all three flags and is still fully memoized.
    Invalid {
        class: bool,
        method: bool,
        field: bool,
    },
}

/// Which of the three grammar entry points a cached verdict belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SigShape {
    Class,
    Method,
    Field,
}

impl ParsedSignature {
    /// An `Invalid` verdict recorded for exactly one shape.
    fn invalid_for(shape: SigShape) -> Self {
        ParsedSignature::Invalid {
            class: shape == SigShape::Class,
            method: shape == SigShape::Method,
            field: shape == SigShape::Field,
        }
    }

    /// True when this entry is a memoized rejection *for `shape`* — the
    /// only case in which a cached `Invalid` may short-circuit a probe.
    fn rejects(&self, shape: SigShape) -> bool {
        match self {
            ParsedSignature::Invalid {
                class,
                method,
                field,
            } => match shape {
                SigShape::Class => *class,
                SigShape::Method => *method,
                SigShape::Field => *field,
            },
            _ => false,
        }
    }
}

struct SignatureCacheInner {
    map: FxHashMap<Arc<str>, ParsedSignature>,
    /// FIFO order for eviction; the front is the oldest entry.
    order: VecDeque<Arc<str>>,
}

impl SignatureCacheInner {
    fn new() -> Self {
        Self {
            map: FxHashMap::with_capacity_and_hasher(256, Default::default()),
            order: VecDeque::with_capacity(256),
        }
    }

    fn insert(&mut self, key: Arc<str>, value: ParsedSignature) {
        // Perf/correctness: dedup the recency queue on re-insert.
        //
        // Previously `order` was unconditionally `push_back`'d on every
        // insert, even when the key was already present. Re-parsing a hot
        // signature (e.g. after a `Some(_)` shape-mismatch fall-through, or
        // a benign concurrent double-parse where two threads miss the probe
        // and both insert) therefore appended a *duplicate* entry. Two bad
        // effects followed:
        //   1. `order.len()` grew without bound past `map.len()`, so the
        //      `map.len() >= CAP` eviction trigger and the queue drifted out
        //      of lockstep — the queue could hold thousands of stale handles
        //      for keys still live in the map (memory the FIFO never reclaims
        //      until the key is finally evicted), and
        //   2. eviction popped the *oldest* occurrence of a key that may have
        //      been re-inserted (and is therefore still hot), evicting a live
        //      entry while a duplicate of it lingered deeper in the queue.
        //
        // Fix: if the key already lives in the map, this is a value refresh —
        // move its single recency slot to the back instead of appending a new
        // one. The map keeps exactly one entry per key and `order` keeps
        // exactly one slot per key, so `order.len() == map.len()` always and
        // eviction can never drop a key that was just touched.
        if self.map.contains_key(&key) {
            // Refresh: relocate the existing recency slot to the back. The
            // queue holds one slot per live key, so there is at most one match
            // to remove. Compare by string *content*, not pointer: the map is
            // keyed by content, so a re-insert may arrive via a different
            // `Arc<str>` allocation carrying the same signature bytes (e.g. a
            // distinct constant-pool entry). Matching on content guarantees we
            // always find — and remove — the stale slot, never leaving a
            // duplicate behind. Linear scan, but the cache is small and
            // re-inserts of an already-present key are the cold path (a true
            // hit short-circuits before `insert` is ever called).
            if let Some(idx) = self.order.iter().position(|k| **k == *key) {
                self.order.remove(idx);
            }
            self.order.push_back(Arc::clone(&key));
            self.map.insert(key, value);
            return;
        }
        if self.map.len() >= SIGNATURE_CACHE_CAP {
            if let Some(victim) = self.order.pop_front() {
                self.map.remove(&victim);
            }
        }
        self.order.push_back(Arc::clone(&key));
        self.map.insert(key, value);
    }

    /// Record an `Invalid` verdict for one shape, merging it into any
    /// verdict already stored for this string.
    ///
    /// Merging (rather than overwriting) is what keeps one cache entry per
    /// string while still remembering *every* shape that has been rejected
    /// — so a string that is garbage under all three grammars (a depth
    /// bomb) ends up fully memoized after each entry point has probed it
    /// once, exactly as the old unqualified `Invalid` did.
    ///
    /// A successfully parsed entry is never clobbered: the probe already
    /// routes a shape mismatch to the uncached parser without touching the
    /// cache, so a success can only be observed here after a racing insert
    /// from another thread, and the parsed AST is the more valuable entry.
    fn record_invalid(&mut self, key: &Arc<str>, shape: SigShape) {
        let merged = match self.map.get(key) {
            Some(ParsedSignature::Invalid {
                class,
                method,
                field,
            }) => ParsedSignature::Invalid {
                class: *class || shape == SigShape::Class,
                method: *method || shape == SigShape::Method,
                field: *field || shape == SigShape::Field,
            },
            // Parsed successfully under some other shape — leave it be.
            Some(_) => return,
            None => ParsedSignature::invalid_for(shape),
        };
        self.insert(Arc::clone(key), merged);
    }
}

fn signature_cache() -> &'static Mutex<SignatureCacheInner> {
    static SIGNATURE_CACHE: OnceLock<Mutex<SignatureCacheInner>> = OnceLock::new();
    SIGNATURE_CACHE.get_or_init(|| Mutex::new(SignatureCacheInner::new()))
}

/// Internal probe — returns the cached entry as an owned value (so the
/// lock is released before any further work). `None` means "no entry
/// for this signature"; `Some(ParsedSignature::Invalid { .. })` means
/// "we have previously parsed this string, under the shapes whose flag
/// is set, and it was invalid for those" — see
/// `ParsedSignature::rejects`.
fn cache_probe(sig: &Arc<str>) -> Option<ParsedSignature> {
    let cache = signature_cache().lock();
    cache.map.get(sig).cloned()
}

/// Parse a class signature, consulting the global signature cache.
///
/// Callers that already hold the signature as `Arc<str>` (the
/// constant-pool path) should prefer this — the cache key is the same
/// allocation, so a hit is a single hash + pointer compare with no
/// extra allocations.
pub fn parse_class_signature_cached(sig: &Arc<str>) -> Option<Arc<ClassSig>> {
    match cache_probe(sig) {
        Some(ParsedSignature::Class(c)) => return Some(c),
        // Only a rejection recorded for THIS shape may short-circuit; an
        // `Invalid` left by the method/field entry points says nothing
        // about the class grammar, so fall through, parse, and merge our
        // verdict into the entry below.
        Some(e) if e.rejects(SigShape::Class) => return None,
        Some(ParsedSignature::Invalid { .. }) => {}
        // Same signature was previously parsed as a different shape —
        // extremely unusual; fall through and re-parse without
        // updating the cache to avoid thrash.
        Some(_) => return parse_class_signature(sig).map(Arc::new),
        None => {}
    }
    let mut p = SigParser::new(sig);
    let parsed = p.parse_class_sig();
    // Honor the sticky depth-exceeded guard exactly like the uncached
    // `parse_class_signature`: a hostile signature that nests past
    // `MAX_SIG_DEPTH` must be treated (and cached) as Invalid, otherwise
    // the cached path would accept — and memoize — a partial AST the
    // uncached path rejects, defeating the recursion/DoS guard.
    let parsed = finish_full_parse(&p, parsed);
    let mut cache = signature_cache().lock();
    match parsed {
        Some(c) => {
            let arc = Arc::new(c);
            cache.insert(Arc::clone(sig), ParsedSignature::Class(Arc::clone(&arc)));
            Some(arc)
        }
        None => {
            cache.record_invalid(sig, SigShape::Class);
            None
        }
    }
}

/// Parse a method signature, consulting the global signature cache.
pub fn parse_method_signature_cached(sig: &Arc<str>) -> Option<Arc<MethodSig>> {
    match cache_probe(sig) {
        Some(ParsedSignature::Method(m)) => return Some(m),
        // Shape-qualified rejection — see `parse_class_signature_cached`.
        Some(e) if e.rejects(SigShape::Method) => return None,
        Some(ParsedSignature::Invalid { .. }) => {}
        Some(_) => return parse_method_signature(sig).map(Arc::new),
        None => {}
    }
    let mut p = SigParser::new(sig);
    let parsed = p.parse_method_sig();
    // Honor the sticky depth-exceeded guard exactly like the uncached
    // `parse_method_signature` (see `parse_class_signature_cached`).
    let parsed = finish_full_parse(&p, parsed);
    let mut cache = signature_cache().lock();
    match parsed {
        Some(m) => {
            let arc = Arc::new(m);
            cache.insert(Arc::clone(sig), ParsedSignature::Method(Arc::clone(&arc)));
            Some(arc)
        }
        None => {
            cache.record_invalid(sig, SigShape::Method);
            None
        }
    }
}

/// Parse a field signature, consulting the global signature cache.
pub fn parse_field_signature_cached(sig: &Arc<str>) -> Option<Arc<TypeSig>> {
    match cache_probe(sig) {
        Some(ParsedSignature::Field(t)) => return Some(t),
        // Shape-qualified rejection — see `parse_class_signature_cached`.
        Some(e) if e.rejects(SigShape::Field) => return None,
        Some(ParsedSignature::Invalid { .. }) => {}
        Some(_) => return parse_field_signature(sig).map(Arc::new),
        None => {}
    }
    let mut p = SigParser::new(sig);
    let parsed = p.parse_type_sig();
    // Honor the sticky depth-exceeded guard exactly like the uncached
    // `parse_field_signature` (see `parse_class_signature_cached`).
    let parsed = finish_full_parse(&p, parsed);
    let mut cache = signature_cache().lock();
    match parsed {
        Some(t) => {
            let arc = Arc::new(t);
            cache.insert(Arc::clone(sig), ParsedSignature::Field(Arc::clone(&arc)));
            Some(arc)
        }
        None => {
            cache.record_invalid(sig, SigShape::Field);
            None
        }
    }
}

/// Clear the signature cache. Exposed for tests and shutdown.
pub fn clear_signature_cache() {
    let mut cache = signature_cache().lock();
    cache.map.clear();
    cache.order.clear();
}

/// Current size of the signature cache — exposed for diagnostics/tests.
pub fn signature_cache_len() -> usize {
    signature_cache().lock().map.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_class_sig() {
        let sig = parse_class_signature("Ljava/lang/Object;").unwrap();
        assert!(sig.type_params.is_empty());
        match &sig.super_class {
            TypeSig::Class {
                name,
                type_args,
                owner,
            } => {
                assert_eq!(name, "java/lang/Object");
                assert!(type_args.is_empty());
                assert!(owner.is_none());
            }
            other => panic!("expected Class, got {:?}", other),
        }
    }

    #[test]
    fn parse_nested_class_owner_chain() {
        // `Outer<Integer>.Inner<Long>` as a field signature: the inner type
        // carries its OWN args (`Long`) and an owner node for `Outer<Integer>`,
        // with the flat `$`-joined raw name. Regression for the bug where the
        // inner args were discarded and the outer args left on the inner name.
        let sig =
            parse_field_signature("LOuter<Ljava/lang/Integer;>.Inner<Ljava/lang/Long;>;").unwrap();
        match sig {
            TypeSig::Class {
                name,
                type_args,
                owner,
            } => {
                assert_eq!(name, "Outer$Inner");
                assert_eq!(type_args.len(), 1);
                assert_eq!(
                    type_args[0],
                    TypeArg::Exact(TypeSig::Class {
                        name: "java/lang/Long".into(),
                        type_args: vec![],
                        owner: None,
                    })
                );
                let owner = owner.expect("owner must be present");
                match *owner {
                    TypeSig::Class {
                        name,
                        type_args,
                        owner,
                    } => {
                        assert_eq!(name, "Outer");
                        assert_eq!(
                            type_args[0],
                            TypeArg::Exact(TypeSig::Class {
                                name: "java/lang/Integer".into(),
                                type_args: vec![],
                                owner: None,
                            })
                        );
                        assert!(owner.is_none());
                    }
                    other => panic!("expected owner Class, got {:?}", other),
                }
            }
            other => panic!("expected Class, got {:?}", other),
        }
    }

    #[test]
    fn parse_void_method() {
        let sig = parse_method_signature("()V").unwrap();
        assert!(sig.param_types.is_empty());
        assert!(matches!(sig.return_type, TypeSig::Base('V')));
    }

    #[test]
    fn trailing_garbage_rejected_by_uncached_parsers() {
        assert!(parse_class_signature("Ljava/lang/Object;garbage").is_none());
        assert!(parse_method_signature("()Vgarbage").is_none());
        assert!(parse_field_signature("Ljava/lang/Object;garbage").is_none());
    }

    #[test]
    fn trailing_garbage_rejected_after_complex_generic_signatures() {
        assert!(parse_class_signature(
            "<T:Ljava/lang/Object;>Ljava/lang/Object;Ljava/io/Serializable;garbage"
        )
        .is_none());
        assert!(parse_method_signature(concat!(
            "<T:Ljava/lang/Object;>",
            "(Ljava/util/List<TT;>;)",
            "Ljava/util/List<TT;>;",
            "^Ljava/lang/Exception;garbage"
        ))
        .is_none());
        assert!(parse_field_signature("[Ljava/util/List<+Ljava/lang/Number;>;garbage").is_none());
    }

    #[test]
    fn trailing_garbage_rejected_by_cached_parsers() {
        let class_sig: Arc<str> = Arc::from("Ljava/lang/Object;cached_class_garbage");
        let method_sig: Arc<str> = Arc::from("()Vcached_method_garbage");
        let field_sig: Arc<str> = Arc::from("Ljava/lang/Object;cached_field_garbage");

        assert!(parse_class_signature_cached(&class_sig).is_none());
        assert!(parse_method_signature_cached(&method_sig).is_none());
        assert!(parse_field_signature_cached(&field_sig).is_none());
    }

    #[test]
    fn deeply_nested_signature_is_rejected_not_overflow() {
        // A pathological array signature `[[[...I` nested far past the
        // depth cap must fail to parse rather than overflow the stack.
        let bomb = format!("{}I", "[".repeat(100_000));
        assert!(parse_field_signature(&bomb).is_none());

        // Deeply-nested generic type arguments are also bounded: build
        // `Lp<Lp<Lp<...>;>;>;` to a hostile depth and confirm no overflow.
        let mut nested = String::new();
        for _ in 0..100_000 {
            nested.push_str("Lp<");
        }
        nested.push_str("Lp;");
        for _ in 0..100_000 {
            nested.push_str(">;");
        }
        assert!(parse_field_signature(&nested).is_none());
    }

    #[test]
    fn cached_parse_round_trips() {
        // Use a signature unique to this test so we don't race with
        // sibling tests that share the global signature cache.
        let key: Arc<str> = Arc::from("()Lround_trips_marker;");
        let m = parse_method_signature_cached(&key).expect("cached parse");
        assert!(m.param_types.is_empty());
        // Hit returns the same Arc (refcount > 1 because the cache
        // holds one reference too).
        let m2 = parse_method_signature_cached(&key).expect("cached hit");
        assert!(Arc::ptr_eq(&m, &m2));
    }

    #[test]
    fn cached_invalid_is_remembered() {
        // Use a unique invalid signature so we don't race with other
        // tests on the shared cache.
        let bad: Arc<str> = Arc::from("not a sig // invalid_remembered_marker");
        assert!(parse_field_signature_cached(&bad).is_none());
        // Second call still returns None — the cache should record
        // the Invalid verdict and short-circuit.
        assert!(parse_field_signature_cached(&bad).is_none());
    }

    #[test]
    fn cached_invalid_verdict_is_shape_qualified() {
        // Regression: the memoized `Invalid` verdict used to be recorded
        // against the signature *string* alone, not the (string, shape)
        // pair. `<T:Ljava/lang/Object;>L...;` is a perfectly valid CLASS
        // signature but is NOT a valid FIELD signature (a field signature
        // is a single type signature, which cannot start with `<`). So a
        // caller that probed the field shape first poisoned the entry, and
        // a later class lookup returned `None` without ever re-parsing —
        // generic type information silently disappeared depending on
        // lookup order.
        //
        // The unique class name keeps this key off the process-global
        // cache's other entries (cf. `cached_and_uncached_agree_on_depth_bomb`).
        // It has to be unique *within* the grammar: a trailing `//marker`
        // comment would leave the parser short of `at_end()` and make the
        // string invalid for every shape.
        let key: Arc<str> = Arc::from("<T:Ljava/lang/Object;>Lshapequal/InvalidVerdictMarker;");

        // Ground truth from the uncached parsers: valid as a class
        // signature, invalid as a field signature.
        assert!(
            parse_class_signature(&key).is_some(),
            "uncached class parse must accept a generic class signature"
        );
        assert!(
            parse_field_signature(&key).is_none(),
            "uncached field parse must reject a class signature"
        );

        // Poison the entry via the field shape first...
        assert!(
            parse_field_signature_cached(&key).is_none(),
            "cached field parse must agree with the uncached verdict"
        );
        // ...and the class shape must still parse it.
        assert!(
            parse_class_signature_cached(&key).is_some(),
            "an Invalid verdict recorded for the field shape must not \
             short-circuit a class lookup"
        );
        // Both orders, and the memoized second call, stay consistent.
        assert!(
            parse_class_signature_cached(&key).is_some(),
            "class verdict must stay Some once memoized"
        );
        assert!(
            parse_field_signature_cached(&key).is_none(),
            "field verdict must stay None"
        );
    }

    #[test]
    fn invalid_verdicts_merge_into_one_entry() {
        // Shape-qualifying the verdict must not cost memoization: a string
        // that is garbage under all three grammars still ends up with one
        // cache entry carrying all three rejection flags, so every entry
        // point short-circuits after its own first probe.
        let bad: Arc<str> = Arc::from("not a sig // merge_invalid_marker");
        assert!(parse_field_signature_cached(&bad).is_none());
        assert!(parse_class_signature_cached(&bad).is_none());
        assert!(parse_method_signature_cached(&bad).is_none());

        let cache = signature_cache().lock();
        match cache.map.get(&bad) {
            Some(ParsedSignature::Invalid {
                class,
                method,
                field,
            }) => {
                assert!(*class && *method && *field, "all shapes must be recorded");
            }
            _ => panic!("expected a merged Invalid entry"),
        }
        // One string, one entry — the merge must not fan out into three.
        assert_eq!(
            cache.order.iter().filter(|k| ***k == *bad).count(),
            1,
            "merging must keep exactly one recency slot per key"
        );
    }

    #[test]
    fn cached_and_uncached_agree_on_depth_bomb() {
        // Regression for the depth-guard bypass: a generic signature
        // nested past MAX_SIG_DEPTH must be rejected on BOTH the cached
        // and uncached paths. Previously the cached path discarded the
        // parser and never read `depth_exceeded`, so it accepted (and
        // memoized) a partial AST the uncached path rejects.
        let mut nested = String::new();
        for _ in 0..100_000 {
            nested.push_str("Lp<");
        }
        nested.push_str("Lp;");
        for _ in 0..100_000 {
            nested.push_str(">;");
        }
        // Unique marker so we don't collide with the sibling
        // `deeply_nested_signature_is_rejected_not_overflow` test or
        // race on the shared cache.
        nested.push_str("// cached_depth_bomb_marker");
        let key: Arc<str> = Arc::from(nested.as_str());

        // Uncached verdict is the ground truth.
        assert!(
            parse_field_signature(&key).is_none(),
            "uncached path must reject a depth-bombed signature"
        );
        // Cached path must agree on the FIRST (parsing) call...
        assert!(
            parse_field_signature_cached(&key).is_none(),
            "cached path must honor the depth-exceeded guard"
        );
        // ...and on subsequent (memoized) calls — the verdict cached
        // must be Invalid, not a partial AST.
        assert!(
            parse_field_signature_cached(&key).is_none(),
            "cached path must remember the Invalid verdict"
        );
    }

    #[test]
    fn reinsert_keeps_order_and_map_in_lockstep() {
        // Regression for the LRU dedup fix: re-inserting an already-present
        // key must REFRESH its single recency slot, never append a duplicate.
        // Otherwise `order.len()` drifts past `map.len()` and eviction can
        // drop a still-hot key.
        let mut cache = SignatureCacheInner::new();
        let k: Arc<str> = Arc::from("Ldedup/Marker;");

        cache.insert(
            Arc::clone(&k),
            ParsedSignature::invalid_for(SigShape::Field),
        );
        assert_eq!(cache.map.len(), 1);
        assert_eq!(cache.order.len(), 1);

        // Re-insert the SAME content many times (including via a distinct
        // Arc allocation carrying the same bytes — the map keys by content,
        // so the dedup must match on content, not pointer identity).
        for _ in 0..100 {
            cache.insert(
                Arc::clone(&k),
                ParsedSignature::invalid_for(SigShape::Field),
            );
            let fresh_alloc: Arc<str> = Arc::from("Ldedup/Marker;");
            cache.insert(fresh_alloc, ParsedSignature::invalid_for(SigShape::Field));
        }

        // The map still holds exactly one entry and the recency queue is in
        // lockstep — no duplicate slots accumulated.
        assert_eq!(cache.map.len(), 1, "map must hold one entry per key");
        assert_eq!(
            cache.order.len(),
            cache.map.len(),
            "order must stay in lockstep with map (one slot per key)"
        );
    }
}
