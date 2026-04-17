//! Sample Rust translation for complexity analysis.

const MAX_SIZE: usize = 256;

pub fn parse_header(buf: &[u8]) -> Result<i32, i32> {
    if buf.is_empty() {
        return Err(-1);
    }
    let mut total: i32 = 0;
    for &b in buf {
        if b == b'\n' {
            continue;
        }
        if b.is_ascii_digit() {
            total = total * 10 + (b - b'0') as i32;
        } else {
            eprintln!("parse error at {}", total);
            return Err(-2);
        }
    }
    Ok(total)
}

/// compute a checksum
fn crc32_small(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &d in data {
        crc ^= d as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

pub fn dispatch(op: i32, x: i32) -> i32 {
    match op {
        1 => x + 1,
        2 => x * 2,
        3 => x - 1,
        _ => 0,
    }
}

#[no_mangle]
pub extern "C" fn ffi_entry(v: i32) -> i32 {
    v + 1
}

#[link_name = "ph_legacy"]
extern "C" {
    fn dummy_imported();
}

#[link_name = "old_name"]
pub extern "C" fn renamed_fn(x: i32) -> i32 {
    x * 2
}
