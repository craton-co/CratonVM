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
    /// absorbed into a flat name with `$`-style separators.
    Class {
        name: String,
        type_args: Vec<TypeArg>,
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
}

impl<'a> SigParser<'a> {
    fn new(s: &'a str) -> Self {
        SigParser {
            input: s.as_bytes(),
            pos: 0,
            depth: 0,
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
        String::from_utf8_lossy(&self.input[start..self.pos]).into_owned()
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
        String::from_utf8_lossy(&self.input[start..self.pos]).into_owned()
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
        // dimensions both descend through this function.
        if self.depth >= MAX_SIG_DEPTH {
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
        // Inner classes: `.Inner<...>` becomes `$Inner` in the dotted name
        // — that's the form Class.getName uses for nested types loaded
        // by HotSpot.
        while self.peek() == Some(b'.') {
            self.advance();
            let inner = self.read_ident();
            full_name.push('$');
            full_name.push_str(&inner);
            if self.peek() == Some(b'<') {
                let _ = self.parse_type_args();
            }
        }
        self.expect(b';');
        Some(TypeSig::Class {
            name: full_name,
            type_args,
        })
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

// ---------------------------------------------------------------------------
// Public parse API
// ---------------------------------------------------------------------------

/// Parse a class signature string.
pub fn parse_class_signature(sig: &str) -> Option<ClassSig> {
    SigParser::new(sig).parse_class_sig()
}

/// Parse a method signature string.
pub fn parse_method_signature(sig: &str) -> Option<MethodSig> {
    SigParser::new(sig).parse_method_sig()
}

/// Parse a field signature string (a single reference type signature).
pub fn parse_field_signature(sig: &str) -> Option<TypeSig> {
    SigParser::new(sig).parse_type_sig()
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
    /// The signature string failed to parse. Cached so a hot loop of
    /// "is this signature valid?" probes (verifier, JVMTI agents) does
    /// not re-walk the parser every call.
    Invalid,
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
        if self.map.len() >= SIGNATURE_CACHE_CAP {
            if let Some(victim) = self.order.pop_front() {
                self.map.remove(&victim);
            }
        }
        self.order.push_back(Arc::clone(&key));
        self.map.insert(key, value);
    }
}

fn signature_cache() -> &'static Mutex<SignatureCacheInner> {
    static SIGNATURE_CACHE: OnceLock<Mutex<SignatureCacheInner>> = OnceLock::new();
    SIGNATURE_CACHE.get_or_init(|| Mutex::new(SignatureCacheInner::new()))
}

/// Internal probe — returns the cached entry as an owned value (so the
/// lock is released before any further work). `None` means "no entry
/// for this signature"; `Some(ParsedSignature::Invalid)` means "we
/// have previously parsed this string and it was invalid".
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
        Some(ParsedSignature::Invalid) => return None,
        // Same signature was previously parsed as a different shape —
        // extremely unusual; fall through and re-parse without
        // updating the cache to avoid thrash.
        Some(_) => return parse_class_signature(sig).map(Arc::new),
        None => {}
    }
    let parsed = SigParser::new(sig).parse_class_sig();
    let mut cache = signature_cache().lock();
    match parsed {
        Some(c) => {
            let arc = Arc::new(c);
            cache.insert(Arc::clone(sig), ParsedSignature::Class(Arc::clone(&arc)));
            Some(arc)
        }
        None => {
            cache.insert(Arc::clone(sig), ParsedSignature::Invalid);
            None
        }
    }
}

/// Parse a method signature, consulting the global signature cache.
pub fn parse_method_signature_cached(sig: &Arc<str>) -> Option<Arc<MethodSig>> {
    match cache_probe(sig) {
        Some(ParsedSignature::Method(m)) => return Some(m),
        Some(ParsedSignature::Invalid) => return None,
        Some(_) => return parse_method_signature(sig).map(Arc::new),
        None => {}
    }
    let parsed = SigParser::new(sig).parse_method_sig();
    let mut cache = signature_cache().lock();
    match parsed {
        Some(m) => {
            let arc = Arc::new(m);
            cache.insert(Arc::clone(sig), ParsedSignature::Method(Arc::clone(&arc)));
            Some(arc)
        }
        None => {
            cache.insert(Arc::clone(sig), ParsedSignature::Invalid);
            None
        }
    }
}

/// Parse a field signature, consulting the global signature cache.
pub fn parse_field_signature_cached(sig: &Arc<str>) -> Option<Arc<TypeSig>> {
    match cache_probe(sig) {
        Some(ParsedSignature::Field(t)) => return Some(t),
        Some(ParsedSignature::Invalid) => return None,
        Some(_) => return parse_field_signature(sig).map(Arc::new),
        None => {}
    }
    let parsed = SigParser::new(sig).parse_type_sig();
    let mut cache = signature_cache().lock();
    match parsed {
        Some(t) => {
            let arc = Arc::new(t);
            cache.insert(Arc::clone(sig), ParsedSignature::Field(Arc::clone(&arc)));
            Some(arc)
        }
        None => {
            cache.insert(Arc::clone(sig), ParsedSignature::Invalid);
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
            TypeSig::Class { name, type_args } => {
                assert_eq!(name, "java/lang/Object");
                assert!(type_args.is_empty());
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
}
