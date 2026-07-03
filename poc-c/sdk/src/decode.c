#include "tedge_dot/decode.h"

#include <inttypes.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Normalise wire bytes to a canonical big-endian buffer: split into 16-bit
 * words, reverse word order if little, swap bytes within words if little.
 * Odd-length buffers (1-byte types) pass through untouched. */
static void reorder(const uint8_t *src, size_t len, tdot_order_t endianness,
                    tdot_order_t word_order, uint8_t *dst) {
    memcpy(dst, src, len);
    if (len < 2 || (len % 2) != 0)
        return;
    size_t words = len / 2;
    if (word_order == TDOT_ORDER_LITTLE) {
        for (size_t i = 0; i < words / 2; i++) {
            uint8_t t0 = dst[i * 2], t1 = dst[i * 2 + 1];
            dst[i * 2] = dst[(words - 1 - i) * 2];
            dst[i * 2 + 1] = dst[(words - 1 - i) * 2 + 1];
            dst[(words - 1 - i) * 2] = t0;
            dst[(words - 1 - i) * 2 + 1] = t1;
        }
    }
    if (endianness == TDOT_ORDER_LITTLE) {
        for (size_t i = 0; i < words; i++) {
            uint8_t t = dst[i * 2];
            dst[i * 2] = dst[i * 2 + 1];
            dst[i * 2 + 1] = t;
        }
    }
}

static uint64_t be_read(const uint8_t *b, size_t len) {
    uint64_t v = 0;
    for (size_t i = 0; i < len; i++)
        v = (v << 8) | b[i];
    return v;
}

static void be_write(uint8_t *b, size_t len, uint64_t v) {
    for (size_t i = 0; i < len; i++)
        b[len - 1 - i] = (uint8_t)(v >> (8 * i));
}

static void num_value(tdot_value_t *out, double num) {
    out->kind = TDOT_VAL_NUM;
    out->num = num;
}

int tdot_decode(tdot_datatype_t dt, const uint8_t *bytes, size_t len,
                tdot_order_t endianness, tdot_order_t word_order,
                tdot_value_t *out, char *err, size_t errlen) {
    memset(out, 0, sizeof *out);

    /* bool: any non-empty buffer; true when any byte is non-zero. */
    if (dt == TDOT_DT_BOOL) {
        if (len == 0) {
            snprintf(err, errlen, "empty buffer for bool");
            return -1;
        }
        out->kind = TDOT_VAL_BOOL;
        out->b = false;
        for (size_t i = 0; i < len; i++)
            if (bytes[i] != 0)
                out->b = true;
        return 0;
    }
    /* string: UTF-8 bytes with trailing NULs trimmed. */
    if (dt == TDOT_DT_STRING) {
        size_t n = len;
        while (n > 0 && bytes[n - 1] == 0)
            n--;
        if (n >= sizeof out->str)
            n = sizeof out->str - 1;
        memcpy(out->str, bytes, n);
        out->str[n] = '\0';
        out->kind = TDOT_VAL_STR;
        return 0;
    }

    size_t want = tdot_datatype_len(dt);
    if (want == 0) {
        snprintf(err, errlen, "unsupported datatype for decode");
        return -1;
    }
    if (len != want) {
        snprintf(err, errlen, "expected %zu bytes for %s, got %zu", want,
                 tdot_datatype_str(dt), len);
        return -1;
    }
    uint8_t canon[8];
    reorder(bytes, len, endianness, word_order, canon);
    uint64_t u = be_read(canon, len);

    switch (dt) {
    case TDOT_DT_BOOL:
        out->kind = TDOT_VAL_BOOL;
        out->b = (u != 0);
        break;
    case TDOT_DT_UINT8:
    case TDOT_DT_UINT16:
    case TDOT_DT_UINT32:
        num_value(out, (double)u);
        break;
    case TDOT_DT_INT8:
        num_value(out, (double)(int8_t)u);
        break;
    case TDOT_DT_INT16:
        num_value(out, (double)(int16_t)u);
        break;
    case TDOT_DT_INT32:
        num_value(out, (double)(int32_t)u);
        break;
    case TDOT_DT_UINT64:
        if (u > (uint64_t)TDOT_JS_SAFE_MAX) {
            out->kind = TDOT_VAL_STR;
            snprintf(out->str, sizeof out->str, "%" PRIu64, u);
        } else {
            num_value(out, (double)u);
        }
        break;
    case TDOT_DT_INT64: {
        int64_t s = (int64_t)u;
        if (s > TDOT_JS_SAFE_MAX || s < -TDOT_JS_SAFE_MAX) {
            out->kind = TDOT_VAL_STR;
            snprintf(out->str, sizeof out->str, "%" PRId64, s);
        } else {
            num_value(out, (double)s);
        }
        break;
    }
    case TDOT_DT_FLOAT32: {
        uint32_t bits = (uint32_t)u;
        float f;
        memcpy(&f, &bits, 4);
        num_value(out, (double)f);
        break;
    }
    case TDOT_DT_FLOAT64: {
        double d;
        memcpy(&d, &u, 8);
        num_value(out, d);
        break;
    }
    default:
        snprintf(err, errlen, "unsupported datatype");
        return -1;
    }
    return 0;
}

