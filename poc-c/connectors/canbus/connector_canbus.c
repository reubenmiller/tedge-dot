/* tedge-dot C PoC — CAN bus connector on Linux SocketCAN. Mirrors
 * crates/connector-canbus: DBC-driven signal extraction, Intel/Motorola bit
 * layouts, read-modify-write signal encoding. The Rust connector is
 * push-based; this PoC adapts it to the poll runtime by draining pending
 * frames into a last-frame cache on every read.
 */
#include <errno.h>
#include <fcntl.h>
#include <linux/can.h>
#include <linux/can/raw.h>
#include <poll.h>
#include <math.h>
#include <net/if.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

#include "dbc.h"
#include "tedge_dot/connector.h"
#include "tedge_dot/decode.h"

#define CB_PAYLOAD_LEN 8
#define CB_MAX_IDS 32 /* distinct CAN ids per device (frame cache slots) */

/* Per-point resolved signal (pt->proto, flat, freed by config_free). */
typedef struct {
    uint32_t can_id;
    bool extended;
    int dlc;
    uint32_t start_bit; /* LSB position (Intel) / MSB position (Motorola) */
    uint32_t bit_len;
    bool little_endian;
    bool is_signed;
    double factor;
    double offset;
} cb_point_t;

typedef struct {
    uint32_t can_id;
    uint8_t data[CB_PAYLOAD_LEN];
    uint8_t len;
    bool seen;
} cb_frame_t;

/* Per-device state (dev->proto, flat, freed by config_free; the socket is
 * closed in disconnect_device). */
typedef struct {
    char interface[IFNAMSIZ];
    int fd; /* -1 when disconnected */
    cb_frame_t cache[CB_MAX_IDS]; /* last frame per configured CAN id */
    size_t nids;
} cb_device_t;

static const char CAPABILITIES[] =
    "{\"protocol\":\"canbus\",\"version\":\"0.1.0-poc\","
    "\"modes\":[\"typed\"],"
    "\"datatypes\":[\"bool\",\"int8\",\"uint8\",\"int16\",\"uint16\","
    "\"int32\",\"uint32\",\"int64\",\"uint64\",\"float32\",\"float64\"],"
    "\"point_kinds\":[\"signal\"],"
    "\"command_verbs\":[\"write\"],"
    "\"features\":[\"polling\"],\"subscribe\":false}";

/* ---- CAN signal bit extraction / encoding (matches can-dbc / Vector) ---- */

static uint64_t bit_mask(uint32_t bit_len) {
    return bit_len >= 64 ? UINT64_MAX : (1ull << bit_len) - 1;
}

/* Intel: start_bit is the LSB position, bits numbered 0..63 little-endian
 * across the payload (bit n = byte n/8, bit n%8). */
static uint64_t extract_intel(const uint8_t *bytes, uint32_t start_bit,
                              uint32_t bit_len) {
    uint64_t raw = 0;
    for (size_t i = 0; i < CB_PAYLOAD_LEN; i++)
        raw |= (uint64_t)bytes[i] << (i * 8);
    return (raw >> start_bit) & bit_mask(bit_len);
}

/* Motorola: start_bit is the MSB position in Vector numbering; traversal
 * runs MSB-first downward within a byte, wrapping to the next byte's bit 7
 * (a plain decrement in this numbering). */
static uint64_t extract_motorola(const uint8_t *bytes, uint32_t start_bit,
                                 uint32_t bit_len) {
    uint64_t result = 0;
    int bit = (int)start_bit;
    for (uint32_t i = 0; i < bit_len; i++, bit--) {
        int byte_idx = bit / 8;
        uint8_t bit_val = (byte_idx >= 0 && byte_idx < CB_PAYLOAD_LEN)
                              ? (bytes[byte_idx] >> (bit % 8)) & 1
                              : 0;
        result = (result << 1) | bit_val;
    }
    return result;
}

