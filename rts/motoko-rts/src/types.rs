use core::ops::{Add, AddAssign};

pub fn size_of<T>() -> Words<u32> {
    Bytes(::core::mem::size_of::<T>() as u32).to_words()
}

pub const WORD_SIZE: u32 = 4;

/// The unit "words": `Words(123u32)` means 123 words.
#[repr(C)]
#[derive(PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub struct Words<A>(pub A);

impl Words<u32> {
    pub fn to_bytes(self) -> Bytes<u32> {
        Bytes(self.0 * WORD_SIZE)
    }
}

impl<A: Add<Output = A>> Add for Words<A> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Words(self.0 + rhs.0)
    }
}

impl<A: AddAssign> AddAssign for Words<A> {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl From<Bytes<u32>> for Words<u32> {
    fn from(bytes: Bytes<u32>) -> Words<u32> {
        bytes.to_words()
    }
}

/// The unit "bytes": `Bytes(123u32)` means 123 bytes.
#[repr(C)]
#[derive(PartialEq, Eq, Clone, Copy, PartialOrd, Ord)]
pub struct Bytes<A>(pub A);

impl Bytes<u32> {
    // Rounds up
    pub fn to_words(self) -> Words<u32> {
        // Rust issue for adding ceiling_div: https://github.com/rust-lang/rfcs/issues/2844
        Words((self.0 + WORD_SIZE - 1) / WORD_SIZE)
    }
}

impl<A: Add<Output = A>> Add for Bytes<A> {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Bytes(self.0 + rhs.0)
    }
}

impl<A: AddAssign> AddAssign for Bytes<A> {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl From<Words<u32>> for Bytes<u32> {
    fn from(words: Words<u32>) -> Bytes<u32> {
        words.to_bytes()
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SkewedPtr(pub usize);

impl SkewedPtr {
    pub fn unskew(self) -> usize {
        self.0.wrapping_add(1)
    }
}

pub fn skew(ptr: usize) -> SkewedPtr {
    SkewedPtr(ptr.wrapping_sub(1))
}

// NOTE: We don't create an enum for tags as we can never assume to do exhaustive pattern match on
// tags, because of heap corruptions and other bugs (in the code generator or RTS, or maybe because
// of an unsafe API usage).
pub type Tag = u32;

pub const TAG_OBJECT: Tag = 1;
pub const TAG_OBJ_IND: Tag = 2;
pub const TAG_ARRAY: Tag = 3;
pub const TAG_BITS64: Tag = 5;
pub const TAG_MUTBOX: Tag = 6;
pub const TAG_CLOSURE: Tag = 7;
pub const TAG_SOME: Tag = 8;
pub const TAG_VARIANT: Tag = 9;
pub const TAG_BLOB: Tag = 10;
pub const TAG_FWD_PTR: Tag = 11;
pub const TAG_BITS32: Tag = 12;
pub const TAG_BIGINT: Tag = 13;
pub const TAG_CONCAT: Tag = 14;

// Common parts of any object. Other object pointers can be coerced into a pointer to this.
#[repr(C)]
pub struct Obj {
    pub tag: Tag,
}

#[repr(C)]
#[rustfmt::skip]
pub struct Array {
    pub header: Obj,
    pub len: u32, // number of elements

    // Array elements follow, each u32 sized. We can't have variable-sized structs in Rust so we
    // can't add a field here for the elements.
    // https://doc.rust-lang.org/nomicon/exotic-sizes.html
}

impl Array {
    pub unsafe fn payload_addr(self: *const Self) -> *const SkewedPtr {
        self.offset(1) as *const SkewedPtr // skip array header
    }

    pub unsafe fn get(self: *const Self, idx: u32) -> SkewedPtr {
        let slot_addr = self.payload_addr() as usize + (idx * WORD_SIZE) as usize;
        *(slot_addr as *const SkewedPtr)
    }
}

#[repr(C)]
pub struct Object {
    pub header: Obj,
    pub size: u32,
    pub hash_ptr: u32, // Pointer to static information about object field labels. Not important
                       // for GC (does not contain pointers).
}

impl Object {
    pub unsafe fn payload_addr(self: *const Self) -> *const SkewedPtr {
        self.offset(1) as *const SkewedPtr // skip object header
    }
}

#[repr(C)]
pub struct ObjInd {
    pub header: Obj,
    pub field: SkewedPtr,
}

#[repr(C)]
pub struct Closure {
    pub header: Obj,
    pub funid: u32,
    pub size: u32, // number of elements
                   // other stuff follows ...
}

impl Closure {
    pub unsafe fn payload_addr(self: *const Self) -> *const SkewedPtr {
        self.offset(1) as *const SkewedPtr // skip closure header
    }
}

#[repr(C)]
pub struct Blob {
    pub header: Obj,
    pub len: Bytes<u32>,
    // data follows ..
}

/// A forwarding pointer placed by the GC in place of an evacuated object.
#[repr(C)]
pub struct FwdPtr {
    pub header: Obj,
    pub fwd: SkewedPtr,
}

#[repr(C)]
pub struct BigInt {
    pub header: Obj,
    // the data following now must describe the `mp_int` struct
    // (https://github.com/libtom/libtommath/blob/44ee82cd34d0524c171ffd0da70f83bba919aa38/tommath.h#L174-L179)
    pub size: u32,
    pub alloc: u32,
    pub sign: u32,
    // Unskewed pointer to a blob payload. data_ptr - 2 (words) gives us the blob header.
    pub data_ptr: usize,
}

#[repr(C)]
pub struct MutBox {
    pub header: Obj,
    pub field: SkewedPtr,
}

#[repr(C)]
pub struct Some {
    pub header: Obj,
    pub field: SkewedPtr,
}

#[repr(C)]
pub struct Variant {
    pub header: Obj,
    pub tag: u32,
    pub field: SkewedPtr,
}

#[repr(C)]
pub struct Concat {
    pub header: Obj,
    pub n_bytes: u32,
    pub text1: SkewedPtr,
    pub text2: SkewedPtr,
}