int tdot_encode(tdot_datatype_t dt, const tdot_value_t *value,
                tdot_order_t endianness, tdot_order_t word_order,
                uint8_t *bytes, size_t *len, char *err, size_t errlen) {
    if (dt == TDOT_DT_STRING) {
        if (value->kind != TDOT_VAL_STR) {
            snprintf(err, errlen, "expected string value");
            return -1;
        }
        size_t n = strlen(value->str);
        if (n > *len) {
            snprintf(err, errlen, "buffer too small");
            return -1;
        }
        memcpy(bytes, value->str, n);
        *len = n;
        return 0;
    }
    size_t want = tdot_datatype_len(dt);
    if (want == 0) {
        snprintf(err, errlen, "unsupported datatype for encode");
        return -1;
    }
    if (*len < want) {
        snprintf(err, errlen, "buffer too small");
        return -1;
    }

    uint8_t canon[8] = {0};
    switch (dt) {
    case TDOT_DT_BOOL: {
        bool b = (value->kind == TDOT_VAL_BOOL) ? value->b
                 : (value->kind == TDOT_VAL_NUM) ? (value->num != 0.0)
                                                 : false;
        if (value->kind != TDOT_VAL_BOOL && value->kind != TDOT_VAL_NUM) {
            snprintf(err, errlen, "expected boolean value");
            return -1;
        }
        canon[0] = b ? 1 : 0;
        break;
    }
    case TDOT_DT_FLOAT32: {
        if (value->kind != TDOT_VAL_NUM) {
            snprintf(err, errlen, "expected numeric value");
            return -1;
        }
        float f = (float)value->num;
        uint32_t bits;
        memcpy(&bits, &f, 4);
        be_write(canon, 4, bits);
        break;
    }
    case TDOT_DT_FLOAT64: {
        if (value->kind != TDOT_VAL_NUM) {
            snprintf(err, errlen, "expected numeric value");
            return -1;
        }
        uint64_t bits;
        memcpy(&bits, &value->num, 8);
        be_write(canon, 8, bits);
        break;
    }
    default: { /* integer types */
        int64_t s = 0;
        uint64_t u = 0;
        bool is_signed = (dt == TDOT_DT_INT8 || dt == TDOT_DT_INT16 ||
                          dt == TDOT_DT_INT32 || dt == TDOT_DT_INT64);
        if (value->kind == TDOT_VAL_NUM) {
            if (value->num != floor(value->num)) {
                snprintf(err, errlen, "expected integer value");
                return -1;
            }
            if (is_signed)
                s = (int64_t)value->num;
            else {
                if (value->num < 0) {
                    snprintf(err, errlen, "negative value for unsigned type");
                    return -1;
                }
                u = (uint64_t)value->num;
            }
        } else if (value->kind == TDOT_VAL_STR) {
            /* 64-bit values outside the JS safe range arrive as strings. */
            if (is_signed)
                s = strtoll(value->str, NULL, 10);
            else
                u = strtoull(value->str, NULL, 10);
        } else {
            snprintf(err, errlen, "expected numeric value");
            return -1;
        }
        if (is_signed) {
            int64_t min = -(1LL << (want * 8 - 1));
            int64_t max = (1LL << (want * 8 - 1)) - 1;
            if (want < 8 && (s < min || s > max)) {
                snprintf(err, errlen, "value out of range for %s",
                         tdot_datatype_str(dt));
                return -1;
            }
            be_write(canon, want, (uint64_t)s);
        } else {
            if (want < 8 && u > ((1ULL << (want * 8)) - 1)) {
                snprintf(err, errlen, "value out of range for %s",
                         tdot_datatype_str(dt));
                return -1;
            }
            be_write(canon, want, u);
        }
        break;
    }
    }

    /* Apply the inverse reordering (reorder is an involution). */
    reorder(canon, want, endianness, word_order, bytes);
    *len = want;
    return 0;
}

uint64_t tdot_bitfield_extract(const uint8_t *bytes, size_t len,
                               tdot_order_t endianness,
                               tdot_order_t word_order, uint32_t start_bit,
                               uint32_t bit_count) {
    uint8_t canon[8];
    if (len > 8)
        len = 8;
    reorder(bytes, len, endianness, word_order, canon);
    uint64_t v = be_read(canon, len);
    if (bit_count == 0)
        return v; /* zero count means the whole value */
    v >>= start_bit;
    if (bit_count < 64)
        v &= (1ULL << bit_count) - 1;
    return v;
}

void tdot_hex_format(const uint8_t *bytes, size_t len, int group, char *dst,
                     size_t dstlen) {
    if (group <= 0)
        group = 1;
    size_t pos = 0;
    dst[0] = '\0';
    for (size_t i = 0; i < len && pos + 3 < dstlen; i++) {
        if (i > 0 && (i % (size_t)group) == 0 && pos + 1 < dstlen)
            dst[pos++] = ' ';
        pos += (size_t)snprintf(dst + pos, dstlen - pos, "%02x", bytes[i]);
    }
    dst[pos < dstlen ? pos : dstlen - 1] = '\0';
}

static int hex_nibble(char c) {
    if (c >= '0' && c <= '9')
        return c - '0';
    if (c >= 'a' && c <= 'f')
        return c - 'a' + 10;
    if (c >= 'A' && c <= 'F')
        return c - 'A' + 10;
    return -1;
}

int tdot_hex_parse(const char *hex, uint8_t *dst, size_t dstlen) {
    size_t n = 0;
    int hi = -1;
    for (const char *p = hex; *p; p++) {
        if (*p == ' ' || *p == '\t')
            continue;
        int v = hex_nibble(*p);
        if (v < 0)
            return -1;
        if (hi < 0) {
            hi = v;
        } else {
            if (n >= dstlen)
                return -1;
            dst[n++] = (uint8_t)((hi << 4) | v);
            hi = -1;
        }
    }
    if (hi >= 0)
        return -1; /* odd digit count */
    return (int)n;
}
