/* tedge-dot C SDK — core data model (datatypes, values, samples, transforms).
 *
 * Mirrors crates/sdk/src/model.rs of the Rust implementation and the
 * OT Connector Contract (doc/contract/ot-connector-contract.md).
 */
#ifndef TDOT_MODEL_H
#define TDOT_MODEL_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- datatypes ---------------------------------------------------------- */

typedef enum {
    TDOT_DT_NONE = 0, /* raw mode / unset */
    TDOT_DT_BOOL,
    TDOT_DT_INT8,
    TDOT_DT_UINT8,
    TDOT_DT_INT16,
    TDOT_DT_UINT16,
    TDOT_DT_INT32,
    TDOT_DT_UINT32,
    TDOT_DT_INT64,
    TDOT_DT_UINT64,
    TDOT_DT_FLOAT32,
    TDOT_DT_FLOAT64,
    TDOT_DT_STRING,
} tdot_datatype_t;

/* Fixed byte length of a datatype, or 0 for variable-length/none. */
size_t tdot_datatype_len(tdot_datatype_t dt);
const char *tdot_datatype_str(tdot_datatype_t dt);
/* Returns TDOT_DT_NONE when the name is unknown. */
tdot_datatype_t tdot_datatype_parse(const char *name);

/* ---- byte / word order --------------------------------------------------- */

typedef enum {
    TDOT_ORDER_BIG = 0, /* default */
    TDOT_ORDER_LITTLE,
} tdot_order_t;

/* ---- values -------------------------------------------------------------- */

/* JS safe-integer bound: 64-bit values outside ±(2^53 - 1) are emitted as
 * strings (value_repr = "string"), matching the Rust SDK. */
#define TDOT_JS_SAFE_MAX 9007199254740991LL

typedef enum {
    TDOT_VAL_NONE = 0,
    TDOT_VAL_BOOL,
    TDOT_VAL_NUM,
    TDOT_VAL_STR,
} tdot_value_kind_t;

typedef struct {
    tdot_value_kind_t kind;
    bool b;
    double num;
    /* Out-of-safe-range 64-bit ints and short strings. 256 B so J1939 DM1/DM2
     * diagnostic-code lists (up to MAX_DM_FIELD DTCs) fit without truncation. */
    char str[256];
} tdot_value_t;

/* ---- per-point linear transform ------------------------------------------ */

/* out = (value * multiplier * 10^decimal_shift / divisor) + offset
 * Applied to numeric values only; divisor of 0 is treated as 1. */
typedef struct {
    double multiplier; /* default 1.0 */
    double divisor;    /* default 1.0 */
    int decimal_shift; /* default 0 */
    double offset;     /* default 0.0 */
} tdot_transform_t;

void tdot_transform_init(tdot_transform_t *t);
double tdot_transform_apply(const tdot_transform_t *t, double value);

/* ---- samples ------------------------------------------------------------- */

typedef enum {
    TDOT_Q_GOOD = 0,
    TDOT_Q_BAD,
    TDOT_Q_STALE,
} tdot_quality_t;

const char *tdot_quality_str(tdot_quality_t q);

#define TDOT_RAW_MAX 64
#define TDOT_ERR_MAX 160

/* One read result for one point, filled by the connector. The runtime turns
 * it into the JSON sample envelope. */
typedef struct {
    tdot_quality_t quality;
    tdot_value_t value;         /* decoded + transformed; NONE when bad */
    uint8_t raw[TDOT_RAW_MAX];  /* wire bytes as read (pre-reorder) */
    size_t raw_len;
    int raw_group;              /* hex grouping: 2 for modbus registers, 1 else */
    char error[TDOT_ERR_MAX];   /* required when quality == bad */
} tdot_sample_t;

void tdot_sample_init(tdot_sample_t *s);
/* Convenience: mark a sample bad with a printf-style reason. */
void tdot_sample_bad(tdot_sample_t *s, const char *fmt, ...);

#ifdef __cplusplus
}
#endif

#endif /* TDOT_MODEL_H */
