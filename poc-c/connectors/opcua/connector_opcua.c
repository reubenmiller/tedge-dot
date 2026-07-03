/* tedge-dot C PoC — OPC UA connector on open62541 (MPL-2.0).
 * Mirrors crates/connector-opcua: client sessions per device, node-id
 * addressed points ("ns=2;s=Temperature"), typed reads/writes, quality "bad"
 * on Bad status codes. Polling only (the Rust connector also supports
 * monitored-item push; out of PoC scope).
 */
#include <open62541/client.h>
#include <open62541/client_config_default.h>
#include <open62541/client_highlevel.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cjson/cJSON.h"
#include "tedge_dot/connector.h"
#include "tedge_dot/decode.h"

/* Per-point parsed address (pt->proto, flat). */
typedef struct {
    char node_id[160]; /* textual "ns=2;s=Temperature" */
} ua_point_t;

/* Per-device state (dev->proto, flat; client freed on disconnect). */
typedef struct {
    char endpoint[256];
    UA_Client *client; /* NULL when disconnected */
} ua_device_t;

typedef struct {
    char application_name[128];
    char application_uri[128];
    int connect_timeout_s;
    int request_timeout_s;
} ua_state_t;

static const char CAPABILITIES[] =
    "{\"protocol\":\"opcua\",\"version\":\"0.1.0-poc\","
    "\"modes\":[\"typed\"],"
    "\"datatypes\":[\"bool\",\"int8\",\"uint8\",\"int16\",\"uint16\","
    "\"int32\",\"uint32\",\"int64\",\"uint64\",\"float32\",\"float64\","
    "\"string\"],"
    "\"point_kinds\":[\"node\"],"
    "\"command_verbs\":[\"write\"],"
    "\"features\":[\"polling\"],\"subscribe\":false}";

static int configure(tdot_connector_t *self, tdot_config_t *cfg, char *err,
                     size_t errlen) {
    ua_state_t *st = self->state;
    snprintf(st->application_name, sizeof st->application_name, "tedge-dot");
    snprintf(st->application_uri, sizeof st->application_uri, "urn:tedge-dot");
    st->connect_timeout_s = 15;
    st->request_timeout_s = 5;
    if (cfg->connection) {
        toml_datum_t d;
        if ((d = toml_string_in(cfg->connection, "application_name")).ok) {
            snprintf(st->application_name, sizeof st->application_name, "%s",
                     d.u.s);
            free(d.u.s);
        }
        if ((d = toml_string_in(cfg->connection, "application_uri")).ok) {
            snprintf(st->application_uri, sizeof st->application_uri, "%s",
                     d.u.s);
            free(d.u.s);
        }
        if ((d = toml_int_in(cfg->connection, "connect_timeout_s")).ok)
            st->connect_timeout_s = (int)d.u.i;
        if ((d = toml_int_in(cfg->connection, "request_timeout_s")).ok)
            st->request_timeout_s = (int)d.u.i;
        /* security_policy / security_mode: PoC supports "None"/"none" only */
    }

    for (size_t i = 0; i < cfg->ndevices; i++) {
        tdot_device_t *dev = &cfg->devices[i];
        ua_device_t *ua = calloc(1, sizeof *ua);
        dev->proto = ua;

        toml_datum_t d = toml_string_in(dev->protocol_address, "endpoint");
        if (!d.ok) {
            snprintf(err, errlen, "device %s: protocol_address requires "
                                  "endpoint",
                     dev->name);
            return -1;
        }
        snprintf(ua->endpoint, sizeof ua->endpoint, "%s", d.u.s);
        free(d.u.s);

        d = toml_string_in(dev->protocol_address, "security_policy");
        if (d.ok) {
            if (strcmp(d.u.s, "None") != 0) {
                snprintf(err, errlen,
                         "device %s: PoC supports security_policy \"None\" "
                         "only (got %s)",
                         dev->name, d.u.s);
                free(d.u.s);
                return -1;
            }
            free(d.u.s);
        }

        for (size_t j = 0; j < dev->npoints; j++) {
            tdot_point_t *pt = &dev->points[j];
            toml_datum_t nd = toml_string_in(pt->address, "node_id");
            if (!nd.ok) {
                snprintf(err, errlen,
                         "point %s/%s: address requires node_id", dev->name,
                         pt->id);
                return -1;
            }
            ua_point_t *up = calloc(1, sizeof *up);
            pt->proto = up;
            snprintf(up->node_id, sizeof up->node_id, "%s", nd.u.s);
            free(nd.u.s);

            cJSON *addr = cJSON_CreateObject();
            cJSON_AddStringToObject(addr, "node_id", up->node_id);
            pt->addr_json = cJSON_PrintUnformatted(addr);
            cJSON_Delete(addr);
        }
    }
    return 0;
}

