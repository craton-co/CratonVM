// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Verification frame — the state tracked during bytecode verification.
//!
//! A `VerificationFrame` represents the types in local variables and on the
//! operand stack at a specific bytecode offset. The verifier maintains a
//! current frame and advances it through each instruction.

use cratonvm_reader::constant_pool::ConstantPool;
use cratonvm_reader::stack_map::StackMapFrame;

use super::vtype::{param_types_from_descriptor, ClassHierarchy, VType};
use cratonvm_types::error::LinkageError;

/// The verification frame: local variable types and operand stack types.
#[derive(Debug, Clone)]
pub struct VerificationFrame {
    /// Local variable types. May contain `Top` for undefined/unusable slots.
    pub locals: Vec<VType>,
    /// Operand stack types, bottom-to-top.
    pub stack: Vec<VType>,
    /// Maximum stack size (from Code attribute).
    max_stack: u16,
    /// Maximum local-slot count (from the `Code` attribute).
    ///
    /// SECURITY (JVMS §4.9.1 static constraint): the interpreter allocates
    /// exactly `max_locals` slots for the frame, so the verifier must never
    /// accept a local index at or above it. Bounding by `locals.len()` alone
    /// (the previous behaviour) was not equivalent in two directions:
    ///
    ///   * `initial_frame` sizes `locals` from the method descriptor and only
    ///     *pads* up to `max_locals`, so a method whose parameters need more
    ///     slots than it declares (`max_locals` under-declared) produced a
    ///     `locals` vector LONGER than the runtime frame, and every access in
    ///     that overhang verified clean and read out of bounds at run time;
    ///   * a `full_frame` in the `StackMapTable` may declare more locals than
    ///     `max_locals`, with the same effect.
    ///
    /// Kept in sync at the one place a declared frame becomes the current
    /// frame — [`VerificationFrame::pad_locals_to`], which is called with the
    /// method's real `max_locals` at every adoption site.
    max_locals: u16,
}

impl VerificationFrame {
    /// Build the initial frame for a method.
    ///
    /// Per JVM spec 4.10.1.6:
    /// - Instance methods: local[0] = `ObjectRef(class)` (or `UninitializedThis` for `<init>`)
    /// - Static methods: no implicit `this`
    /// - Parameters fill subsequent locals
    /// - Category-2 types occupy two slots (the type + Top)
    /// - Stack is empty
    pub fn initial_frame(
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        is_static: bool,
        max_locals: u16,
        max_stack: u16,
    ) -> Self {
        let mut locals = Vec::with_capacity(max_locals as usize);

        // Instance methods get `this` in local[0]
        if !is_static {
            if method_name == "<init>" {
                locals.push(VType::UninitializedThis);
            } else {
                locals.push(VType::ObjectRef(std::sync::Arc::from(class_name)));
            }
        }

        // Parse parameter types from descriptor and add to locals
        for param_type in param_types_from_descriptor(descriptor) {
            let is_cat2 = param_type.is_category2();
            locals.push(param_type);
            if is_cat2 {
                locals.push(VType::Top); // second slot for long/double
            }
        }

        // Fill remaining locals with Top
        while locals.len() < max_locals as usize {
            locals.push(VType::Top);
        }

        VerificationFrame {
            locals,
            stack: Vec::new(),
            max_stack,
            max_locals,
        }
    }

    /// Build a compact initial frame for StackMapTable derivation.
    ///
    /// Per JVM spec 4.7.4, the initial frame used to derive StackMapTable entries
    /// contains ONLY the method parameters (not padded to max_locals). AppendFrame
    /// and ChopFrame operations modify this compact frame.
    pub fn compact_initial_frame(
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        is_static: bool,
        max_stack: u16,
    ) -> Self {
        let mut locals = Vec::new();

        // Instance methods get `this` in local[0]
        if !is_static {
            if method_name == "<init>" {
                locals.push(VType::UninitializedThis);
            } else {
                locals.push(VType::ObjectRef(std::sync::Arc::from(class_name)));
            }
        }

        // Parse parameter types from descriptor and add to locals
        for param_type in param_types_from_descriptor(descriptor) {
            let is_cat2 = param_type.is_category2();
            locals.push(param_type);
            if is_cat2 {
                locals.push(VType::Top); // second slot for long/double
            }
        }

        // Do NOT pad to max_locals — StackMapTable frames are derived from this compact form
        VerificationFrame {
            locals,
            stack: Vec::new(),
            max_stack,
            // A compact frame (and everything derived from it by
            // `apply_stack_map_frame`) is a *declaration*, never the frame an
            // instruction executes against: the walk adopts it via
            // `pad_locals_to(max_locals)`, which installs the method's real
            // bound. Until then the only limit is the declared locals length,
            // which `local_limit` already applies.
            max_locals: u16::MAX,
        }
    }

