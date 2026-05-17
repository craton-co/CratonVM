use crate::class_reader_error::ClassReaderError;

/// A byte buffer for reading binary data from a `.class` file.
///
/// Provides sequential reading of primitive types in big-endian byte order,
/// as specified by the JVM class file format.
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
        let pos = self.position;
        let bytes = self
            .data
            .get(pos..pos + count)
            .ok_or(ClassReaderError::UnexpectedEndOfData { position: pos })?;
        self.position = pos + count;
        Ok(bytes)
    }

    pub fn skip(&mut self, count: usize) -> Result<(), ClassReaderError> {
        let pos = self.position;
        if self.data.get(pos..pos + count).is_none() {
            return Err(ClassReaderError::UnexpectedEndOfData { position: pos });
        }
        self.position = pos + count;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_u8() {
        let data = [0xCA];
        let mut buf = ClassFileBuffer::new(&data);
        assert_eq!(buf.read_u8().unwrap(), 0xCA);
    }

    #[test]
    fn read_u16_big_endian() {
        let data = [0xCA, 0xFE];
        let mut buf = ClassFileBuffer::new(&data);
        assert_eq!(buf.read_u16().unwrap(), 0xCAFE);
    }

    #[test]
    fn read_u32_big_endian() {
        let data = [0xCA, 0xFE, 0xBA, 0xBE];
        let mut buf = ClassFileBuffer::new(&data);
        assert_eq!(buf.read_u32().unwrap(), 0xCAFEBABE);
    }

    #[test]
    fn read_past_end_returns_error() {
        let data = [0x01];
        let mut buf = ClassFileBuffer::new(&data);
        assert!(buf.read_u16().is_err());
    }

    #[test]
    fn read_bytes_slice() {
        let data = [1, 2, 3, 4, 5];
        let mut buf = ClassFileBuffer::new(&data);
        let bytes = buf.read_bytes(3).unwrap();
        assert_eq!(bytes, &[1, 2, 3]);
        assert_eq!(buf.position(), 3);
    }
}
