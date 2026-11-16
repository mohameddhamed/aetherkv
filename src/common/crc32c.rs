//! CRC32C (Castagnoli, polynomial 0x1EDC6F41 / reflected 0x82F63B78)
//! Supports SSE4.2 hardware acceleration on x86_64, ARMv8 CRC32 on AArch64,
//! and an 8-way sliced lookup table fallback.

pub const CRC32C_INIT: u32 = 0xFFFF_FFFF;

/// Precomputed Castagnoli CRC32C table (8 slices of 256 words each for slice-by-8).
const CRC32C_TABLE: [[u32; 256]; 8] = {
    let mut table = [[0u32; 256]; 8];
    let poly = 0x82F6_3B78u32;

    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = (crc >> 1) ^ ((crc & 1) * poly);
            j += 1;
        }
        table[0][i] = crc;
        i += 1;
    }

    let mut slice = 1;
    while slice < 8 {
        let mut i = 0;
        while i < 256 {
            table[slice][i] =
                (table[slice - 1][i] >> 8) ^ table[0][(table[slice - 1][i] & 0xFF) as usize];
            i += 1;
        }
        slice += 1;
    }
    table
};

#[inline(always)]
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_extend(CRC32C_INIT, data) ^ 0xFFFF_FFFF
}

#[inline(always)]
pub fn crc32c_extend(crc: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("sse4.2") {
            return unsafe { crc32c_sse42(crc, data) };
        }
    }

    #[cfg(all(target_arch = "aarch64", target_feature = "crc"))]
    {
        return unsafe { crc32c_aarch64(crc, data) };
    }

    #[allow(unreachable_code)]
    crc32c_slice8(crc, data)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_sse42(mut crc: u32, mut data: &[u8]) -> u32 {
    use std::arch::x86_64::*;

    let mut crc64 = crc as u64;

    while data.len() >= 8 {
        let chunk = u64::from_le_bytes(data[..8].try_into().unwrap());
        crc64 = _mm_crc32_u64(crc64, chunk);
        data = &data[8..];
    }

    if data.len() >= 4 {
        let chunk = u32::from_le_bytes(data[..4].try_into().unwrap());
        crc64 = _mm_crc32_u32(crc64 as u32, chunk) as u64;
        data = &data[4..];
    }

    if data.len() >= 2 {
        let chunk = u16::from_le_bytes(data[..2].try_into().unwrap());
        crc64 = _mm_crc32_u16(crc64 as u32, chunk) as u64;
        data = &data[2..];
    }

    if data.len() == 1 {
        crc64 = _mm_crc32_u8(crc64 as u32, data[0]) as u64;
    }

    crc64 as u32
}

#[cfg(all(target_arch = "aarch64", target_feature = "crc"))]
#[target_feature(enable = "crc")]
unsafe fn crc32c_aarch64(mut crc: u32, mut data: &[u8]) -> u32 {
    use std::arch::aarch64::*;

    while data.len() >= 8 {
        let chunk = u64::from_le_bytes(data[..8].try_into().unwrap());
        crc = __crc32cd(crc, chunk);
        data = &data[8..];
    }

    if data.len() >= 4 {
        let chunk = u32::from_le_bytes(data[..4].try_into().unwrap());
        crc = __crc32cw(crc, chunk);
        data = &data[4..];
    }

    if data.len() >= 2 {
        let chunk = u16::from_le_bytes(data[..2].try_into().unwrap());
        crc = __crc32ch(crc, chunk);
        data = &data[2..];
    }

    if data.len() == 1 {
        crc = __crc32cb(crc, data[0]);
    }

    crc
}

/// High-performance 8-way sliced lookup table implementation.
pub fn crc32c_slice8(mut crc: u32, mut data: &[u8]) -> u32 {
    while data.len() >= 8 {
        let one = data[0] as usize ^ (crc as usize & 0xFF);
        let two = data[1] as usize ^ ((crc as usize >> 8) & 0xFF);
        let three = data[2] as usize ^ ((crc as usize >> 16) & 0xFF);
        let four = data[3] as usize ^ ((crc as usize >> 24) & 0xFF);

        crc = CRC32C_TABLE[7][one]
            ^ CRC32C_TABLE[6][two]
            ^ CRC32C_TABLE[5][three]
            ^ CRC32C_TABLE[4][four]
            ^ CRC32C_TABLE[3][data[4] as usize]
            ^ CRC32C_TABLE[2][data[5] as usize]
            ^ CRC32C_TABLE[1][data[6] as usize]
            ^ CRC32C_TABLE[0][data[7] as usize];

        data = &data[8..];
    }

    for &byte in data {
        crc = (crc >> 8) ^ CRC32C_TABLE[0][((crc ^ (byte as u32)) & 0xFF) as usize];
    }

    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_standard_crc32c_test_vector() {
        // Standard Castagnoli CRC32C for ASCII "123456789" is 0xE3069283
        let input = b"123456789";
        let checksum = crc32c(input);
        assert_eq!(
            checksum, 0xE3069283,
            "Expected standard CRC32C vector 0xE3069283, got 0x{:08X}",
            checksum
        );
    }

    #[test]
    fn test_slice8_matches_incremental() {
        let payload = b"AetherKV high throughput lock-free LSM storage engine in Rust";
        let full = crc32c(payload);

        let mut running = CRC32C_INIT;
        for chunk in payload.chunks(7) {
            running = crc32c_extend(running, chunk);
        }
        let incremental = running ^ 0xFFFF_FFFF;

        assert_eq!(full, incremental);
    }
}
