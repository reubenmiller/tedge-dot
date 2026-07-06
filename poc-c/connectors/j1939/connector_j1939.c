/* tedge-dot C PoC — SAE J1939 connector for heavy-duty machines, built on the
 * vendored MIT Open-SAE-J1939 library (fetched by CMake, SOCKETCAN platform).
 *
 * J1939 is a higher-layer protocol on CAN 2.0B: the 29-bit id carries a PGN
 * (Parameter Group Number) + source address (SA); a PGN's payload holds one or
 * more SPNs (bit-fields). Unlike raw canbus, the library owns the wire concerns
 * a config/DBC-driven connector must not hand-roll — Transport Protocol
 * reassembly, address claiming, diagnostics.
 *
 * Model (mirrors canopen's connection-level shared bus, because the library's
 * SocketCAN backend is a single-socket, single-ECU stack):
 *   [connection]  interface + dbc_file (one J1939 bus per connector)
 *   [[device]]    protocol_address = { source_address }   (one ECU = one SA)
 *   [[point]]     address = { pgn, signal_name }          (SPN layout via DBC)
 *
 * PHASE 1 (this file): passive read of single-frame broadcast PGNs. The library
 * returns RX_MSG_UNKNOWN for any PGN it has no built-in case for, and we capture
 * the raw frame from our read callback — no library patch needed (see
 * README.md#the-patch / the Step-0 validation). Multi-packet (Transport
 * Protocol) PGNs, on-request PGNs and DM1/DM2 diagnostics are Phase 2+.
 *
 * NOTE: SPN bit layout is resolved from a J1939 DBC via the canbus DBC parser
 * (dbc.c, shared). Points key on `pgn` (routing) + `signal_name` (layout);
 * numeric SPN lookup awaits an SPN-attribute extension to dbc.c.
 *
 * Linux-only: CMake gates compilation of this file to Linux hosts.
 */
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "dbc.h"
#include "tedge_dot/connector.h"
#include "tedge_dot/decode.h"

#include "Open_SAE_J1939/Open_SAE_J1939.h"
#include "Hardware/SocketCAN.h"

#define J_PAYLOAD_LEN 8      /* single-frame payload length */
#define J_MAX_PGN_BYTES 256  /* cache slot capacity: single-frame + multi-packet (TP) */
#define J_CACHE_MAX 32       /* distinct (SA,PGN) frame-cache slots */
#define J_IFNAME_MAX 32
#define J_PGN_DM1 65226u     /* active diagnostic trouble codes (0x00FECA) */
#define J_PGN_DM2 65227u     /* previously active DTCs (0x00FECB) */

typedef enum {
    JP_SPN = 0,       /* an SPN signal decoded from a PGN payload */
    JP_DIAG_DTCS,     /* DM1/DM2 active DTC list, as a string */
    JP_DIAG_COUNT,    /* DM1/DM2 active DTC count, as a number */
} jp_kind_t;

/* Per-point resolved address (pt->proto, flat, freed by config_free). */
typedef struct {
    jp_kind_t kind;
    uint8_t sa;         /* owning device's source address */
    uint32_t pgn;
    /* SPN signal layout (kind == JP_SPN): */
    uint32_t start_bit; /* LSB position (Intel) / MSB position (Motorola) */
    uint32_t bit_len;
    bool little_endian;
    bool is_signed;
} jp_point_t;

/* Per-device state (dev->proto, flat, freed by config_free). */
typedef struct {
    uint8_t sa;
    bool connected;
} jd_device_t;

/* One captured PGN payload, keyed by (SA, PGN). Holds single-frame (≤8 B) and
 * reassembled multi-packet (TP) payloads up to J_MAX_PGN_BYTES. */
typedef struct {
    uint8_t sa;
    uint32_t pgn;
    uint8_t data[J_MAX_PGN_BYTES];
    uint16_t len;
    bool seen;
} j_frame_t;

/* Connector state (self->state). One shared J1939 bus, opened by the first
 * device to connect and closed when the last connected device disconnects. */
typedef struct {
    char interface[J_IFNAME_MAX];
    bool bus_up;
    int nconnected;
    J1939 j1939;                 /* library ECU/session/address-claim state */
    j_frame_t cache[J_CACHE_MAX];
    size_t ncache;
} j_state_t;