static uint64_t extract_signal(const cb_point_t *cp, const uint8_t *bytes) {
    return cp->little_endian
               ? extract_intel(bytes, cp->start_bit, cp->bit_len)
               : extract_motorola(bytes, cp->start_bit, cp->bit_len);
}

static void encode_intel(uint8_t *payload, uint32_t start_bit,
                         uint32_t bit_len, uint64_t value) {
    uint64_t masked = value & bit_mask(bit_len);
    for (uint32_t i = 0; i < bit_len; i++) {
        uint32_t abs_bit = start_bit + i;
        size_t byte_idx = abs_bit / 8;
        uint8_t bit_idx = abs_bit % 8;
        if (byte_idx < CB_PAYLOAD_LEN) {
            uint8_t bit_val = (masked >> i) & 1;
            payload[byte_idx] = (uint8_t)((payload[byte_idx] &
                                           ~(1u << bit_idx)) |
                                          (uint8_t)(bit_val << bit_idx));
        }
    }
}

static void encode_motorola(uint8_t *payload, uint32_t start_bit,
                            uint32_t bit_len, uint64_t value) {
    uint64_t masked = value & bit_mask(bit_len);
    int bit = (int)start_bit;
    for (uint32_t i = 0; i < bit_len; i++, bit--) {
        uint8_t bit_val = (masked >> (bit_len - 1 - i)) & 1;
        int byte_idx = bit / 8;
        uint8_t bit_idx = (uint8_t)(bit % 8);
        if (byte_idx >= 0 && byte_idx < CB_PAYLOAD_LEN)
            payload[byte_idx] = (uint8_t)((payload[byte_idx] &
                                           ~(1u << bit_idx)) |
                                          (uint8_t)(bit_val << bit_idx));
    }
}

static void encode_signal(const cb_point_t *cp, uint8_t *payload,
                          uint64_t value) {
    if (cp->little_endian)
        encode_intel(payload, cp->start_bit, cp->bit_len, value);
    else
        encode_motorola(payload, cp->start_bit, cp->bit_len, value);
}

static int64_t sign_extend(uint64_t value, uint32_t bit_len) {
    if (bit_len == 0 || bit_len >= 64)
        return (int64_t)value;
    if (value & (1ull << (bit_len - 1)))
        return (int64_t)(value | (UINT64_MAX << bit_len));
    return (int64_t)value;
}

/* ---- frame cache -------------------------------------------------------- */

static cb_frame_t *cache_find(cb_device_t *cb, uint32_t can_id) {
    for (size_t i = 0; i < cb->nids; i++)
        if (cb->cache[i].can_id == can_id)
            return &cb->cache[i];
    return NULL;
}

/* ---- connector ---------------------------------------------------------- */