    /// Apply a StackMapTable frame to produce a new verification frame.
    ///
    /// Per JVM spec 4.7.4, each frame type modifies the previous frame differently.
    pub fn apply_stack_map_frame(
        &self,
        frame: &StackMapFrame,
        cp: &ConstantPool,
    ) -> Result<Self, LinkageError> {
        match frame {
            StackMapFrame::SameFrame { .. } => {
                // Same locals, empty stack
                Ok(VerificationFrame {
                    locals: self.locals.clone(),
                    stack: Vec::new(),
                    max_stack: self.max_stack,
                    max_locals: self.max_locals,
                })
            }

            StackMapFrame::SameLocals1StackItem { stack, .. }
            | StackMapFrame::SameLocals1StackItemExtended { stack, .. } => {
                let vtype = VType::from_verification_type_info(stack, cp)?;
                let is_cat2 = vtype.is_category2();
                let mut new_stack = Vec::with_capacity(if is_cat2 { 2 } else { 1 });
                new_stack.push(vtype);
                // Long and Double occupy two stack slots (value + Top)
                if is_cat2 {
                    new_stack.push(VType::Top);
                }
                Ok(VerificationFrame {
                    locals: self.locals.clone(),
                    stack: new_stack,
                    max_stack: self.max_stack,
                    max_locals: self.max_locals,
                })
            }

            StackMapFrame::ChopFrame { chopped, .. } => {
                let mut locals = self.locals.clone();
                // Remove the last `chopped` logical locals.
                // Category-2 types (Long/Double) occupy two slots (value + Top),
                // so chopping one logical local may require popping 2 entries.
                for _ in 0..*chopped {
                    match locals.pop() {
                        None => {
                            return Err(verify_error("chop_frame: not enough locals to chop"));
                        }
                        Some(VType::Top) => {
                            // This Top may be the companion slot of a Long/Double.
                            // If so, also pop the Long/Double itself.
                            if let Some(prev) = locals.last() {
                                if prev.is_category2() {
                                    locals.pop();
                                }
                            }
                        }
                        Some(_) => {
                            // Single-slot type — already removed
                        }
                    }
                }
                Ok(VerificationFrame {
                    locals,
                    stack: Vec::new(),
                    max_stack: self.max_stack,
                    max_locals: self.max_locals,
                })
            }

            StackMapFrame::SameFrameExtended { .. } => Ok(VerificationFrame {
                locals: self.locals.clone(),
                stack: Vec::new(),
                max_stack: self.max_stack,
                max_locals: self.max_locals,
            }),

            StackMapFrame::AppendFrame {
                locals: new_locals, ..
            } => {
                let mut locals = self.locals.clone();
                for info in new_locals {
                    let vtype = VType::from_verification_type_info(info, cp)?;
                    let is_cat2 = vtype.is_category2();
                    locals.push(vtype);
                    // Long and Double occupy two local slots (value + Top)
                    if is_cat2 {
                        locals.push(VType::Top);
                    }
                }
                Ok(VerificationFrame {
                    locals,
                    stack: Vec::new(),
                    max_stack: self.max_stack,
                    max_locals: self.max_locals,
                })
            }

            StackMapFrame::FullFrame {
                locals: frame_locals,
                stack: frame_stack,
                ..
            } => {
                let mut locals = Vec::with_capacity(frame_locals.len());
                for info in frame_locals {
                    let vtype = VType::from_verification_type_info(info, cp)?;
                    let is_cat2 = vtype.is_category2();
                    locals.push(vtype);
                    // Long and Double occupy two local slots (value + Top)
                    if is_cat2 {
                        locals.push(VType::Top);
                    }
                }
                let mut stack = Vec::with_capacity(frame_stack.len());
                for info in frame_stack {
                    let vtype = VType::from_verification_type_info(info, cp)?;
                    let is_cat2 = vtype.is_category2();
                    stack.push(vtype);
                    // Long and Double occupy two stack slots (value + Top)
                    if is_cat2 {
                        stack.push(VType::Top);
                    }
                }
                Ok(VerificationFrame {
                    locals,
                    stack,
                    max_stack: self.max_stack,
                    max_locals: self.max_locals,
                })
            }
        }
    }

