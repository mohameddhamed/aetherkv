#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOp {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

/// Atomic WriteBatch representing a group of mutations to be applied together.
#[derive(Debug, Default, Clone)]
pub struct WriteBatch {
    ops: Vec<BatchOp>,
    size_in_bytes: usize,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        self.size_in_bytes += key.len() + value.len() + 8;
        self.ops.push(BatchOp::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        });
    }

    pub fn delete(&mut self, key: &[u8]) {
        self.size_in_bytes += key.len() + 8;
        self.ops.push(BatchOp::Delete { key: key.to_vec() });
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    #[inline(always)]
    pub fn size_in_bytes(&self) -> usize {
        self.size_in_bytes
    }

    #[inline(always)]
    pub fn ops(&self) -> &[BatchOp] {
        &self.ops
    }

    pub fn clear(&mut self) {
        self.ops.clear();
        self.size_in_bytes = 0;
    }
}