static void disconnect_device(tdot_connector_t *self, tdot_device_t *dev) {
    (void)self;
    ua_device_t *ua = dev->proto;
    if (ua && ua->client) {
        UA_Client_disconnect(ua->client);
        UA_Client_delete(ua->client);
        ua->client = NULL;
    }
}

static int connect_device(tdot_connector_t *self, tdot_device_t *dev,
                          char *err, size_t errlen) {
    ua_state_t *st = self->state;
    ua_device_t *ua = dev->proto;
    disconnect_device(self, dev);

    ua->client = UA_Client_new();
    UA_ClientConfig *cc = UA_Client_getConfig(ua->client);
    UA_ClientConfig_setDefault(cc);
    cc->timeout = (UA_UInt32)st->request_timeout_s * 1000;
    UA_LocaleId locale = UA_STRING_ALLOC("en");
    UA_String name = UA_STRING_ALLOC(st->application_name);
    UA_String uri = UA_STRING_ALLOC(st->application_uri);
    UA_LocalizedText_clear(&cc->clientDescription.applicationName);
    cc->clientDescription.applicationName.locale = locale;
    cc->clientDescription.applicationName.text = name;
    UA_String_clear(&cc->clientDescription.applicationUri);
    cc->clientDescription.applicationUri = uri;
    /* keep the client quiet unless debugging */
    cc->logging->log = NULL;

    UA_StatusCode rc = UA_Client_connect(ua->client, ua->endpoint);
    if (rc != UA_STATUSCODE_GOOD) {
        snprintf(err, errlen, "connect %s: %s", ua->endpoint,
                 UA_StatusCode_name(rc));
        UA_Client_delete(ua->client);
        ua->client = NULL;
        return -1;
    }
    return 0;
}

/* Serialize a UA variant scalar to canonical big-endian bytes + value,
 * honouring the point's configured datatype for the envelope. */
static int variant_to_sample(const UA_Variant *v, tdot_point_t *pt,
                             tdot_sample_t *out) {
    uint64_t bits = 0;
    size_t len = 0;
    tdot_value_t raw_val = {0};

    if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_BOOLEAN])) {
        bool b = *(UA_Boolean *)v->data;
        raw_val.kind = TDOT_VAL_BOOL;
        raw_val.b = b;
        bits = b ? 1 : 0;
        len = 1;
    } else if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_SBYTE])) {
        int8_t x = *(UA_SByte *)v->data;
        raw_val.kind = TDOT_VAL_NUM;
        raw_val.num = x;
        bits = (uint8_t)x;
        len = 1;
    } else if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_BYTE])) {
        uint8_t x = *(UA_Byte *)v->data;
        raw_val.kind = TDOT_VAL_NUM;
        raw_val.num = x;
        bits = x;
        len = 1;
    } else if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_INT16])) {
        int16_t x = *(UA_Int16 *)v->data;
        raw_val.kind = TDOT_VAL_NUM;
        raw_val.num = x;
        bits = (uint16_t)x;
        len = 2;
    } else if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_UINT16])) {
        uint16_t x = *(UA_UInt16 *)v->data;
        raw_val.kind = TDOT_VAL_NUM;
        raw_val.num = x;
        bits = x;
        len = 2;
    } else if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_INT32])) {
        int32_t x = *(UA_Int32 *)v->data;
        raw_val.kind = TDOT_VAL_NUM;
        raw_val.num = x;
        bits = (uint32_t)x;
        len = 4;
    } else if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_UINT32])) {
        uint32_t x = *(UA_UInt32 *)v->data;
        raw_val.kind = TDOT_VAL_NUM;
        raw_val.num = x;
        bits = x;
        len = 4;
    } else if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_INT64])) {
        int64_t x = *(UA_Int64 *)v->data;
        if (x > TDOT_JS_SAFE_MAX || x < -TDOT_JS_SAFE_MAX) {
            raw_val.kind = TDOT_VAL_STR;
            snprintf(raw_val.str, sizeof raw_val.str, "%lld", (long long)x);
        } else {
            raw_val.kind = TDOT_VAL_NUM;
            raw_val.num = (double)x;
        }
        bits = (uint64_t)x;
        len = 8;
    } else if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_UINT64])) {
        uint64_t x = *(UA_UInt64 *)v->data;
        if (x > (uint64_t)TDOT_JS_SAFE_MAX) {
            raw_val.kind = TDOT_VAL_STR;
            snprintf(raw_val.str, sizeof raw_val.str, "%llu",
                     (unsigned long long)x);
        } else {
            raw_val.kind = TDOT_VAL_NUM;
            raw_val.num = (double)x;
        }
        bits = x;
        len = 8;
    } else if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_FLOAT])) {
        float f = *(UA_Float *)v->data;
        raw_val.kind = TDOT_VAL_NUM;
        raw_val.num = (double)f;
        uint32_t b32;
        memcpy(&b32, &f, 4);
        bits = b32;
        len = 4;
    } else if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_DOUBLE])) {
        double d = *(UA_Double *)v->data;
        raw_val.kind = TDOT_VAL_NUM;
        raw_val.num = d;
        memcpy(&bits, &d, 8);
        len = 8;
    } else if (UA_Variant_hasScalarType(v, &UA_TYPES[UA_TYPES_STRING])) {
        UA_String *s = (UA_String *)v->data;
        raw_val.kind = TDOT_VAL_STR;
        size_t n = s->length < sizeof raw_val.str - 1 ? s->length
                                                      : sizeof raw_val.str - 1;
        memcpy(raw_val.str, s->data, n);
        raw_val.str[n] = '\0';
        size_t rn = s->length < TDOT_RAW_MAX ? s->length : TDOT_RAW_MAX;
        memcpy(out->raw, s->data, rn);
        out->raw_len = rn;
        out->raw_group = 1;
        out->value = raw_val;
        return 0;
    } else {
        tdot_sample_bad(out, "unsupported OPC-UA value type");
        return 0;
    }

    /* big-endian raw echo */
    for (size_t i = 0; i < len; i++)
        out->raw[i] = (uint8_t)(bits >> (8 * (len - 1 - i)));
    out->raw_len = len;
    out->raw_group = 1;
    out->value = raw_val;

    if (out->value.kind == TDOT_VAL_NUM && pt->has_transform)
        out->value.num = tdot_transform_apply(&pt->transform, out->value.num);
    return 0;
}