    /// Check if this frame is assignable to `target` (all slots assignable).
    pub fn is_assignable_to(
        &self,
        target: &VerificationFrame,
        hierarchy: &dyn ClassHierarchy,
    ) -> bool {
        // Stack must be the same length
        if self.stack.len() != target.stack.len() {
            return false;
        }

        // Each stack entry must be assignable
        for (a, b) in self.stack.iter().zip(target.stack.iter()) {
            if !a.is_assignable_to(b, hierarchy) {
                return false;
            }
        }

        // Each local must be assignable (up to target's length)
        // If target has more locals, they must all be Top in the source
        for (i, target_local) in target.locals.iter().enumerate() {
            let source_local = self.locals.get(i).unwrap_or(&VType::Top);
            if !source_local.is_assignable_to(target_local, hierarchy) {
                return false;
            }
        }

        true
    }

    /// Merge this frame with another, producing the least upper bound.
    pub fn merge(
        &self,
        other: &VerificationFrame,
        hierarchy: &dyn ClassHierarchy,
    ) -> Result<VerificationFrame, LinkageError> {
        if self.stack.len() != other.stack.len() {
            return Err(verify_error(
                "cannot merge frames with different stack sizes",
            ));
        }

        let stack: Vec<VType> = self
            .stack
            .iter()
            .zip(other.stack.iter())
            .map(|(a, b)| a.merge(b, hierarchy))
            .collect();

        let max_len = self.locals.len().max(other.locals.len());
        let mut locals = Vec::with_capacity(max_len);
        for i in 0..max_len {
            let a = self.locals.get(i).unwrap_or(&VType::Top);
            let b = other.locals.get(i).unwrap_or(&VType::Top);
            locals.push(a.merge(b, hierarchy));
        }

        Ok(VerificationFrame {
            locals,
            stack,
            max_stack: self.max_stack,
            // Conservative: a merged frame may only address the slots BOTH
            // predecessors could address.
            max_locals: self.max_locals.min(other.max_locals),
        })
    }

    // ----- Stack operations -------------------------------------------------

    /// Push a type onto the operand stack.
    pub fn push(&mut self, vtype: VType) -> Result<(), LinkageError> {
        if self.stack.len() >= self.max_stack as usize {
            // Name the numbers. "stack overflow during verification" alone
            // cannot distinguish an under-declared `max_stack` in the class
            // file from the verifier over-counting a category-2 value, and
            // the two have opposite fixes — the Infinispan
            // `ConfigurationBuilder` retransform rejection
            // (`cacheautoconfigurationtests-…-20260805`) burned a whole
            // investigation on exactly that ambiguity.
            return Err(verify_error(&format!(
                "stack overflow during verification: pushing {vtype:?} onto a \
                 {}-deep stack would exceed max_stack={} (stack: {:?})",
                self.stack.len(),
                self.max_stack,
                self.stack,
            )));
        }
        self.stack.push(vtype);
        Ok(())
    }

    /// Pop a type from the operand stack.
    pub fn pop(&mut self) -> Result<VType, LinkageError> {
        self.stack
            .pop()
            .ok_or_else(|| verify_error("stack underflow during verification"))
    }

    /// Pop a type and check that it's assignable to `expected`.
    pub fn pop_expect(
        &mut self,
        expected: &VType,
        hierarchy: &dyn ClassHierarchy,
    ) -> Result<VType, LinkageError> {
        let actual = self.pop()?;
        if !actual.is_assignable_to(expected, hierarchy) {
            return Err(verify_error(&format!(
                "expected {expected:?} on stack, found {actual:?}"
            )));
        }
        Ok(actual)
    }

