use crate::text::text_of_ptr_size;
use crate::types::{Bytes, SkewedPtr};

// The meaning of the `mode` parameter is documented in motoko-base, function Float.format()
#[no_mangle]
unsafe extern "C" fn float_fmt(a: f64, prec: u32, mode: u32) -> SkewedPtr {
    // prec and mode are tagged (TODO (osa): what tag???)
    let mode = mode >> 24;
    let prec = core::cmp::min(prec >> 24, 100) as usize;

    // 110 bytes needed for max precision (TODO (osa): why? how?)
    let buf = [0u8; 120];

    // NB. Using snprintf because I think only 0 and 3 are supposed by Rust's built-in formatter
    let fmt = match mode {
        0 => "%.*f\0",
        1 => "%.*e\0",
        2 => "%.*g\0",
        3 => "%.*a\0",
        _ => panic!("float_fmt: unrecognized mode"),
    };

    let n_written = libc::snprintf(
        buf.as_ptr() as *mut _,
        120,
        fmt.as_ptr() as *const _,
        prec,
        a as libc::c_double,
    );

    assert!(n_written > 0);

    text_of_ptr_size(buf.as_ptr(), Bytes(n_written as u32))
}