static int read_point(tdot_connector_t *self, tdot_device_t *dev,
                      tdot_point_t *pt, tdot_sample_t *out) {
    (void)self;
    ua_device_t *ua = dev->proto;
    ua_point_t *up = pt->proto;

    if (!ua->client) {
        tdot_sample_bad(out, "device not connected");
        return -1;
    }

    UA_NodeId node;
    if (UA_NodeId_parse(&node, UA_STRING(up->node_id)) !=
        UA_STATUSCODE_GOOD) {
        tdot_sample_bad(out, "invalid node id: %s", up->node_id);
        return 0;
    }

    UA_Variant value;
    UA_Variant_init(&value);
    UA_StatusCode rc =
        UA_Client_readValueAttribute(ua->client, node, &value);
    UA_NodeId_clear(&node);

    if (rc != UA_STATUSCODE_GOOD) {
        tdot_sample_bad(out, "bad status: %s", UA_StatusCode_name(rc));
        /* server answered -> transport healthy; connection loss -> down */
        bool transport_down =
            rc == UA_STATUSCODE_BADCONNECTIONCLOSED ||
            rc == UA_STATUSCODE_BADSERVERNOTCONNECTED ||
            rc == UA_STATUSCODE_BADDISCONNECT ||
            rc == UA_STATUSCODE_BADTIMEOUT ||
            rc == UA_STATUSCODE_BADSESSIONIDINVALID ||
            rc == UA_STATUSCODE_BADSECURECHANNELCLOSED ||
            rc == UA_STATUSCODE_BADINTERNALERROR;
        return transport_down ? -1 : 0;
    }

    int r = variant_to_sample(&value, pt, out);
    UA_Variant_clear(&value);
    return r;
}

