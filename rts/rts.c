#include "rts.h"

export as_ptr alloc_blob(size_t n) {
  as_ptr r = alloc_bytes (BLOB_HEADER_SIZE*sizeof(void*) + n);
  TAG(r) = TAG_BLOB;
  BLOB_LEN(r) = n;
  return r;
}

char *alloc(size_t n) {
  as_ptr r = alloc_blob(n);
  return BLOB_PAYLOAD(r);
}

export as_ptr alloc_array(uint32_t len) {
  // Array payload should not be larger than half of the memory.
  if (len > 1 << (32 - 2 - 1)) { // 2 for word size, 1 to divide by two
    rts_trap_with("Array allocation too large");
  }

  as_ptr a = alloc_words(ARRAY_HEADER_SIZE + len);
  TAG(a) = TAG_ARRAY;
  ARRAY_LEN(a) = len;
  return a;
}

void __attribute__ ((noreturn)) trap_with_prefix(const char* prefix, const char *str) {
  int len1 = strlen(prefix);
  int len2 = strlen(str);
  char msg[len1 + len2];
  memcpy(msg, prefix, len1);
  memcpy(msg + len1, str, len2);
  rts_trap(msg, len1 + len2);
}

void __attribute__ ((noreturn)) idl_trap_with(const char *str) {
  trap_with_prefix("IDL error: ", str);
}

void __attribute__ ((noreturn)) rts_trap_with(const char *str) {
  trap_with_prefix("RTS error: ", str);
}

// This is mostly to test static strings and access to the AS heap
const char* RTS_VERSION = "0.1";

// This is mostly to test function pointers
as_ptr get_version() { return text_of_cstr(RTS_VERSION); }
as_ptr (*version_getter)() = &get_version;

export as_ptr version() { return (*version_getter)(); }

/* (S)LEB128 Encoding of words */

export void leb128_encode(uint32_t n, unsigned char *buf) {
  while (true) {
    buf[0] = (unsigned char)n; // get low bits
    if (n >>= 7) {
      // more bytes to come, set high bit and continue
      buf[0] |= 1<<7;
      buf++;
    } else {
      // we are done. high bit should be cleared anyway
      return;
    }
  }
}

export void sleb128_encode(int32_t n, unsigned char *buf) {
  while (true) {
    *buf = n & 0x7F; // get low bits
    if (n >= -64 && n < 64) {
      // last byte written, high bit is clear
      return;
    } else {
      // more bytes to come, set high bit and continue
      *buf++ |= 0x80;
      n >>= 7;
    }
  }
}