#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>

#include "cjson/cJSON.h"
#include "tedge_dot/decode.h"
#include "tedge_dot/runtime.h"

double tdot_mono(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

double tdot_now_ms(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return (double)tv.tv_sec * 1000.0 + (double)tv.tv_usec / 1000.0;
}

void tdot_now_rfc3339(char *dst, size_t dstlen) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    struct tm tm;
    gmtime_r(&tv.tv_sec, &tm);
    size_t n = strftime(dst, dstlen, "%Y-%m-%dT%H:%M:%S", &tm);
    snprintf(dst + n, dstlen - n, ".%03dZ", (int)(tv.tv_usec / 1000));
}

static void add_value(cJSON *obj, const tdot_value_t *v) {
    switch (v->kind) {
    case TDOT_VAL_BOOL:
        cJSON_AddBoolToObject(obj, "value", v->b);
        cJSON_AddStringToObject(obj, "value_repr", "boolean");
        break;
    case TDOT_VAL_NUM:
        cJSON_AddNumberToObject(obj, "value", v->num);
        cJSON_AddStringToObject(obj, "value_repr", "number");
        break;
    case TDOT_VAL_STR:
        cJSON_AddStringToObject(obj, "value", v->str);
        cJSON_AddStringToObject(obj, "value_repr", "string");
        break;
    default:
        break;
    }
}

char *tdot_envelope_sample(const tdot_config_t *cfg, const tdot_device_t *dev,
                           const tdot_point_t *pt, const tdot_sample_t *s) {
    cJSON *obj = cJSON_CreateObject();
    char ts[40];
    tdot_now_rfc3339(ts, sizeof ts);
    cJSON_AddStringToObject(obj, "ts", ts);
    cJSON_AddNumberToObject(obj, "ts_ms", tdot_now_ms());
    cJSON_AddStringToObject(obj, "device", dev->name);
    cJSON_AddStringToObject(obj, "protocol", cfg->protocol);
    cJSON_AddStringToObject(obj, "point", pt->id);
    cJSON_AddStringToObject(obj, "mode", "typed");
    if (pt->datatype != TDOT_DT_NONE)
        cJSON_AddStringToObject(obj, "datatype",
                                tdot_datatype_str(pt->datatype));
    if (s->quality != TDOT_Q_BAD)
        add_value(obj, &s->value);

    char hex[TDOT_RAW_MAX * 3 + 1];
    tdot_hex_format(s->raw, s->raw_len, s->raw_group, hex, sizeof hex);
    cJSON_AddStringToObject(obj, "raw", hex);
    cJSON_AddStringToObject(obj, "quality", tdot_quality_str(s->quality));
    if (pt->unit)
        cJSON_AddStringToObject(obj, "unit", pt->unit);
    if (pt->addr_json) {
        cJSON *addr = cJSON_Parse(pt->addr_json);
        if (addr)
            cJSON_AddItemToObject(obj, "addr", addr);
    }
    cJSON_AddNumberToObject(obj, "seq", (double)pt->seq);
    if (s->quality == TDOT_Q_BAD)
        cJSON_AddStringToObject(obj, "error", s->error);
    if (pt->meta_json) {
        cJSON *meta = cJSON_Parse(pt->meta_json);
        if (meta)
            cJSON_AddItemToObject(obj, "meta", meta);
    }
    char *out = cJSON_PrintUnformatted(obj);
    cJSON_Delete(obj);
    return out;
}