static const char CAPABILITIES[] =
    "{\"protocol\":\"j1939\",\"version\":\"0.1.0-poc\","
    "\"modes\":[\"typed\"],"
    "\"datatypes\":[\"bool\",\"int8\",\"uint8\",\"int16\",\"uint16\","
    "\"int32\",\"uint32\",\"int64\",\"uint64\",\"float32\",\"float64\","
    "\"string\"],"
    "\"point_kinds\":[\"spn\",\"dm1\",\"dm2\"],"
    "\"command_verbs\":[],"
    "\"features\":[\"polling\"],\"subscribe\":false}";

/* The Open-SAE-J1939 SocketCAN backend is a process-global single socket, so the
 * capture sink is reached through a file-scope pointer to the active connector.
 * The PoC runs one connector per process, so this is safe. */
static j_state_t *g_active;

/* ---- CAN id / PGN helpers (mirror the library's own extraction) ---------- */

static uint32_t pgn_of(uint32_t can_id) {
    uint8_t pf = (uint8_t)(can_id >> 16);
    return (pf >= 240) ? ((can_id >> 8) & 0x3FFFFu) : ((can_id >> 8) & 0x3FF00u);
}
static uint8_t sa_of(uint32_t can_id) { return (uint8_t)(can_id & 0xFF); }

/* ---- SPN bit extraction (same math as connectors/canbus, but length-aware so
 * SPNs at any byte offset in a multi-packet payload decode) ----------------- */

/* Intel: start_bit is the LSB position; bits numbered 0.. little-endian across
 * the payload. Bits beyond `len` read as 0. bit_len ≤ 64. */
static uint64_t extract_intel(const uint8_t *b, size_t len, uint32_t start_bit,
                              uint32_t bit_len) {
    uint64_t v = 0;
    for (uint32_t i = 0; i < bit_len; i++) {
        uint32_t bit = start_bit + i;
        size_t byte = bit >> 3;
        uint64_t bitval = (byte < len) ? ((b[byte] >> (bit & 7)) & 1u) : 0u;
        v |= bitval << i;
    }
    return v;
}
/* Motorola: start_bit is the MSB position in Vector numbering, descending. */
static uint64_t extract_motorola(const uint8_t *b, size_t len,
                                 uint32_t start_bit, uint32_t bit_len) {
    uint64_t result = 0;
    int bit = (int)start_bit;
    for (uint32_t i = 0; i < bit_len; i++, bit--) {
        size_t byte_idx = (size_t)(bit / 8);
        uint8_t v = (bit >= 0 && byte_idx < len)
                        ? (b[byte_idx] >> (bit % 8)) & 1
                        : 0;
        result = (result << 1) | v;
    }
    return result;
}
static uint64_t extract_signal(const jp_point_t *p, const uint8_t *bytes,
                               size_t len) {
    return p->little_endian
               ? extract_intel(bytes, len, p->start_bit, p->bit_len)
               : extract_motorola(bytes, len, p->start_bit, p->bit_len);
}
static int64_t sign_extend(uint64_t value, uint32_t bit_len) {
    if (bit_len == 0 || bit_len >= 64)
        return (int64_t)value;
    if (value & (1ull << (bit_len - 1)))
        return (int64_t)(value | (UINT64_MAX << bit_len));
    return (int64_t)value;
}

/* ---- capture cache ------------------------------------------------------- */

static j_frame_t *cache_find(j_state_t *st, uint8_t sa, uint32_t pgn) {
    for (size_t i = 0; i < st->ncache; i++)
        if (st->cache[i].sa == sa && st->cache[i].pgn == pgn)
            return &st->cache[i];
    return NULL;
}

/* Generic PGN capture sink. Called for single-frame PGNs from the drain loop,
 * and (once the Step-0 TP.DT patch lands) for reassembled multi-packet PGNs. */
void on_raw_pgn(uint8_t sa, uint32_t pgn, const uint8_t *data, uint32_t len) {
    if (!g_active)
        return;
    j_frame_t *f = cache_find(g_active, sa, pgn);
    if (!f) {
        if (g_active->ncache >= J_CACHE_MAX)
            return; /* unconfigured PGN on a busy bus — drop */
        f = &g_active->cache[g_active->ncache++];
        f->sa = sa;
        f->pgn = pgn;
    }
    uint16_t n = len > J_MAX_PGN_BYTES ? J_MAX_PGN_BYTES : (uint16_t)len;
    memcpy(f->data, data, n);
    f->len = n;
    f->seen = true;
}