    /// Clear the stack (used when entering exception handlers).
    pub fn clear_stack(&mut self) {
        self.stack.clear();
    }

    /// Pad locals to max_locals by filling with Top.
    /// StackMapTable-derived frames may have fewer locals than max_locals.
    ///
    /// This is also the point at which a declared frame becomes an *executable*
    /// frame, so it installs the method's `max_locals` bound (see the field
    /// docs on [`VerificationFrame::max_locals`]). It never widens the bound: a
    /// declared frame carrying more locals than `max_locals` keeps the smaller
    /// limit and the overhang becomes unaddressable.
    pub fn pad_locals_to(&mut self, max_locals: u16) {
        self.max_locals = max_locals;
        while self.locals.len() < max_locals as usize {
            self.locals.push(VType::Top);
        }
    }

    /// The method's declared `max_locals`.
    pub fn max_locals(&self) -> u16 {
        self.max_locals
    }

    /// Does any slot of this frame still hold the uninitialized `this`?
    ///
    /// JVMS §4.10.1.9: a constructor may not `return` while its receiver is
    /// still `uninitializedThis` — the caller would observe an object whose
    /// superclass constructor never ran.
    pub fn has_uninitialized_this(&self) -> bool {
        self.locals.iter().any(|t| *t == VType::UninitializedThis)
            || self.stack.iter().any(|t| *t == VType::UninitializedThis)
    }

    // ----- Local variable operations ----------------------------------------

    /// Number of local slots this frame may address.
    ///
    /// The tighter of "slots the runtime frame has" (`max_locals`) and "slots
    /// this frame describes" (`locals.len()`). See the field docs on
    /// [`VerificationFrame::max_locals`] for why `locals.len()` alone is not a
    /// sound bound.
    fn local_limit(&self) -> usize {
        (self.max_locals as usize).min(self.locals.len())
    }

    /// Load a type from a local variable slot.
    pub fn local_load(&self, index: u16) -> Result<&VType, LinkageError> {
        let limit = self.local_limit();
        if (index as usize) >= limit {
            return Err(verify_error(&format!(
                "local variable index {index} out of range (addressable slots: {limit}, \
                 max_locals: {})",
                self.max_locals
            )));
        }
        Ok(&self.locals[index as usize])
    }

    /// Load a category-2 (`long` / `double`) value from a local variable pair.
    ///
    /// JVMS §4.10.1.6: a `long`/`double` occupies slots `index` and `index+1`;
    /// the upper half must still be the `Top` that was written with the base.
    /// A pair whose upper half has been overwritten by an intervening
    /// category-1 store is *split* and must not be read back as a wide value.
    pub fn local_load_wide(&self, index: u16, expected: &VType) -> Result<(), LinkageError> {
        let upper = index.checked_add(1).ok_or_else(|| {
            verify_error(&format!(
                "category-2 local load at index {index} would address slot {}, \
                 which overflows the local index space",
                u32::from(index) + 1
            ))
        })?;
        let base = self.local_load(index)?;
        if base != expected {
            return Err(verify_error(&format!(
                "local {index} is {base:?}, expected {expected:?}"
            )));
        }
        let hi = self.local_load(upper)?;
        if *hi != VType::Top {
            return Err(verify_error(&format!(
                "category-2 local pair at index {index} is split: slot {upper} is {hi:?}, \
                 expected the Top upper half"
            )));
        }
        Ok(())
    }

    /// Store a type into a local variable slot.
    ///
    /// Writing slot `index` invalidates a category-2 value based at `index - 1`
    /// (JVMS §4.10.1.6): this store overwrites that value's `Top` upper half,
    /// so the base must become unusable. Without this the frame would still
    /// claim slot `index - 1` holds a whole `long`/`double` while half of it
    /// has been replaced by an unrelated category-1 value — a `lload` of that
    /// slot would then verify clean and read a torn value at run time.
    pub fn local_store(&mut self, index: u16, vtype: VType) -> Result<(), LinkageError> {
        let idx = index as usize;
        let limit = self.local_limit();
        if idx >= limit {
            return Err(verify_error(&format!(
                "local variable index {index} out of range (addressable slots: {limit}, \
                 max_locals: {})",
                self.max_locals
            )));
        }
        self.invalidate_cat2_base_below(idx);
        self.locals[idx] = vtype;
        Ok(())
    }

