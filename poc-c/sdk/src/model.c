#include "tedge_dot/model.h"

#include <math.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

size_t tdot_datatype_len(tdot_datatype_t dt) {
    switch (dt) {
    case TDOT_DT_BOOL:
    case TDOT_DT_INT8:
    case TDOT_DT_UINT8:
        return 1;
    case TDOT_DT_INT16:
    case TDOT_DT_UINT16:
        return 2;
    case TDOT_DT_INT32:
    case TDOT_DT_UINT32:
    case TDOT_DT_FLOAT32:
        return 4;
    case TDOT_DT_INT64:
    case TDOT_DT_UINT64:
    case TDOT_DT_FLOAT64:
        return 8;
    default:
        return 0;
    }
}

static const struct {
    tdot_datatype_t dt;
    const char *name;
} DT_NAMES[] = {
    {TDOT_DT_BOOL, "bool"},       {TDOT_DT_INT8, "int8"},
    {TDOT_DT_UINT8, "uint8"},     {TDOT_DT_INT16, "int16"},
    {TDOT_DT_UINT16, "uint16"},   {TDOT_DT_INT32, "int32"},
    {TDOT_DT_UINT32, "uint32"},   {TDOT_DT_INT64, "int64"},
    {TDOT_DT_UINT64, "uint64"},   {TDOT_DT_FLOAT32, "float32"},
    {TDOT_DT_FLOAT64, "float64"}, {TDOT_DT_STRING, "string"},
};

const char *tdot_datatype_str(tdot_datatype_t dt) {
    for (size_t i = 0; i < sizeof DT_NAMES / sizeof DT_NAMES[0]; i++)
        if (DT_NAMES[i].dt == dt)
            return DT_NAMES[i].name;
    return NULL;
}

tdot_datatype_t tdot_datatype_parse(const char *name) {
    if (!name)
        return TDOT_DT_NONE;
    for (size_t i = 0; i < sizeof DT_NAMES / sizeof DT_NAMES[0]; i++)
        if (strcmp(DT_NAMES[i].name, name) == 0)
            return DT_NAMES[i].dt;
    return TDOT_DT_NONE;
}

void tdot_transform_init(tdot_transform_t *t) {
    t->multiplier = 1.0;
    t->divisor = 1.0;
    t->decimal_shift = 0;
    t->offset = 0.0;
}

double tdot_transform_apply(const tdot_transform_t *t, double value) {
    double divisor = (t->divisor == 0.0) ? 1.0 : t->divisor;
    return (value * t->multiplier * pow(10.0, t->decimal_shift) / divisor) +
           t->offset;
}

const char *tdot_quality_str(tdot_quality_t q) {
    switch (q) {
    case TDOT_Q_GOOD:
        return "good";
    case TDOT_Q_BAD:
        return "bad";
    case TDOT_Q_STALE:
        return "stale";
    }
    return "bad";
}

void tdot_sample_init(tdot_sample_t *s) {
    memset(s, 0, sizeof *s);
    s->quality = TDOT_Q_GOOD;
    s->raw_group = 1;
}

void tdot_sample_bad(tdot_sample_t *s, const char *fmt, ...) {
    s->quality = TDOT_Q_BAD;
    s->value.kind = TDOT_VAL_NONE;
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(s->error, sizeof s->error, fmt, ap);
    va_end(ap);
}