/* ---- frame draining ------------------------------------------------------ */
/* Under the SOCKETCAN platform the library reads frames DIRECTLY via
 * socketcan_receive: CAN_Read_Message does not invoke a registered read
 * callback (that path exists only for the INTERNAL_CALLBACK platform). So there
 * are no callbacks to register — each new frame is stored in j1939.ID/j1939.data
 * and Listen returns non-RX_MSG_NONE, and we capture the raw frame straight from
 * the struct. Built-in PGNs (and, once the TP.DT patch lands, reassembled
 * multi-packet PGNs) additionally arrive via on_raw_pgn from inside the library.
 * Verified on real hardware (rpi4 aarch64 + vcan): EEC1 → 1000 rpm.
 *
 * Each Listen call blocks at most SOCKETCAN_RCVTIMEOUT (1 ms); loop until no new
 * frame arrives (bounded so a saturated bus can't wedge the poll). */
static void j_pump(j_state_t *st) {
    for (int i = 0; i < 256; i++) {
        ENUM_J1939_RX_MSG rx = Open_SAE_J1939_Listen_For_Messages(&st->j1939);
        if (rx == RX_MSG_NONE)
            break; /* no new frame this iteration */
        on_raw_pgn(sa_of(st->j1939.ID), pgn_of(st->j1939.ID), st->j1939.data,
                   J_PAYLOAD_LEN);
    }
}

/* ---- connector vtable ---------------------------------------------------- */

static int configure(tdot_connector_t *self, tdot_config_t *cfg, char *err,
                     size_t errlen) {
    j_state_t *st = self->state;
    toml_datum_t d;

    if (!cfg->connection ||
        !(d = toml_string_in(cfg->connection, "interface")).ok) {
        snprintf(err, errlen, "[connection] requires interface");
        return -1;
    }
    snprintf(st->interface, sizeof st->interface, "%s", d.u.s);
    free(d.u.s);

    d = toml_string_in(cfg->connection, "dbc_file");
    if (!d.ok) {
        snprintf(err, errlen, "[connection] requires dbc_file");
        return -1;
    }
    dbc_file_t dbc;
    if (dbc_load(d.u.s, &dbc, err, errlen) != 0) {
        free(d.u.s);
        return -1;
    }
    free(d.u.s);

    for (size_t i = 0; i < cfg->ndevices; i++) {
        tdot_device_t *dev = &cfg->devices[i];
        jd_device_t *jd = calloc(1, sizeof *jd);
        dev->proto = jd;

        d = toml_int_in(dev->protocol_address, "source_address");
        if (!d.ok) {
            snprintf(err, errlen,
                     "device %s: protocol_address requires source_address",
                     dev->name);
            dbc_free(&dbc);
            return -1;
        }
        if (d.u.i < 0 || d.u.i > 0xFF) {
            snprintf(err, errlen,
                     "device %s: source_address %ld out of range (0-255)",
                     dev->name, (long)d.u.i);
            dbc_free(&dbc);
            return -1;
        }
        jd->sa = (uint8_t)d.u.i;

        for (size_t j = 0; j < dev->npoints; j++) {
            tdot_point_t *pt = &dev->points[j];
            d = toml_int_in(pt->address, "pgn");
            if (!d.ok) {
                snprintf(err, errlen, "point %s/%s: address requires pgn",
                         dev->name, pt->id);
                dbc_free(&dbc);
                return -1;
            }
            uint32_t pgn = (uint32_t)d.u.i;
            toml_datum_t sn = toml_string_in(pt->address, "signal_name");

            jp_point_t *jp = calloc(1, sizeof *jp);
            pt->proto = jp;
            jp->sa = jd->sa;
            jp->pgn = pgn;

            /* Diagnostics point: DM1/DM2 with no signal_name. The library decodes
             * the DTC list into its struct; the point reports the DTC list (string)
             * or the active count. Optional field = "dtcs" (default) | "count". */
            if ((pgn == J_PGN_DM1 || pgn == J_PGN_DM2) && !sn.ok) {
                jp->kind = JP_DIAG_DTCS;
                toml_datum_t fld = toml_string_in(pt->address, "field");
                if (fld.ok) {
                    if (strcmp(fld.u.s, "count") == 0)
                        jp->kind = JP_DIAG_COUNT;
                    else if (strcmp(fld.u.s, "dtcs") != 0) {
                        snprintf(err, errlen,
                                 "point %s/%s: field must be \"dtcs\" or \"count\"",
                                 dev->name, pt->id);
                        free(fld.u.s);
                        dbc_free(&dbc);
                        return -1;
                    }
                    free(fld.u.s);
                }
                char addr[96];
                snprintf(addr, sizeof addr, "{\"sa\":%u,\"pgn\":%u,\"diag\":\"%s\"}",
                         jd->sa, pgn, jp->kind == JP_DIAG_COUNT ? "count" : "dtcs");
                pt->addr_json = strdup(addr);
                continue;
            }

            /* SPN signal point: bit layout resolved from the DBC by signal_name. */
            if (!sn.ok) {
                snprintf(err, errlen,
                         "point %s/%s: address requires pgn + signal_name",
                         dev->name, pt->id);
                dbc_free(&dbc);
                return -1;
            }
            const dbc_message_t *msg = NULL;
            for (size_t k = 0; k < dbc.n; k++) {
                if (pgn_of(dbc.messages[k].can_id) == pgn) {
                    msg = &dbc.messages[k];
                    break;
                }
            }
            if (!msg) {
                snprintf(err, errlen,
                         "point %s/%s: no DBC message for PGN %u", dev->name,
                         pt->id, pgn);
                free(sn.u.s);
                dbc_free(&dbc);
                return -1;
            }
            const dbc_signal_t *sig = dbc_find_signal(msg, sn.u.s);
            if (!sig) {
                snprintf(err, errlen,
                         "point %s/%s: signal '%s' not found in PGN %u",
                         dev->name, pt->id, sn.u.s, pgn);
                free(sn.u.s);
                dbc_free(&dbc);
                return -1;
            }

            jp->kind = JP_SPN;
            jp->start_bit = sig->start_bit;
            jp->bit_len = sig->bit_len;
            jp->little_endian = sig->little_endian;
            jp->is_signed = sig->is_signed;

            char addr[96];
            snprintf(addr, sizeof addr,
                     "{\"sa\":%u,\"pgn\":%u,\"signal\":\"%s\"}", jd->sa, pgn,
                     sn.u.s);
            pt->addr_json = strdup(addr);
            free(sn.u.s);

            if (!cache_find(st, jd->sa, pgn) && st->ncache < J_CACHE_MAX) {
                st->cache[st->ncache].sa = jd->sa;
                st->cache[st->ncache].pgn = pgn;
                st->ncache++;
            }
        }
    }
    dbc_free(&dbc);
    return 0;
}