static int configure(tdot_connector_t *self, tdot_config_t *cfg, char *err,
                     size_t errlen) {
    (void)self; /* no [connection] parameters for canbus */

    for (size_t i = 0; i < cfg->ndevices; i++) {
        tdot_device_t *dev = &cfg->devices[i];
        cb_device_t *cb = calloc(1, sizeof *cb);
        dev->proto = cb;
        cb->fd = -1;
        toml_table_t *pa = dev->protocol_address;
        toml_datum_t d;

        d = toml_string_in(pa, "interface");
        if (!d.ok) {
            snprintf(err, errlen, "device %s: interface required", dev->name);
            return -1;
        }
        if (strlen(d.u.s) >= sizeof cb->interface) {
            snprintf(err, errlen, "device %s: interface name '%s' too long",
                     dev->name, d.u.s);
            free(d.u.s);
            return -1;
        }
        snprintf(cb->interface, sizeof cb->interface, "%s", d.u.s);
        free(d.u.s);

        /* bitrate is informational only; the connector never sets it */
        (void)toml_int_in(pa, "bitrate");

        d = toml_string_in(pa, "dbc_file");
        if (!d.ok) {
            snprintf(err, errlen, "device %s: dbc_file required", dev->name);
            return -1;
        }
        dbc_file_t dbc;
        if (dbc_load(d.u.s, &dbc, err, errlen) != 0) {
            free(d.u.s);
            return -1;
        }
        free(d.u.s);

        for (size_t j = 0; j < dev->npoints; j++) {
            tdot_point_t *pt = &dev->points[j];
            toml_datum_t mn = toml_string_in(pt->address, "message_name");
            toml_datum_t sn = toml_string_in(pt->address, "signal_name");
            if (!mn.ok || !sn.ok) {
                snprintf(err, errlen,
                         "point %s/%s: address requires message_name + "
                         "signal_name",
                         dev->name, pt->id);
                if (mn.ok)
                    free(mn.u.s);
                if (sn.ok)
                    free(sn.u.s);
                dbc_free(&dbc);
                return -1;
            }

            const dbc_message_t *msg = dbc_find_message(&dbc, mn.u.s);
            if (!msg) {
                snprintf(err, errlen,
                         "point %s/%s: DBC message '%s' not found", dev->name,
                         pt->id, mn.u.s);
                free(mn.u.s);
                free(sn.u.s);
                dbc_free(&dbc);
                return -1;
            }
            const dbc_signal_t *sig = dbc_find_signal(msg, sn.u.s);
            if (!sig) {
                snprintf(err, errlen,
                         "point %s/%s: DBC signal '%s' not found in message "
                         "'%s'",
                         dev->name, pt->id, sn.u.s, mn.u.s);
                free(mn.u.s);
                free(sn.u.s);
                dbc_free(&dbc);
                return -1;
            }
            free(mn.u.s);
            free(sn.u.s);

            cb_point_t *cp = calloc(1, sizeof *cp);
            pt->proto = cp;
            cp->can_id = msg->can_id;
            cp->extended = msg->extended;
            cp->dlc = msg->dlc;
            cp->start_bit = sig->start_bit;
            cp->bit_len = sig->bit_len;
            cp->little_endian = sig->little_endian;
            cp->is_signed = sig->is_signed;
            cp->factor = sig->factor;
            cp->offset = sig->offset;

            if (!cache_find(cb, cp->can_id)) {
                if (cb->nids >= CB_MAX_IDS) {
                    snprintf(err, errlen,
                             "device %s: more than %d distinct CAN ids",
                             dev->name, CB_MAX_IDS);
                    dbc_free(&dbc);
                    return -1;
                }
                cb->cache[cb->nids++].can_id = cp->can_id;
            }

            char addr[64];
            snprintf(addr, sizeof addr, "{\"can_id\":\"0x%x\"}", cp->can_id);
            pt->addr_json = strdup(addr);
        }
        dbc_free(&dbc);
    }
    return 0;
}

static void disconnect_device(tdot_connector_t *self, tdot_device_t *dev) {
    (void)self;
    cb_device_t *cb = dev->proto;
    if (cb && cb->fd >= 0) {
        close(cb->fd);
        cb->fd = -1;
    }
}

static int connect_device(tdot_connector_t *self, tdot_device_t *dev,
                          char *err, size_t errlen) {
    cb_device_t *cb = dev->proto;
    disconnect_device(self, dev);

    int fd = socket(PF_CAN, SOCK_RAW, CAN_RAW);
    if (fd < 0) {
        snprintf(err, errlen, "can socket: %s", strerror(errno));
        return -1;
    }

    /* Best effort, like the Rust connector: the drain loop drops frames for
     * unconfigured ids anyway. */
    struct can_filter filters[CB_MAX_IDS];
    for (size_t i = 0; i < cb->nids; i++) {
        filters[i].can_id = cb->cache[i].can_id;
        filters[i].can_mask = CAN_EFF_MASK;
    }
    if (cb->nids > 0)
        (void)setsockopt(fd, SOL_CAN_RAW, CAN_RAW_FILTER, filters,
                         (socklen_t)(cb->nids * sizeof filters[0]));

    struct ifreq ifr;
    memset(&ifr, 0, sizeof ifr);
    snprintf(ifr.ifr_name, sizeof ifr.ifr_name, "%s", cb->interface);
    if (ioctl(fd, SIOCGIFINDEX, &ifr) < 0) {
        snprintf(err, errlen, "interface %s: %s", cb->interface,
                 strerror(errno));
        close(fd);
        return -1;
    }

    struct sockaddr_can addr;
    memset(&addr, 0, sizeof addr);
    addr.can_family = AF_CAN;
    addr.can_ifindex = ifr.ifr_ifindex;
    if (bind(fd, (struct sockaddr *)&addr, sizeof addr) < 0) {
        snprintf(err, errlen, "bind %s: %s", cb->interface, strerror(errno));
        close(fd);
        return -1;
    }

    if (fcntl(fd, F_SETFL, O_NONBLOCK) < 0) {
        snprintf(err, errlen, "fcntl %s: %s", cb->interface, strerror(errno));
        close(fd);
        return -1;
    }

    cb->fd = fd;
    return 0;
}