static int write_point(tdot_connector_t *self, tdot_device_t *dev,
                       tdot_point_t *pt, const tdot_value_t *value, char *err,
                       size_t errlen) {
    (void)self;
    ua_device_t *ua = dev->proto;
    ua_point_t *up = pt->proto;

    if (!ua->client) {
        snprintf(err, errlen, "device not connected");
        return -1;
    }

    UA_Variant v;
    UA_Variant_init(&v);
    UA_Boolean vb;
    UA_SByte vi8;
    UA_Byte vu8;
    UA_Int16 vi16;
    UA_UInt16 vu16;
    UA_Int32 vi32;
    UA_UInt32 vu32;
    UA_Int64 vi64;
    UA_UInt64 vu64;
    UA_Float vf;
    UA_Double vd;
    UA_String vs;

    double num = value->kind == TDOT_VAL_NUM ? value->num
                 : value->kind == TDOT_VAL_STR ? strtod(value->str, NULL)
                                               : 0;
    switch (pt->datatype) {
    case TDOT_DT_BOOL:
        vb = value->kind == TDOT_VAL_BOOL ? value->b : (num != 0);
        UA_Variant_setScalar(&v, &vb, &UA_TYPES[UA_TYPES_BOOLEAN]);
        break;
    case TDOT_DT_INT8:
        vi8 = (UA_SByte)num;
        UA_Variant_setScalar(&v, &vi8, &UA_TYPES[UA_TYPES_SBYTE]);
        break;
    case TDOT_DT_UINT8:
        vu8 = (UA_Byte)num;
        UA_Variant_setScalar(&v, &vu8, &UA_TYPES[UA_TYPES_BYTE]);
        break;
    case TDOT_DT_INT16:
        vi16 = (UA_Int16)num;
        UA_Variant_setScalar(&v, &vi16, &UA_TYPES[UA_TYPES_INT16]);
        break;
    case TDOT_DT_UINT16:
        vu16 = (UA_UInt16)num;
        UA_Variant_setScalar(&v, &vu16, &UA_TYPES[UA_TYPES_UINT16]);
        break;
    case TDOT_DT_INT32:
        vi32 = (UA_Int32)num;
        UA_Variant_setScalar(&v, &vi32, &UA_TYPES[UA_TYPES_INT32]);
        break;
    case TDOT_DT_UINT32:
        vu32 = (UA_UInt32)num;
        UA_Variant_setScalar(&v, &vu32, &UA_TYPES[UA_TYPES_UINT32]);
        break;
    case TDOT_DT_INT64:
        vi64 = value->kind == TDOT_VAL_STR ? strtoll(value->str, NULL, 10)
                                           : (UA_Int64)num;
        UA_Variant_setScalar(&v, &vi64, &UA_TYPES[UA_TYPES_INT64]);
        break;
    case TDOT_DT_UINT64:
        vu64 = value->kind == TDOT_VAL_STR ? strtoull(value->str, NULL, 10)
                                           : (UA_UInt64)num;
        UA_Variant_setScalar(&v, &vu64, &UA_TYPES[UA_TYPES_UINT64]);
        break;
    case TDOT_DT_FLOAT32:
        vf = (UA_Float)num;
        UA_Variant_setScalar(&v, &vf, &UA_TYPES[UA_TYPES_FLOAT]);
        break;
    case TDOT_DT_FLOAT64:
        vd = (UA_Double)num;
        UA_Variant_setScalar(&v, &vd, &UA_TYPES[UA_TYPES_DOUBLE]);
        break;
    case TDOT_DT_STRING:
        if (value->kind != TDOT_VAL_STR) {
            snprintf(err, errlen, "expected string value");
            return -1;
        }
        vs = UA_STRING((char *)value->str);
        UA_Variant_setScalar(&v, &vs, &UA_TYPES[UA_TYPES_STRING]);
        break;
    default:
        snprintf(err, errlen, "write requires a datatype on the point");
        return -1;
    }

    UA_NodeId node;
    if (UA_NodeId_parse(&node, UA_STRING(up->node_id)) !=
        UA_STATUSCODE_GOOD) {
        snprintf(err, errlen, "invalid node id: %s", up->node_id);
        return -1;
    }
    UA_StatusCode rc =
        UA_Client_writeValueAttribute(ua->client, node, &v);
    UA_NodeId_clear(&node);
    if (rc != UA_STATUSCODE_GOOD) {
        snprintf(err, errlen, "write failed: %s", UA_StatusCode_name(rc));
        return -1;
    }
    return 0;
}

static void destroy(tdot_connector_t *self) {
    free(self->state);
    free(self);
}

tdot_connector_t *tdot_connector_opcua_new(void) {
    tdot_connector_t *c = calloc(1, sizeof *c);
    c->protocol = "opcua";
    c->capabilities_json = CAPABILITIES;
    c->state = calloc(1, sizeof(ua_state_t));
    c->configure = configure;
    c->connect_device = connect_device;
    c->read_point = read_point;
    c->write_point = write_point;
    c->disconnect_device = disconnect_device;
    c->destroy = destroy;
    return c;
}