static void disconnect_device(tdot_connector_t *self, tdot_device_t *dev) {
    j_state_t *st = self->state;
    jd_device_t *jd = dev->proto;
    if (!jd || !jd->connected)
        return;
    jd->connected = false;
    if (st->nconnected > 0)
        st->nconnected--;
    if (st->nconnected == 0 && st->bus_up) {
        Open_SAE_J1939_Closedown_ECU(&st->j1939);
        st->bus_up = false;
        g_active = NULL;
    }
}

static int connect_device(tdot_connector_t *self, tdot_device_t *dev,
                          char *err, size_t errlen) {
    j_state_t *st = self->state;
    jd_device_t *jd = dev->proto;
    disconnect_device(self, dev);

    if (!st->bus_up) {
        if (socketcan_setup(st->interface) < 0) {
            snprintf(err, errlen, "open %s failed (is the interface up?)",
                     st->interface);
            return -1;
        }
        /* No CAN callbacks to register: the SOCKETCAN platform reads/writes the
         * socket directly. Passive read (Phase 1): rely on library defaults for
         * our own SA/NAME; address claiming + Request PGNs are Phase 2. */
        Open_SAE_J1939_Startup_ECU(&st->j1939);
        g_active = st;
        st->bus_up = true;
    }
    jd->connected = true;
    st->nconnected++;
    return 0;
}

