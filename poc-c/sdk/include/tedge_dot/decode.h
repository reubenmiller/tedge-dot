/* tedge-dot C SDK — primitive decode/encode with endianness + word order.
 * Mirrors crates/sdk/src/decode.rs (validated by the same golden vectors).
 */
#ifndef TDOT_DECODE_H
#define TDOT_DECODE_H

#include "model.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Decode wire bytes into a value.
 *
 * The bytes are first normalised to a canonical big-endian buffer: split into
 * 16-bit words, reverse word order when word_order == little, then swap bytes
 * within each word when endianness == little. 64-bit integers outside the JS
 * safe range decode to a string value (value_repr "string").
 *
 * Returns 0 on success, -1 with err filled (wrong length, unsupported type).
 */
int tdot_decode(tdot_datatype_t dt, const uint8_t *bytes, size_t len,
                tdot_order_t endianness, tdot_order_t word_order,
                tdot_value_t *out, char *err, size_t errlen);

/* Encode a value into wire bytes (inverse of tdot_decode, same reordering).
 * *len is in/out: capacity in, bytes written out.
 * Returns 0 on success, -1 with err filled (range, kind mismatch). */
int tdot_encode(tdot_datatype_t dt, const tdot_value_t *value,
                tdot_order_t endianness, tdot_order_t word_order,
                uint8_t *bytes, size_t *len, char *err, size_t errlen);

/* Extract a contiguous bit range (LSB indexing on the canonical big-endian
 * buffer). bit_count of 0 returns the whole value. */
uint64_t tdot_bitfield_extract(const uint8_t *bytes, size_t len,
                               tdot_order_t endianness,
                               tdot_order_t word_order, uint32_t start_bit,
                               uint32_t bit_count);

/* Hex helpers. Grouped: "422a 0000" for group=2. dst must hold
 * len*2 + len/group + 1 bytes. */
void tdot_hex_format(const uint8_t *bytes, size_t len, int group, char *dst,
                     size_t dstlen);
/* Parse hex (spaces allowed). Returns byte count or -1. */
int tdot_hex_parse(const char *hex, uint8_t *dst, size_t dstlen);

#ifdef __cplusplus
}
#endif

#endif /* TDOT_DECODE_H */
