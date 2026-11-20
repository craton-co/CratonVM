// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use crate::class_reader_error::ClassReaderError;

/// A byte buffer for reading binary data from a `.class` file.
///
/// Provides sequential reading of primitive types in big-endian byte order,
/// as specified by the JVM class file format.
///
/// `Clone` (interpreter round i5 wave 111, lane L5): a trial decode of an
/// attribute HotSpot skips runs on a copy, so a failure leaves the original
/// where it was.
#[derive(Clone)]
pub struct ClassFileBuffer<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> ClassFileBuffer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.position
    }

    #[inline(always)]
    pub fn read_u8(&mut self) -> Result<u8, ClassReaderError> {
        let pos = self.position;
        // `get` lets the compiler elide the redundant index bounds check
        // that would arise from `self.data[pos]`.
        let bytes = self
            .data
            .get(pos..pos + 1)
            .ok_or(ClassReaderError::UnexpectedEndOfData { position: pos })?;
        self.position = pos + 1;
        // SAFETY/Justification: explicit length check above guarantees len == 1,
        // so `try_into` can never fail; `unwrap` compiles to a single load.
        let arr: [u8; 1] = bytes.try_into().unwrap();
        Ok(arr[0])
    }

    #[inline(always)]
    pub fn read_u16(&mut self) -> Result<u16, ClassReaderError> {
        let pos = self.position;
        let bytes = self
            .data
            .get(pos..pos + 2)
            .ok_or(ClassReaderError::UnexpectedEndOfData { position: pos })?;
        self.position = pos + 2;
        let arr: [u8; 2] = bytes.try_into().unwrap();
        Ok(u16::from_be_bytes(arr))
    }

    #[inline(always)]
    pub fn read_u32(&mut self) -> Result<u32, ClassReaderError> {
        let pos = self.position;
        let bytes = self
            .data
            .get(pos..pos + 4)
            .ok_or(ClassReaderError::UnexpectedEndOfData { position: pos })?;
        self.position = pos + 4;
        let arr: [u8; 4] = bytes.try_into().unwrap();
        Ok(u32::from_be_bytes(arr))
    }

    pub fn read_i32(&mut self) -> Result<i32, ClassReaderError> {
        self.read_u32().map(|v| v as i32)
    }

    #[inline(always)]
    pub fn read_i64(&mut self) -> Result<i64, ClassReaderError> {
        let pos = self.position;
        let bytes = self
            .data
            .get(pos..pos + 8)
            .ok_or(ClassReaderError::UnexpectedEndOfData { position: pos })?;
        self.position = pos + 8;
        let arr: [u8; 8] = bytes.try_into().unwrap();
        Ok(i64::from_be_bytes(arr))
    }

    pub fn read_f32(&mut self) -> Result<f32, ClassReaderError> {
        self.read_u32().map(f32::from_bits)
    }

    pub fn read_f64(&mut self) -> Result<f64, ClassReaderError> {
        self.read_i64().map(|v| f64::from_bits(v as u64))
    }

    #[inline(always)]
    pub fn read_bytes(&mut self, count: usize) -> Result<&'a [u8], ClassReaderError> {
        // Round 7 audit fix (MED #7 / round-4 #7): use `checked_add`
        // for `pos + count` so a caller passing an unvalidated `count`
        // (e.g. derived from a u32 length field on a 32-bit target,
        // or from `N * ENTRY_SIZE` arithmetic in the bulk parsers in
        // `attribute.rs`) can't wrap to a small `end` and silently
        // succeed with `self.data.get(pos..end)` returning a short
        // prefix. On overflow we return `UnexpectedEndOfData` — there
        // can't possibly be that many bytes left in the buffer.
        let pos = self.position;
        let end = pos
            .checked_add(count)
            .ok_or(ClassReaderError::UnexpectedEndOfData { position: pos })?;
        let bytes = self
            .data
            .get(pos..end)
            .ok_or(ClassReaderError::UnexpectedEndOfData { position: pos })?;
        self.position = end;
        Ok(bytes)
    }

    pub fn skip(&mut self, count: usize) -> Result<(), ClassReaderError> {
        // Round 7 audit fix (MED #7 / round-4 #7): same overflow
        // hardening as `read_bytes` — see comment above.
        let pos = self.position;
        let end = pos
            .checked_add(count)
            .ok_or(ClassReaderError::UnexpectedEndOfData { position: pos })?;
        if self.data.get(pos..end).is_none() {
            return Err(ClassReaderError::UnexpectedEndOfData { position: pos });
        }
        self.position = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
