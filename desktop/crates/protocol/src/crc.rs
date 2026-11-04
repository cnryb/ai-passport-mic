//! CRC-16/CCITT-FALSE：poly 0x1021、init 0xFFFF、不反转、xorout 0。
//!
//! 校验向量：`"123456789"` → `0x29B1`。固件侧 `device/main/protocol.c` 的实现相同，
//! 两端由 `tests/c_vectors.rs` 用同一份向量文件对拍。

pub fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for byte in data {
        crc ^= (*byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        assert_eq!(crc16_ccitt(b"123456789"), 0x29B1);
    }

    #[test]
    fn empty_is_init_value() {
        // init 就是 0xFFFF：空输入不做任何迭代。
        assert_eq!(crc16_ccitt(&[]), 0xFFFF);
    }
}
