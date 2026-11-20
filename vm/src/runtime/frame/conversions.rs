use super::*;

impl From<Vec<Frame>> for FrameStack {
    #[inline]
    fn from(mut buf: Vec<Frame>) -> Self {
        // By-value frames own their buffers; the new stack's slab is empty,
        // so every one of them was pushed at its bottom mark.
        for frame in buf.iter_mut() {
            frame.slab_mark = 0;
        }
        let depth = buf.len();
        Self {
            buf,
            depth,
            reloc_epoch: 0,
            code_moves: 0,
            conversion_pending: false,
            cold: None,
            slab: SlotSlab::new(),
            requested_frame_limit: 0,
            excuse_cell: 0,
        }
    }
}

impl From<FrameStack> for Vec<Frame> {
    #[inline]
    fn from(mut fs: FrameStack) -> Vec<Frame> {
        fs.trim_retired();
        // The slab is dropped with `fs`: every frame leaves with owned buffers.
        for frame in fs.buf.iter_mut() {
            frame.detach_from_slab();
        }
        fs.buf
    }
}

impl std::ops::Deref for FrameStack {
    type Target = [Frame];
    #[inline(always)]
    fn deref(&self) -> &[Frame] {
        self.live()
    }
}

impl std::ops::DerefMut for FrameStack {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [Frame] {
        self.live_mut()
    }
}

impl<I: std::slice::SliceIndex<[Frame]>> std::ops::Index<I> for FrameStack {
    type Output = I::Output;
    #[inline(always)]
    fn index(&self, index: I) -> &Self::Output {
        std::ops::Index::index(self.live(), index)
    }
}

impl<I: std::slice::SliceIndex<[Frame]>> std::ops::IndexMut<I> for FrameStack {
    #[inline(always)]
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        std::ops::IndexMut::index_mut(self.live_mut(), index)
    }
}

impl<'a> IntoIterator for &'a FrameStack {
    type Item = &'a Frame;
    type IntoIter = std::slice::Iter<'a, Frame>;
    #[inline(always)]
    fn into_iter(self) -> Self::IntoIter {
        self.live().iter()
    }
}

impl<'a> IntoIterator for &'a mut FrameStack {
    type Item = &'a mut Frame;
    type IntoIter = std::slice::IterMut<'a, Frame>;
    #[inline(always)]
    fn into_iter(self) -> Self::IntoIter {
        self.live_mut().iter_mut()
    }
}

impl IntoIterator for FrameStack {
    type Item = Frame;
    type IntoIter = std::vec::IntoIter<Frame>;
    #[inline(always)]
    fn into_iter(mut self) -> Self::IntoIter {
        // Retired slots are not frames anyone may see.
        self.trim_retired();
        // The slab goes with `self`: every frame leaves with owned buffers.
        for frame in self.buf.iter_mut() {
            frame.detach_from_slab();
        }
        self.buf.into_iter()
    }
}

impl FromIterator<Frame> for FrameStack {
    fn from_iter<T: IntoIterator<Item = Frame>>(iter: T) -> Self {
        let buf: Vec<Frame> = iter.into_iter().collect();
        Self::from(buf)
    }
}