    /// Store a category-2 (`long` / `double`) value into a local variable pair.
    ///
    /// Writes the base at `index` and its `Top` upper half at `index + 1`,
    /// both bounds-checked against `max_locals`. The `index + 1` computation is
    /// checked: `index` comes from a `wide`-prefixed operand and may be
    /// `u16::MAX`, where the previous `*index + 1` overflowed — a panic in a
    /// debug build and a wrap to slot 0 in release, both driven directly by
    /// attacker-supplied bytecode.
    pub fn local_store_wide(&mut self, index: u16, vtype: VType) -> Result<(), LinkageError> {
        let upper = index.checked_add(1).ok_or_else(|| {
            verify_error(&format!(
                "category-2 local store at index {index} would address slot {}, \
                 which overflows the local index space",
                u32::from(index) + 1
            ))
        })?;
        let lo = index as usize;
        let hi = upper as usize;
        let limit = self.local_limit();
        if hi >= limit {
            return Err(verify_error(&format!(
                "category-2 local store at index {index} needs slots {index} and {upper}, \
                 but only {limit} local slots are addressable (max_locals: {})",
                self.max_locals
            )));
        }
        self.invalidate_cat2_base_below(lo);
        self.locals[lo] = vtype;
        self.locals[hi] = VType::Top;
        Ok(())
    }

    /// If slot `idx - 1` holds a category-2 base, demote it to `Top`: a write
    /// to `idx` is a write to that value's upper half.
    fn invalidate_cat2_base_below(&mut self, idx: usize) {
        if idx > 0 && self.locals[idx - 1].is_category2() {
            self.locals[idx - 1] = VType::Top;
        }
    }

    /// Get the current stack depth.
    pub fn stack_depth(&self) -> usize {
        self.stack.len()
    }
}