static int read_point(tdot_connector_t *self, tdot_device_t *dev,
                      tdot_point_t *pt, tdot_sample_t *out) {
    j_state_t *st = self->state;
    jd_device_t *jd = dev->proto;
    jp_point_t *jp = pt->proto;

    if (!jd->connected || !st->bus_up) {
        tdot_sample_bad(out, "device not connected");
        return -1;
    }

    /* Diagnostics point (DM1/DM2): the library decodes DTCs into its struct as
     * frames arrive (single-frame or via the TP path); pump, then read the
     * struct. "No active faults" is a valid good sample, so we never wait/bad. */
    if (jp->kind != JP_SPN) {
        j_pump(st);
        const struct DM *dm = &st->j1939.from_other_ecu_dm;
        const struct DM1 *codes = (jp->pgn == J_PGN_DM2) ? &dm->dm2 : &dm->dm1;
        uint8_t active = (jp->pgn == J_PGN_DM2) ? dm->errors_dm2_active
                                                : dm->errors_dm1_active;
        if (active > MAX_DM_FIELD)
            active = MAX_DM_FIELD;
        out->raw_len = 0;

        if (jp->kind == JP_DIAG_COUNT) {
            unsigned n = 0;
            for (uint8_t i = 0; i < active; i++)
                if (codes->from_ecu_address[i] == jp->sa)
                    n++;
            out->value.kind = TDOT_VAL_NUM;
            out->value.num = (double)n;
            return 0;
        }

        /* JP_DIAG_DTCS: compact "SPN<n>/FMI<n>/OC<n>[,...]" list for this SA, or
         * "none". Truncates if it overflows the sample string. */
        out->value.kind = TDOT_VAL_STR;
        char *p = out->value.str;
        size_t rem = sizeof out->value.str;
        int first = 1;
        for (uint8_t i = 0; i < active && rem > 1; i++) {
            if (codes->from_ecu_address[i] != jp->sa)
                continue;
            int w = snprintf(p, rem, "%sSPN%u/FMI%u/OC%u", first ? "" : ",",
                             (unsigned)codes->SPN[i], (unsigned)codes->FMI[i],
                             (unsigned)codes->occurrence_count[i]);
            if (w < 0 || (size_t)w >= rem)
                break; /* truncated */
            p += w;
            rem -= (size_t)w;
            first = 0;
        }
        if (first)
            snprintf(out->value.str, sizeof out->value.str, "none");
        return 0;
    }

    j_pump(st);
    j_frame_t *f = cache_find(st, jp->sa, jp->pgn);
    /* Cold cache: wait briefly for the periodic broadcast (J1939 is push). */
    for (int waited = 0; (!f || !f->seen) && waited < 1200; waited += 50) {
        struct timespec ts = {0, 50 * 1000 * 1000};
        nanosleep(&ts, NULL);
        j_pump(st);
        f = cache_find(st, jp->sa, jp->pgn);
    }
    if (!f || !f->seen) {
        tdot_sample_bad(out, "no frame for SA 0x%02x PGN %u", jp->sa, jp->pgn);
        return 0; /* transport is fine, just no traffic */
    }

    size_t raw_len = f->len > TDOT_RAW_MAX ? TDOT_RAW_MAX : f->len;
    memcpy(out->raw, f->data, raw_len);
    out->raw_len = raw_len;
    out->raw_group = 1;

    if (pt->datatype == TDOT_DT_NONE) {
        out->value.kind = TDOT_VAL_NONE;
        return 0;
    }

    /* SPN resolution/offset is a property of the point (transform), not applied
     * here — matching canbus, so raw SPN values are consistent across
     * connectors. */
    uint64_t bits = extract_signal(jp, f->data, f->len);
    if (jp->is_signed) {
        int64_t sv = sign_extend(bits, jp->bit_len);
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

/* Phase 1 is read-only: J1939 telemetry is broadcast, and writing an SPN means
 * this node sourcing a PGN — deferred (needs a claimed address). */
static int write_point(tdot_connector_t *self, tdot_device_t *dev,
                       tdot_point_t *pt, const tdot_value_t *value, char *err,
                       size_t errlen) {
    (void)self; (void)dev; (void)pt; (void)value;
    snprintf(err, errlen, "j1939 connector is read-only in this PoC");
    return -1;
}

static void destroy(tdot_connector_t *self) {
    j_state_t *st = self->state;
    if (st && st->bus_up)
        Open_SAE_J1939_Closedown_ECU(&st->j1939);
    if (g_active == st)
        g_active = NULL;
    free(st);
    free(self);
}

tdot_connector_t *tdot_connector_j1939_new(void) {
    tdot_connector_t *c = calloc(1, sizeof *c);
    c->protocol = "j1939";
    c->capabilities_json = CAPABILITIES;
    j_state_t *st = calloc(1, sizeof *st);
    c->state = st;
    c->configure = configure;
    c->connect_device = connect_device;
    c->read_point = read_point;
    c->write_point = write_point;
    c->disconnect_device = disconnect_device;
    c->destroy = destroy;
    return c;
}
