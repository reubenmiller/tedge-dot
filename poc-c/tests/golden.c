/* Golden-vector conformance runner: validates the C decode/encode helpers
 * against the Rust SDK's vectors (crates/sdk/conformance/vectors.json).
 *
 *   tedge-dot-golden <vectors.json>
 */
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cjson/cJSON.h"
#include "tedge_dot/decode.h"

static char *slurp(const char *path) {
    FILE *fp = fopen(path, "rb");
    if (!fp)
        return NULL;
    fseek(fp, 0, SEEK_END);
    long n = ftell(fp);
    fseek(fp, 0, SEEK_SET);
    char *buf = malloc((size_t)n + 1);
    fread(buf, 1, (size_t)n, fp);
    buf[n] = '\0';
    fclose(fp);
    return buf;
}

static tdot_order_t order_of(const cJSON *v, const char *key) {
    const cJSON *o = cJSON_GetObjectItem(v, key);
    return (cJSON_IsString(o) && strcmp(o->valuestring, "little") == 0)
               ? TDOT_ORDER_LITTLE
               : TDOT_ORDER_BIG;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fputs("usage: tedge-dot-golden <vectors.json>\n", stderr);
        return 2;
    }
    char *text = slurp(argv[1]);
    if (!text) {
        fprintf(stderr, "cannot read %s\n", argv[1]);
        return 2;
    }
    cJSON *doc = cJSON_Parse(text);
    const cJSON *vectors = cJSON_GetObjectItem(doc, "vectors");
    int pass = 0, fail = 0, skip = 0;

    const cJSON *v;
    cJSON_ArrayForEach(v, vectors) {
        const char *id = cJSON_GetObjectItem(v, "id")->valuestring;
        tdot_datatype_t dt = tdot_datatype_parse(
            cJSON_GetObjectItem(v, "datatype")->valuestring);
        const char *hex = cJSON_GetObjectItem(v, "bytes")->valuestring;
        const cJSON *expect = cJSON_GetObjectItem(v, "expect");
        const cJSON *bitfield = cJSON_GetObjectItem(v, "bitfield");
        tdot_order_t e = order_of(v, "endianness");
        tdot_order_t w = order_of(v, "word_order");
        bool expect_error = cJSON_IsTrue(cJSON_GetObjectItem(expect, "error"));

        if (dt == TDOT_DT_NONE &&
            strcmp(cJSON_GetObjectItem(v, "datatype")->valuestring,
                   "bytes") == 0) {
            /* "bytes" datatype has no typed value: decoding must fail. */
            if (expect_error) {
                pass++;
                continue;
            }
            skip++;
            continue;
        }

        uint8_t bytes[64];
        int blen = tdot_hex_parse(hex, bytes, sizeof bytes);
        if (blen < 0) {
            printf("FAIL %-36s (bad hex in vector)\n", id);
            fail++;
            continue;
        }

        char err[160];
        tdot_value_t val;
        bool ok;

        if (bitfield) {
            uint32_t sb = (uint32_t)cJSON_GetObjectItem(bitfield, "start_bit")
                              ->valuedouble;
            uint32_t bc = (uint32_t)cJSON_GetObjectItem(bitfield, "bit_count")
                              ->valuedouble;
            uint64_t got = tdot_bitfield_extract(bytes, (size_t)blen, e, w,
                                                 sb, bc);
            double want =
                cJSON_GetObjectItem(expect, "value")->valuedouble;
            ok = (double)got == want;
            if (!ok)
                printf("FAIL %-36s bitfield got %llu want %g\n", id,
                       (unsigned long long)got, want);
        } else {
            int rc = tdot_decode(dt, bytes, (size_t)blen, e, w, &val, err,
                                 sizeof err);
            if (expect_error) {
                ok = rc != 0;
                if (!ok)
                    printf("FAIL %-36s expected decode error\n", id);
            } else if (rc != 0) {
                ok = false;
                printf("FAIL %-36s decode error: %s\n", id, err);
            } else {
                const cJSON *special =
                    cJSON_GetObjectItem(expect, "special");
                const cJSON *want = cJSON_GetObjectItem(expect, "value");
                if (cJSON_IsString(special)) {
                    const char *s = special->valuestring;
                    ok = val.kind == TDOT_VAL_NUM &&
                         ((strcmp(s, "nan") == 0 && isnan(val.num)) ||
                          (strcmp(s, "+inf") == 0 && isinf(val.num) &&
                           val.num > 0) ||
                          (strcmp(s, "-inf") == 0 && isinf(val.num) &&
                           val.num < 0));
                } else if (cJSON_IsBool(want)) {
                    ok = val.kind == TDOT_VAL_BOOL &&
                         val.b == cJSON_IsTrue(want);
                } else if (cJSON_IsNumber(want)) {
                    ok = val.kind == TDOT_VAL_NUM &&
                         val.num == want->valuedouble;
                } else if (cJSON_IsString(want)) {
                    ok = val.kind == TDOT_VAL_STR &&
                         strcmp(val.str, want->valuestring) == 0;
                } else {
                    ok = false;
                }
                if (!ok)
                    printf("FAIL %-36s value mismatch (kind=%d num=%g "
                           "str=%s)\n",
                           id, val.kind, val.num, val.str);

                /* round-trip: encode(decode(bytes)) == bytes */
                if (ok && cJSON_IsTrue(cJSON_GetObjectItem(v, "roundtrip"))) {
                    uint8_t enc[64];
                    size_t elen = sizeof enc;
                    if (tdot_encode(dt, &val, e, w, enc, &elen, err,
                                    sizeof err) != 0 ||
                        elen != (size_t)blen ||
                        memcmp(enc, bytes, elen) != 0) {
                        ok = false;
                        printf("FAIL %-36s roundtrip mismatch\n", id);
                    }
                }
            }
        }
        if (ok)
            pass++;
        else
            fail++;
    }

    printf("golden vectors: %d passed, %d failed, %d skipped\n", pass, fail,
           skip);
    cJSON_Delete(doc);
    free(text);
    return fail == 0 ? 0 : 1;
}