fn verify_error(message: &str) -> LinkageError {
    LinkageError::VerifyError {
        class_name: String::new(),
        method_name: String::new(),
        message: message.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vtype::VType;
    use std::sync::Arc;

    struct MockHierarchy;

    impl ClassHierarchy for MockHierarchy {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            if child == parent || parent == "java/lang/Object" {
                return true;
            }
            // String implements CharSequence (used by frame-merge tests).
            matches!(
                (child, parent),
                ("java/lang/String", "java/lang/CharSequence")
            )
        }

        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }

        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    #[test]
    fn initial_frame_static_void() {
        let frame = VerificationFrame::initial_frame(
            "com/example/Foo",
            "bar",
            "()V",
            true, // static
            1,
            2,
        );
        // Static method with no params: all locals are Top
        assert_eq!(frame.locals, vec![VType::Top]);
        assert!(frame.stack.is_empty());
    }

    #[test]
    fn initial_frame_instance_method() {
        let frame = VerificationFrame::initial_frame(
            "com/example/Foo",
            "bar",
            "(I)V",
            false, // instance
            3,
            2,
        );
        // local[0] = Foo, local[1] = Int, local[2] = Top
        assert_eq!(
            frame.locals[0],
            VType::ObjectRef(Arc::from("com/example/Foo"))
        );
        assert_eq!(frame.locals[1], VType::Int);
        assert_eq!(frame.locals[2], VType::Top);
    }

    #[test]
    fn initial_frame_constructor() {
        let frame =
            VerificationFrame::initial_frame("com/example/Foo", "<init>", "()V", false, 1, 1);
        assert_eq!(frame.locals[0], VType::UninitializedThis);
    }

    #[test]
    fn initial_frame_long_param() {
        let frame = VerificationFrame::initial_frame("Foo", "m", "(J)V", true, 3, 1);
        // local[0] = Long, local[1] = Top (second slot), local[2] = Top
        assert_eq!(frame.locals[0], VType::Long);
        assert_eq!(frame.locals[1], VType::Top);
        assert_eq!(frame.locals[2], VType::Top);
    }

    #[test]
    fn push_pop() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 4);
        frame.push(VType::Int).unwrap();
        frame.push(VType::Float).unwrap();
        assert_eq!(frame.stack_depth(), 2);

        let top = frame.pop().unwrap();
        assert_eq!(top, VType::Float);
        assert_eq!(frame.stack_depth(), 1);
    }

    #[test]
    fn pop_empty_stack_errors() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        assert!(frame.pop().is_err());
    }

    #[test]
    fn push_overflow_errors() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        frame.push(VType::Int).unwrap();
        assert!(frame.push(VType::Int).is_err());
    }

    #[test]
    fn pop_expect_matching() {
        let h = MockHierarchy;
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 2);
        frame.push(VType::Int).unwrap();
        assert!(frame.pop_expect(&VType::Int, &h).is_ok());
    }

    #[test]
    fn pop_expect_wrong_type() {
        let h = MockHierarchy;
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 2);
        frame.push(VType::Float).unwrap();
        assert!(frame.pop_expect(&VType::Int, &h).is_err());
    }

    #[test]
    fn local_load_store() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 3, 1);
        frame.local_store(1, VType::Int).unwrap();
        assert_eq!(frame.local_load(1).unwrap(), &VType::Int);
    }

    #[test]
    fn local_out_of_range_errors() {
        let frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        assert!(frame.local_load(5).is_err());
    }

    #[test]
    fn frame_is_assignable_to_self() {
        let h = MockHierarchy;
        let frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        assert!(frame.is_assignable_to(&frame, &h));
    }

    #[test]
    fn frame_merge_same_is_same() {
        let h = MockHierarchy;
        let frame = VerificationFrame::initial_frame("Foo", "m", "(I)V", true, 2, 2);
        let merged = frame.merge(&frame, &h).unwrap();
        assert_eq!(merged.locals, frame.locals);
    }

    #[test]
    fn frame_merge_different_stack_depth_errors() {
        let h = MockHierarchy;
        let mut frame1 = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 2);
        let frame2 = frame1.clone();
        frame1.push(VType::Int).unwrap();
        assert!(frame1.merge(&frame2, &h).is_err());
    }

    #[test]
    fn frame_assignable_string_to_charsequence_local() {
        // Regression: at a StackMapTable merge point, a local typed as
        // String in the current frame must be assignable to a declared
        // local of CharSequence (String implements CharSequence).
        let h = MockHierarchy;
        let mut current = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        current.locals[0] = VType::ObjectRef(Arc::from("java/lang/String"));
        let mut declared = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        declared.locals[0] = VType::ObjectRef(Arc::from("java/lang/CharSequence"));
        assert!(current.is_assignable_to(&declared, &h));
    }

    #[test]
    fn frame_assignable_int_local_to_top_declared() {
        // Regression (B2): a StackMapTable full_frame may declare a local
        // slot as Top to indicate the value is unused past the merge point.
        // The current frame may still carry a concrete type (e.g. Int) from
        // a fall-through path; it must still be considered assignable since
        // Top is the top of the verification type lattice (JVMS 4.10.1.2).
        let h = MockHierarchy;
        let mut current = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
        current.locals[3] = VType::Int;
        let mut declared = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
        declared.locals[3] = VType::Top;
        assert!(current.is_assignable_to(&declared, &h));
    }

    // ----- max_locals bound (JVMS §4.9.1) -----------------------------------

    #[test]
    fn local_index_beyond_max_locals_rejected_even_when_described() {
        // `initial_frame` sizes `locals` from the descriptor and only pads up
        // to `max_locals`. Here the descriptor needs 3 slots (this + long) but
        // `max_locals` is 1, so slots 1 and 2 are DESCRIBED by the frame while
        // the runtime frame does not have them. They must be unaddressable.
        let frame = VerificationFrame::initial_frame("Foo", "m", "(J)V", false, 1, 1);
        assert!(
            frame.locals.len() > 1,
            "descriptor over-fills the locals vec"
        );
        assert!(frame.local_load(0).is_ok());
        assert!(
            frame.local_load(1).is_err(),
            "slot 1 is past max_locals and must not verify"
        );
        assert!(frame.local_load(2).is_err());
    }

    #[test]
    fn local_store_beyond_max_locals_rejected() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "(J)V", false, 1, 1);
        assert!(frame.local_store(2, VType::Int).is_err());
    }

    #[test]
    fn pad_locals_to_installs_the_max_locals_bound() {
        // A compact (StackMapTable-derived) frame is unbounded until adopted;
        // adoption is `pad_locals_to`, which installs the real bound.
        let mut compact = VerificationFrame::compact_initial_frame("Foo", "m", "(I)V", true, 2);
        assert_eq!(compact.locals.len(), 1);
        compact.pad_locals_to(4);
        assert_eq!(compact.max_locals(), 4);
        assert_eq!(compact.locals.len(), 4);
        assert!(compact.local_load(3).is_ok());
        assert!(compact.local_load(4).is_err());
    }

    #[test]
    fn pad_locals_to_never_widens_past_max_locals() {
        // A `full_frame` may declare MORE locals than `max_locals`; the
        // overhang must become unaddressable rather than raising the bound.
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 6, 1);
        frame.pad_locals_to(2);
        assert_eq!(frame.locals.len(), 6, "padding never shrinks the vec");
        assert!(frame.local_load(1).is_ok());
        assert!(
            frame.local_load(2).is_err(),
            "slot 2 is described but past max_locals"
        );
    }

    // ----- category-2 slot pairing (JVMS §4.10.1.6) -------------------------

    #[test]
    fn cat2_local_pair_round_trips() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
        frame.local_store_wide(1, VType::Long).unwrap();
        assert_eq!(frame.locals[1], VType::Long);
        assert_eq!(frame.locals[2], VType::Top);
        assert!(frame.local_load_wide(1, &VType::Long).is_ok());
    }

    #[test]
    fn storing_over_the_upper_half_invalidates_the_cat2_base() {
        // `lstore_1; istore_2` overwrites the long's upper half. The base must
        // become `Top`, otherwise a later `lload_1` would verify clean and read
        // a torn value at run time.
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
        frame.local_store_wide(1, VType::Long).unwrap();
        frame.local_store(2, VType::Int).unwrap();
        assert_eq!(
            frame.locals[1],
            VType::Top,
            "cat-2 base must be invalidated"
        );
        assert!(
            frame.local_load_wide(1, &VType::Long).is_err(),
            "the split pair must not read back as a long"
        );
    }

    #[test]
    fn cat2_load_rejects_a_split_pair() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
        // Hand-build a split pair: a Long base whose upper half is an Int.
        frame.locals[1] = VType::Long;
        frame.locals[2] = VType::Int;
        assert!(frame.local_load_wide(1, &VType::Long).is_err());
    }

    #[test]
    fn cat2_store_at_last_slot_rejected() {
        // `max_locals = 2` leaves slots 0 and 1; a long at slot 1 needs slot 2.
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 2, 1);
        assert!(frame.local_store_wide(1, VType::Double).is_err());
        assert!(frame.local_store_wide(0, VType::Double).is_ok());
    }

    #[test]
    fn cat2_store_at_u16_max_does_not_overflow() {
        // A `wide lstore 65535` used to compute `index + 1` unchecked: a debug
        // panic, and a wrap to slot 0 in release. Both are driven straight from
        // attacker-supplied bytecode, so this must be a clean rejection.
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
        assert!(frame.local_store_wide(u16::MAX, VType::Long).is_err());
        assert!(frame.local_load_wide(u16::MAX, &VType::Long).is_err());
    }

    // ----- uninitializedThis tracking ---------------------------------------

    #[test]
    fn constructor_initial_frame_reports_uninitialized_this() {
        let frame = VerificationFrame::initial_frame("Foo", "<init>", "()V", false, 1, 1);
        assert!(frame.has_uninitialized_this());
    }

    #[test]
    fn ordinary_method_has_no_uninitialized_this() {
        let frame = VerificationFrame::initial_frame("Foo", "m", "()V", false, 1, 1);
        assert!(!frame.has_uninitialized_this());
    }

    #[test]
    fn clear_stack() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 4);
        frame.push(VType::Int).unwrap();
        frame.push(VType::Float).unwrap();
        frame.clear_stack();
        assert!(frame.stack.is_empty());
    }
}
