/* tedge-dot — OPC UA connector on open62541 (MPL-2.0).
 * Mirrors impl/rust/crates/connector-opcua: client sessions per device, node-id
 * addressed points ("ns=2;s=Temperature"), typed reads/writes, quality "bad"
 * on Bad status codes, and monitored-item push delivery.
 */
#include <open62541/client.h>
#include <open62541/client_config_default.h>
#include <open62541/client_highlevel.h>
#include <open62541/client_subscriptions.h>
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

/* Samples that arrived by subscription since the last drain.
 *
 * open62541 delivers data changes through a callback fired from inside
 * UA_Client_run_iterate(), which drain_subscriptions() calls on the runtime
 * thread -- so producer and consumer are the same thread and the queue needs no
 * locking. It is a ring rather than a single slot per point so that a burst of
 * changes between two ticks is delivered as the separate value changes it was,
 * not collapsed into the latest one. */
#define UA_PUSH_QUEUE_LEN 256

typedef struct {
    tdot_point_t *pt;
    tdot_sample_t sample;
} ua_pending_t;

/* Per-device state (dev->proto, flat; client freed on disconnect). */
typedef struct {
    char endpoint[256];
    UA_Client *client; /* NULL when disconnected */

    /* Push delivery state, all reset on (re)connect. */
    UA_UInt32 sub_id;
    bool subscribed;
    /* Set from open62541's callbacks when the subscription stops existing --
     * deleted by the server, killed with the session, or gone quiet past its
     * keep-alive. open62541 does NOT surface any of these through
     * UA_Client_run_iterate's return code (it keeps returning GOOD), so
     * without this flag a dead subscription would look exactly like a device
     * that simply has nothing new to report: the points stay off the polling
     * schedule and the device goes silent for good behind a healthy-looking
     * `connected` link. */
    bool sub_lost;
    ua_pending_t queue[UA_PUSH_QUEUE_LEN];
    size_t head; /* next slot to write */
    size_t tail; /* next slot to read */
    unsigned long dropped; /* overruns since the last warning */
} ua_device_t;

typedef struct {
    char application_name[128];
    char application_uri[128];
    int connect_timeout_s;
    int request_timeout_s;
} ua_state_t;

static const char CAPABILITIES[] =
    "{\"protocol\":\"opcua\",\"version\":\"" TDOT_VERSION "\","
    "\"modes\":[\"raw\",\"typed\"],"
    "\"datatypes\":[\"bool\",\"int8\",\"uint8\",\"int16\",\"uint16\","
    "\"int32\",\"uint32\",\"int64\",\"uint64\",\"float32\",\"float64\","
    "\"string\"],"
    "\"point_kinds\":[\"variable\"],"
    "\"command_verbs\":[\"write\",\"write-batch\"],"
    "\"features\":[\"polling\",\"subscribe\"],\"subscribe\":true}";

