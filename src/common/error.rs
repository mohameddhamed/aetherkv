use std::fmt;
use std::io;

#[derive(Debug)]
pub enum AetherError {
    Io(io::Error),
    Corruption(String),
    WalExhausted,
    MemTableFull,
    InvalidMagicNumber,
    BlockCrcMismatch { expected: u32, actual: u32 },
    FrameCrcMismatch { expected: u32, actual: u32 },
    InvalidRecordType(u8),
    KeyNotFound,
    CompactionError(String),
}

impl fmt::Display for AetherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {}", err),
            Self::Corruption(msg) => write!(f, "Data corruption: {}", msg),
            Self::WalExhausted => write!(f, "WAL segment exhausted"),
            Self::MemTableFull => write!(f, "MemTable arena capacity exceeded"),
            Self::InvalidMagicNumber => write!(f, "Invalid SSTable magic number"),
            Self::BlockCrcMismatch { expected, actual } => {
                write!(f, "Block CRC mismatch: expected 0x{:08X}, got 0x{:08X}", expected, actual)
            }
            Self::FrameCrcMismatch { expected, actual } => {
                write!(f, "WAL Frame CRC mismatch: expected 0x{:08X}, got 0x{:08X}", expected, actual)
            }
            Self::InvalidRecordType(t) => write!(f, "Invalid record type: {}", t),
            Self::KeyNotFound => write!(f, "Key not found"),
            Self::CompactionError(msg) => write!(f, "Compaction failure: {}", msg),
        }
    }
}

impl std::error::Error for AetherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for AetherError {
    #[inline]
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

pub type Result<T> = std::result::Result<T, AetherError>;