/* Drain every pending frame into the last-frame cache (the poll-based
 * rendering of the Rust subscribe loop). Returns 0 when the socket is
 * healthy (including EAGAIN), -1 on a real receive error. */
static int drain_frames(cb_device_t *cb) {
    for (;;) {
        struct can_frame fr;
        ssize_t n = read(cb->fd, &fr, sizeof fr);
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK)
                return 0;
            if (errno == EINTR)
                continue;
            return -1;
        }
        if ((size_t)n < sizeof fr)
            return 0;
        cb_frame_t *e = cache_find(cb, fr.can_id & CAN_EFF_MASK);
        if (e) {
            memcpy(e->data, fr.data, CB_PAYLOAD_LEN);
            e->len = fr.can_dlc > CB_PAYLOAD_LEN ? CB_PAYLOAD_LEN
                                                 : fr.can_dlc;
            e->seen = true;
        }
    }
}

static int read_point(tdot_connector_t *self, tdot_device_t *dev,
                      tdot_point_t *pt, tdot_sample_t *out) {
    (void)self;
    cb_device_t *cb = dev->proto;
    cb_point_t *cp = pt->proto;

    if (cb->fd < 0) {
        tdot_sample_bad(out, "device not connected");
        return -1;
    }
    if (drain_frames(cb) != 0) {
        tdot_sample_bad(out, "can recv error: %s", strerror(errno));
        return -1; /* transport down -> runtime reconnects */
    }

    cb_frame_t *fr = cache_find(cb, cp->can_id);
    /* Cold cache (e.g. a one-shot CLI read right after the socket opened):
     * wait briefly for the periodic broadcast instead of failing outright. */
    for (int waited_ms = 0; (!fr || !fr->seen) && waited_ms < 1200;
         waited_ms += 50) {
        struct pollfd pfd = {.fd = cb->fd, .events = POLLIN};
        if (poll(&pfd, 1, 50) < 0 && errno != EINTR)
            break;
        if (drain_frames(cb) != 0) {
            tdot_sample_bad(out, "can recv error: %s", strerror(errno));
            return -1;
        }
        fr = cache_find(cb, cp->can_id);
    }
    if (!fr || !fr->seen) {
        tdot_sample_bad(out, "no frame received for can id 0x%x", cp->can_id);
        return 0; /* transport is fine, just no traffic */
    }

    size_t raw_len = (size_t)cp->dlc;
    if (raw_len > CB_PAYLOAD_LEN)
        raw_len = CB_PAYLOAD_LEN;
    memcpy(out->raw, fr->data, raw_len);
    out->raw_len = raw_len;
    out->raw_group = 1;

    if (pt->datatype == TDOT_DT_NONE) {
        out->value.kind = TDOT_VAL_NONE;
        return 0;
    }

    /* DBC factor/offset are deliberately NOT applied, matching the Rust
     * connector: scaling is a property of the signal declared on the point
     * (transform) or handled in flows, so both implementations emit the
     * same values for the same config. */
    uint64_t bits = extract_signal(cp, fr->data);
    if (cp->is_signed) {
        int64_t sv = sign_extend(bits, cp->bit_len);
        if (sv > TDOT_JS_SAFE_MAX || sv < -TDOT_JS_SAFE_MAX) {
            out->value.kind = TDOT_VAL_STR;
            snprintf(out->value.str, sizeof out->value.str, "%lld",
                     (long long)sv);
            return 0;
        }
        out->value.num = (double)sv;
    } else {
        if (bits > (uint64_t)TDOT_JS_SAFE_MAX) {
            out->value.kind = TDOT_VAL_STR;
            snprintf(out->value.str, sizeof out->value.str, "%llu",
                     (unsigned long long)bits);
            return 0;
        }
        out->value.num = (double)bits;
    }

    if (pt->datatype == TDOT_DT_BOOL) {
        out->value.kind = TDOT_VAL_BOOL;
        out->value.b = out->value.num != 0.0;
        return 0;
    }
    out->value.kind = TDOT_VAL_NUM;
    if (pt->has_transform)
        out->value.num = tdot_transform_apply(&pt->transform, out->value.num);
    return 0;
}