static int configure(tdot_connector_t *self, tdot_config_t *cfg, char *err,
                     size_t errlen) {
    ua_state_t *st = self->state;
    snprintf(st->application_name, sizeof st->application_name, "tedge-dot");
    snprintf(st->application_uri, sizeof st->application_uri, "urn:tedge-dot");
    st->connect_timeout_s = 15;
    /* connector.operation_timeout is the contract-level bound on one protocol
     * call (§8.1); open62541's client timeout is where it actually takes
     * effect, since this runtime cannot cancel a call in flight. The
     * opcua-specific [connection] request_timeout_s still wins when set, so an
     * existing config keeps its tuning. */
    st->request_timeout_s = (int)(cfg->operation_timeout_s + 0.5);
    if (st->request_timeout_s < 1)
        st->request_timeout_s = 1;
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
        /* security_policy / security_mode: this build supports "None"/"none"
         * only; see the parity table in impl/c/README.md (`opcua-security`) */
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
                         "device %s: the C build supports security_policy \"None\" "
                         "only (got %s)",
                         dev->name, d.u.s);
                free(d.u.s);
                return -1;
            }
            free(d.u.s);
        }

        for (size_t j = 0; j < dev->npoints; j++) {
            tdot_point_t *pt = &dev->points[j];
            /* Two address forms, like the Rust module: `node_id = "ns=2;s=X"`
             * or structured `namespace = 2, identifier = "X" | 1001`. */
            ua_point_t *up = calloc(1, sizeof *up);
            pt->proto = up;
            toml_datum_t nd = toml_string_in(pt->address, "node_id");
            if (nd.ok) {
                snprintf(up->node_id, sizeof up->node_id, "%s", nd.u.s);
                free(nd.u.s);
            } else {
                toml_datum_t ns = toml_int_in(pt->address, "namespace");
                toml_datum_t sid = toml_string_in(pt->address, "identifier");
                toml_datum_t iid = toml_int_in(pt->address, "identifier");
                if (!ns.ok || (!sid.ok && !iid.ok)) {
                    if (sid.ok)
                        free(sid.u.s);
                    snprintf(err, errlen,
                             "point %s/%s: address requires node_id, or "
                             "namespace + identifier",
                             dev->name, pt->id);
                    return -1;
                }
                if (sid.ok) {
                    snprintf(up->node_id, sizeof up->node_id, "ns=%d;s=%s",
                             (int)ns.u.i, sid.u.s);
                    free(sid.u.s);
                } else {
                    snprintf(up->node_id, sizeof up->node_id, "ns=%d;i=%lld",
                             (int)ns.u.i, (long long)iid.u.i);
                }
            }

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
    if (ua) {
        /* The subscription died with the session. Drop the id and anything
         * still queued so a reconnect cannot deliver samples belonging to the
         * previous session, or reuse its subscription id. The runtime has
         * already cleared pt->subscribed for every point. */
        ua->sub_id = 0;
        ua->subscribed = false;
        ua->sub_lost = false;
        ua->head = ua->tail = 0;
        ua->dropped = 0;
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
    /* Ask for the None/None endpoint explicitly. The default leaves the mode
     * "invalid" (= pick any endpoint), which some servers (async-opcua) reject
     * at session activation with BadSecurityChecksFailed. This build supports
     * security policy None only anyway (checked in configure()). */
    cc->securityMode = UA_MESSAGESECURITYMODE_NONE;
    UA_String_clear(&cc->securityPolicyUri);
    cc->securityPolicyUri =
        UA_STRING_ALLOC("http://opcfoundation.org/UA/SecurityPolicy#None");
    /* The handshake gets its own bound, as in the Rust module, which wraps
     * wait_for_connection() in connect_timeout_s: establishing a session is
     * several round trips and a slow-but-working server should not be cut off
     * by the per-request timeout. Restored to request_timeout_s below once the
     * session is up, so ordinary reads keep the tighter bound. */
    cc->timeout = (UA_UInt32)st->connect_timeout_s * 1000;
    UA_LocaleId locale = UA_STRING_ALLOC("en");
    UA_String name = UA_STRING_ALLOC(st->application_name);
    UA_String uri = UA_STRING_ALLOC(st->application_uri);
    UA_LocalizedText_clear(&cc->clientDescription.applicationName);
    cc->clientDescription.applicationName.locale = locale;
    cc->clientDescription.applicationName.text = name;
    UA_String_clear(&cc->clientDescription.applicationUri);
    cc->clientDescription.applicationUri = uri;
    /* keep the client quiet unless debugging (TDOT_OPCUA_DEBUG=1 keeps
     * open62541's own handshake log on stdout) */
    if (!getenv("TDOT_OPCUA_DEBUG"))
        cc->logging->log = NULL;

    UA_StatusCode rc = UA_Client_connect(ua->client, ua->endpoint);
    if (rc != UA_STATUSCODE_GOOD) {
        snprintf(err, errlen, "connect %s: %s", ua->endpoint,
                 UA_StatusCode_name(rc));
        UA_Client_delete(ua->client);
        ua->client = NULL;
        return -1;
    }
    /* Session established: from here every request is bounded by
     * connector.operation_timeout (see configure()). open62541 reads
     * config.timeout per request, so changing it now applies to all of them. */
    cc->timeout = (UA_UInt32)st->request_timeout_s * 1000;
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
        /* The list cannot be complete. A synchronous service call on a client
         * that is not fully connected first reconnects inside the call
         * (open62541's __Client_Service -> connectSync) and, when that fails,
         * returns the failed attempt's status -- whatever connection error it
         * ended with. Read as a plain bad point, that keeps the link merely
         * degraded and repeats the blocking reconnect for every point on every
         * tick instead of backing off. A node-level Bad status (unknown node,
         * access denied) arrives over an activated session, so the session
         * state tells the two apart whatever the code. */
        if (!transport_down) {
            UA_SecureChannelState channel_state;
            UA_SessionState session_state;
            UA_StatusCode connect_status;
            UA_Client_getState(ua->client, &channel_state, &session_state,
                               &connect_status);
            transport_down = session_state != UA_SESSIONSTATE_ACTIVATED ||
                             connect_status != UA_STATUSCODE_GOOD;
        }
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

/* ---- push delivery (monitored items) -------------------------------------
 *
 * One subscription per device, one monitored item per subscribe-enabled point,
 * mirroring impl/rust/crates/connector-opcua. The data-change callback runs
 * inside UA_Client_run_iterate(), which the runtime calls from its own loop
 * through drain_subscriptions() -- so the callback only parks the sample in the
 * device's ring and the runtime publishes it, exactly as it does for a polled
 * one. Nothing here touches MQTT.
 */

/* A point's RESOLVED poll interval (point ?? device ?? connector) is its
 * monitored-item sampling interval, which is what the Rust runtime hands its
 * module in PointRef::interval -- always Some, never the module's own default.
 * Both implementations must derive it identically: the same config otherwise
 * monitors at different rates in the two builds, and a subscription sampling
 * more slowly silently coalesces away value changes the other one reports. */
static double sampling_interval_ms(const tdot_point_t *pt) {
    return pt->poll_interval_s * 1000.0;
}

static void on_data_change(UA_Client *client, UA_UInt32 sub_id,
                           void *sub_ctx, UA_UInt32 mon_id, void *mon_ctx,
                           UA_DataValue *value) {
    (void)client;
    (void)sub_id;
    (void)mon_id;
    tdot_device_t *dev = sub_ctx;
    tdot_point_t *pt = mon_ctx;
    if (!dev || !pt)
        return;
    ua_device_t *ua = dev->proto;

    size_t next = (ua->head + 1) % UA_PUSH_QUEUE_LEN;
    if (next == ua->tail) {
        /* Ring full: the runtime has not drained for a while. Drop the NEWEST
         * change rather than overwriting the oldest, so the samples that are
         * already queued keep their order and none is silently replaced. */
        ua->dropped++;
        return;
    }

    ua_pending_t *slot = &ua->queue[ua->head];
    slot->pt = pt;
    tdot_sample_init(&slot->sample);
    if (!value->hasValue || !UA_Variant_isScalar(&value->value)) {
        tdot_sample_bad(&slot->sample, "no scalar value in notification");
    } else if (value->hasStatus && value->status != UA_STATUSCODE_GOOD) {
        tdot_sample_bad(&slot->sample, "%s",
                        UA_StatusCode_name(value->status));
    } else {
        variant_to_sample(&value->value, pt, &slot->sample);
    }
    ua->head = next;
}

/* The subscription is gone. All three hooks below mean the same thing to us --
 * push delivery has stopped -- and are handled by dropping the link so the
 * runtime's existing reconnect/re-subscribe path runs. */
static void mark_subscription_lost(tdot_device_t *dev) {
    if (!dev)
        return;
    ua_device_t *ua = dev->proto;
    if (!ua)
        return;
    ua->subscribed = false;
    ua->sub_lost = true;
}

/* Server deleted the subscription, or it died with the session. */
static void on_subscription_deleted(UA_Client *client, UA_UInt32 sub_id,
                                    void *sub_ctx) {
    (void)client;
    (void)sub_id;
    mark_subscription_lost(sub_ctx);
}

/* Server reported a status change on the subscription (e.g. BadTimeout). */
static void on_subscription_status_change(UA_Client *client, UA_UInt32 sub_id,
                                          void *sub_ctx,
                                          UA_StatusChangeNotification *n) {
    (void)client;
    (void)sub_id;
    (void)n;
    mark_subscription_lost(sub_ctx);
}

/* No PublishResponse within the keep-alive window: the path is up but the
 * subscription is not delivering. */
static void on_subscription_inactivity(UA_Client *client, UA_UInt32 sub_id,
                                       void *sub_ctx) {
    (void)client;
    (void)sub_id;
    mark_subscription_lost(sub_ctx);
}

static int subscribe_device(tdot_connector_t *self, tdot_device_t *dev,
                            char *err, size_t errlen) {
    (void)self;
    ua_device_t *ua = dev->proto;
    if (!ua || !ua->client) {
        snprintf(err, errlen, "device not connected");
        return -1;
    }

    ua->sub_id = 0;
    ua->subscribed = false;
    ua->sub_lost = false;
    ua->head = ua->tail = 0;
    ua->dropped = 0;

    /* Points that opted out (subscribe = false) and write-only points stay on
     * the polling schedule. */
    size_t wanted = 0;
    double fastest = 0;
    for (size_t j = 0; j < dev->npoints; j++) {
        tdot_point_t *pt = &dev->points[j];
        if (!pt->subscribe || !(pt->access & TDOT_ACCESS_READ))
            continue;
        wanted++;
        double ms = sampling_interval_ms(pt);
        if (fastest == 0 || ms < fastest)
            fastest = ms;
    }
    if (wanted == 0)
        return 0; /* nothing to push: armed, with no points */

    /* One subscription per device, publishing at the fastest requested point
     * rate -- the same parameters the Rust module uses. */
    UA_CreateSubscriptionRequest req = UA_CreateSubscriptionRequest_default();
    req.requestedPublishingInterval = fastest;
    req.requestedLifetimeCount = 60;
    req.requestedMaxKeepAliveCount = 20;
    UA_Client_getConfig(ua->client)->subscriptionInactivityCallback =
        on_subscription_inactivity;
    UA_CreateSubscriptionResponse resp = UA_Client_Subscriptions_create(
        ua->client, req, dev /*subContext*/, on_subscription_status_change,
        on_subscription_deleted);
    if (resp.responseHeader.serviceResult != UA_STATUSCODE_GOOD) {
        snprintf(err, errlen, "create_subscription failed: %s",
                 UA_StatusCode_name(resp.responseHeader.serviceResult));
        UA_CreateSubscriptionResponse_clear(&resp);
        return -1;
    }
    ua->sub_id = resp.subscriptionId;
    ua->subscribed = true;
    /* The response header can carry a heap-allocated diagnostics/string table;
     * a flapping link re-subscribes often enough for that to add up. */
    UA_CreateSubscriptionResponse_clear(&resp);

    size_t armed = 0;
    for (size_t j = 0; j < dev->npoints; j++) {
        tdot_point_t *pt = &dev->points[j];
        if (!pt->subscribe || !(pt->access & TDOT_ACCESS_READ))
            continue;
        ua_point_t *up = pt->proto;
        UA_NodeId node;
        if (UA_NodeId_parse(&node, UA_STRING(up->node_id)) !=
            UA_STATUSCODE_GOOD) {
            UA_NodeId_clear(&node);
            continue; /* the polling path reports this as a bad sample */
        }

        UA_MonitoredItemCreateRequest mreq =
            UA_MonitoredItemCreateRequest_default(node);
        mreq.requestedParameters.samplingInterval = sampling_interval_ms(pt);
        mreq.requestedParameters.queueSize = 1;
        mreq.requestedParameters.discardOldest = true;
        UA_MonitoredItemCreateResult mres =
            UA_Client_MonitoredItems_createDataChange(
                ua->client, ua->sub_id, UA_TIMESTAMPSTORETURN_BOTH, mreq,
                pt /*monContext*/, on_data_change, NULL);
        UA_NodeId_clear(&node);
        if (mres.statusCode == UA_STATUSCODE_GOOD) {
            /* Telling the runtime this point is pushed is what takes it off the
             * polling schedule. A node the server refuses to monitor simply
             * stays polled. */
            pt->subscribed = true;
            armed++;
        }
        UA_MonitoredItemCreateResult_clear(&mres); /* filterResult may be heap */
    }
    if (armed == 0) {
        /* The subscription exists but carries nothing: tear it down rather than
         * leaving an idle one on the server. */
        UA_Client_Subscriptions_deleteSingle(ua->client, ua->sub_id);
        ua->sub_id = 0;
        ua->subscribed = false;
        /* deleteSingle fires on_subscription_deleted, which sets sub_lost. Clearing it here
         * keeps "we tore this down deliberately" from looking like "it died on us": today the
         * runtime never drains a device with no subscribed points, but a future change that
         * did would see a permanent -1 and reconnect in a 1s loop against a server that simply
         * refuses monitored items. */
        ua->sub_lost = false;
        snprintf(err, errlen,
                 "server accepted no monitored item for %zu requested point(s)",
                 wanted);
        return -1;
    }
    return 0;
}

static int drain_subscriptions(tdot_connector_t *self, tdot_device_t *dev,
                               tdot_sample_sink_t sink, void *sink_ctx) {
    (void)self;
    ua_device_t *ua = dev->proto;
    if (!ua || !ua->client)
        return 0;
    if (ua->sub_lost)
        return -1; /* reported below; drop the link so we re-subscribe */
    if (!ua->subscribed)
        return 0;

    /* Non-blocking: services whatever publish responses have arrived, firing
     * on_data_change() for each notification, and keeps publish requests in
     * flight. It can also fire the subscription callbacks above, so re-check
     * sub_lost afterwards. */
    UA_StatusCode rc = UA_Client_run_iterate(ua->client, 0);
    if (rc != UA_STATUSCODE_GOOD)
        return -1; /* session gone: the runtime reconnects and re-subscribes */
    if (ua->sub_lost) {
        fprintf(stderr,
                "warn  device %s: OPC UA subscription lost; reconnecting\n",
                dev->name);
        return -1;
    }

    /* The session can also go down without run_iterate saying so. Points
     * delivered by push are off the polling schedule, so nothing else would
     * notice. */
    UA_SecureChannelState channel_state;
    UA_SessionState session_state;
    UA_StatusCode connect_status;
    UA_Client_getState(ua->client, &channel_state, &session_state,
                       &connect_status);
    if (session_state != UA_SESSIONSTATE_ACTIVATED ||
        connect_status != UA_STATUSCODE_GOOD)
        return -1;

    while (ua->tail != ua->head) {
        ua_pending_t *slot = &ua->queue[ua->tail];
        sink(sink_ctx, dev, slot->pt, &slot->sample);
        ua->tail = (ua->tail + 1) % UA_PUSH_QUEUE_LEN;
    }
    if (ua->dropped) {
        fprintf(stderr,
                "warn  device %s: dropped %lu pushed sample(s); the queue of "
                "%d filled between two runtime ticks\n",
                dev->name, ua->dropped, UA_PUSH_QUEUE_LEN);
        ua->dropped = 0;
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
    c->subscribe_device = subscribe_device;
    c->drain_subscriptions = drain_subscriptions;
    c->disconnect_device = disconnect_device;
    c->destroy = destroy;
    return c;
}