static int write_point(tdot_connector_t *self, tdot_device_t *dev,
                       tdot_point_t *pt, const tdot_value_t *value, char *err,
                       size_t errlen) {
    (void)self;
    cb_device_t *cb = dev->proto;
    cb_point_t *cp = pt->proto;

    if (cb->fd < 0) {
        snprintf(err, errlen, "device not connected");
        return -1;
    }

    uint64_t bits;
    if (pt->datatype == TDOT_DT_BOOL) {
        bool b = value->kind == TDOT_VAL_BOOL  ? value->b
                 : value->kind == TDOT_VAL_NUM ? (value->num != 0)
                                               : false;
        bits = b ? 1 : 0;
    } else if (value->kind == TDOT_VAL_NUM ||
               value->kind == TDOT_VAL_BOOL) {
        /* raw signal value, no DBC scaling (see read_point) */
        double physical = value->kind == TDOT_VAL_BOOL ? (value->b ? 1 : 0)
                                                       : value->num;
        bits = (uint64_t)llround(physical);
    } else {
        snprintf(err, errlen, "write requires a numeric or boolean value");
        return -1;
    }
    bits &= bit_mask(cp->bit_len);

    /* Read-modify-write on the last seen frame (zeros if never seen). */
    struct can_frame fr;
    memset(&fr, 0, sizeof fr);
    cb_frame_t *cached = cache_find(cb, cp->can_id);
    if (cached && cached->seen)
        memcpy(fr.data, cached->data, CB_PAYLOAD_LEN);
    encode_signal(cp, fr.data, bits);

    fr.can_id = cp->can_id | (cp->extended ? CAN_EFF_FLAG : 0);
    fr.can_dlc = (uint8_t)(cp->dlc > CB_PAYLOAD_LEN ? CB_PAYLOAD_LEN
                                                    : cp->dlc);
    ssize_t n = write(cb->fd, &fr, sizeof fr);
    if (n != (ssize_t)sizeof fr) {
        snprintf(err, errlen, "can send 0x%x: %s", cp->can_id,
                 strerror(errno));
        return -1;
    }

    if (cached) {
        memcpy(cached->data, fr.data, CB_PAYLOAD_LEN);
        cached->len = fr.can_dlc;
        cached->seen = true;
    }
    return 0;
}

static void destroy(tdot_connector_t *self) {
    free(self->state);
    free(self);
}

tdot_connector_t *tdot_connector_canbus_new(void) {
    tdot_connector_t *c = calloc(1, sizeof *c);
    c->protocol = "canbus";
    c->capabilities_json = CAPABILITIES;
    c->state = NULL;
    c->configure = configure;
    c->connect_device = connect_device;
    c->read_point = read_point;
    c->write_point = write_point;
    c->disconnect_device = disconnect_device;
    c->destroy = destroy;
    return c;
}
