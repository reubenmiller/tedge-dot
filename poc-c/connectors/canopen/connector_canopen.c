/* tedge-dot C PoC — CANopen connector on Linux SocketCAN (no external
 * library). Mirrors crates/connector-canopen: expedited SDO upload (read)
 * and download (write) over a shared raw CAN socket, NMT Start broadcast on
 * bus open, identity-object probe (0x1018:0) on connect. Segmented SDO,
 * PDO and heartbeat are out of scope for this PoC.
 *
 * Linux-only: CMake gates compilation of this file to Linux hosts.
 */
#include <errno.h>
#include <linux/can.h>
#include <linux/can/raw.h>
#include <net/if.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#include "tedge_dot/connector.h"
#include "tedge_dot/decode.h"

#define CO_NMT_COB_ID 0x000u
#define CO_SDO_REQ_BASE 0x600u
#define CO_SDO_RESP_BASE 0x580u
#define CO_SDO_TIMEOUT_MS 1000
#define CO_IDENTITY_INDEX 0x1018u

/* Per-point parsed OD address (pt->proto, flat, freed by config_free). */
typedef struct {
    uint16_t index;
    uint8_t subindex;
} co_point_t;

/* Per-device state (dev->proto, flat, freed by config_free). The CAN socket
 * itself is shared connector state (one bus per config). */
typedef struct {
    int node_id;
    bool connected;
} co_device_t;

/* Connector state (self->state): the shared SocketCAN socket for the
 * [connection].interface, opened by the first device to connect and closed
 * when the last connected device disconnects. */
typedef struct {
    char interface[IFNAMSIZ];
    int fd;          /* -1 when closed */
    int nconnected;  /* devices currently connected over this socket */
} co_state_t;

static const char CAPABILITIES[] =
    "{\"protocol\":\"canopen\",\"version\":\"0.1.0-poc\","
    "\"modes\":[\"typed\"],"
    "\"datatypes\":[\"bool\",\"int8\",\"uint8\",\"int16\",\"uint16\","
    "\"int32\",\"uint32\",\"int64\",\"uint64\",\"float32\",\"float64\"],"
    "\"point_kinds\":[\"od_entry\"],"
    "\"command_verbs\":[\"write\"],"
    "\"features\":[\"polling\"],\"subscribe\":false}";

/* ---- SDO frame building / parsing (pure, unit-testable) ------------------ */

/* Expedited upload (read) request: ccs=2, index/subindex little-endian. */
static void sdo_upload_request(uint16_t index, uint8_t subindex,
                               uint8_t out[8]) {
    memset(out, 0, 8);
    out[0] = 0x40;
    out[1] = (uint8_t)(index & 0xff);
    out[2] = (uint8_t)(index >> 8);
    out[3] = subindex;
}

/* Expedited download (write) request: ccs=1, e=1, s=1, n = unused bytes.
 * data is the value in wire (little-endian) order, 1..4 bytes. */
static int sdo_download_request(uint16_t index, uint8_t subindex,
                                const uint8_t *data, size_t len,
                                uint8_t out[8]) {
    if (len < 1 || len > 4)
        return -1;
    memset(out, 0, 8);
    out[0] = (uint8_t)(0x23 | ((4 - len) << 2));
    out[1] = (uint8_t)(index & 0xff);
    out[2] = (uint8_t)(index >> 8);
    out[3] = subindex;
    memcpy(out + 4, data, len);
    return 0;
}

static uint32_t sdo_abort_code(const uint8_t resp[8]) {
    return (uint32_t)resp[4] | ((uint32_t)resp[5] << 8) |
           ((uint32_t)resp[6] << 16) | ((uint32_t)resp[7] << 24);
}

/* Parse an SDO upload response. Returns 0 with the expedited data bytes (in
 * wire little-endian order) copied to data/len, or -1 with err filled (abort,
 * segmented transfer, unexpected command specifier). */
static int sdo_parse_upload(const uint8_t resp[8], uint8_t *data, size_t *len,
                            char *err, size_t errlen) {
    if (resp[0] == 0x80) {
        snprintf(err, errlen, "SDO abort 0x%08x", sdo_abort_code(resp));
        return -1;
    }
    if ((resp[0] & 0xE0) != 0x40) {
        snprintf(err, errlen, "unexpected SDO response 0x%02x", resp[0]);
        return -1;
    }
    if (!(resp[0] & 0x02)) { /* not expedited */
        snprintf(err, errlen, "segmented SDO not supported");
        return -1;
    }
    size_t n = (resp[0] & 0x01) ? 4 - ((resp[0] >> 2) & 0x03) : 4;
    memcpy(data, resp + 4, n);
    *len = n;
    return 0;
}

/* ---- SocketCAN transport -------------------------------------------------- */

/* Send an NMT Start broadcast (node 0 = all nodes), then give nodes a moment
 * to enter Operational before probing. */
static void co_nmt_start_all(int fd) {
    struct can_frame f;
    memset(&f, 0, sizeof f);
    f.can_id = CO_NMT_COB_ID;
    f.can_dlc = 2;
    f.data[0] = 0x01; /* Start Remote Node */
    f.data[1] = 0x00; /* all nodes */
    (void)write(fd, &f, sizeof f);
    struct timespec ts = {0, 100 * 1000 * 1000};
    nanosleep(&ts, NULL);
}

/* Open the shared raw CAN socket when not already open. */
static int co_open_bus(co_state_t *st, char *err, size_t errlen) {
    if (st->fd >= 0)
        return 0;
    int fd = socket(PF_CAN, SOCK_RAW, CAN_RAW);
    if (fd < 0) {
        snprintf(err, errlen, "CAN socket: %s", strerror(errno));
        return -1;
    }
    struct ifreq ifr;
    memset(&ifr, 0, sizeof ifr);
    snprintf(ifr.ifr_name, sizeof ifr.ifr_name, "%s", st->interface);
    if (ioctl(fd, SIOCGIFINDEX, &ifr) < 0) {
        snprintf(err, errlen, "CAN interface %s: %s", st->interface,
                 strerror(errno));
        close(fd);
        return -1;
    }
    struct sockaddr_can addr;
    memset(&addr, 0, sizeof addr);
    addr.can_family = AF_CAN;
    addr.can_ifindex = ifr.ifr_ifindex;
    if (bind(fd, (struct sockaddr *)&addr, sizeof addr) < 0) {
        snprintf(err, errlen, "bind %s: %s", st->interface, strerror(errno));
        close(fd);
        return -1;
    }
    st->fd = fd;
    co_nmt_start_all(fd);
    return 0;
}

static double co_now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec * 1000.0 + (double)ts.tv_nsec / 1e6;
}

/* One SDO request/response exchange with node_id. Unrelated frames (other
 * COB-IDs — e.g. canbus-sim traffic sharing vcan0) are ignored while waiting.
 * Returns 0 with resp filled, -1 on timeout (err "SDO timeout"), -2 on a
 * socket-level error (link down). */
static int co_sdo_transfer(co_state_t *st, int node_id, const uint8_t req[8],
                           uint8_t resp[8], char *err, size_t errlen) {
    struct can_frame f;
    memset(&f, 0, sizeof f);
    f.can_id = CO_SDO_REQ_BASE + (canid_t)node_id;
    f.can_dlc = 8;
    memcpy(f.data, req, 8);
    if (write(st->fd, &f, sizeof f) != (ssize_t)sizeof f) {
        snprintf(err, errlen, "CAN send: %s", strerror(errno));
        return -2;
    }

    double deadline = co_now_ms() + CO_SDO_TIMEOUT_MS;
    for (;;) {
        double left = deadline - co_now_ms();
        if (left <= 0)
            break;
        struct pollfd pfd = {.fd = st->fd, .events = POLLIN};
        int rc = poll(&pfd, 1, (int)left);
        if (rc < 0) {
            if (errno == EINTR)
                continue;
            snprintf(err, errlen, "CAN poll: %s", strerror(errno));
            return -2;
        }
        if (rc == 0)
            break;
        struct can_frame in;
        ssize_t n = read(st->fd, &in, sizeof in);
        if (n < 0) {
            if (errno == EINTR)
                continue;
            snprintf(err, errlen, "CAN recv: %s", strerror(errno));
            return -2;
        }
        if (n < (ssize_t)sizeof in)
            continue;
        if (in.can_id != CO_SDO_RESP_BASE + (canid_t)node_id ||
            in.can_dlc < 8)
            continue; /* not for us — shared bus traffic */
        memcpy(resp, in.data, 8);
        return 0;
    }
    snprintf(err, errlen, "SDO timeout (node %d, no response in %d ms)",
             node_id, CO_SDO_TIMEOUT_MS);
    return -1;
}

/* ---- connector vtable ----------------------------------------------------- */

static int configure(tdot_connector_t *self, tdot_config_t *cfg, char *err,
                     size_t errlen) {
    co_state_t *st = self->state;
    toml_datum_t d;

    if (!cfg->connection ||
        !(d = toml_string_in(cfg->connection, "interface")).ok) {
        snprintf(err, errlen, "[connection] requires interface");
        return -1;
    }
    snprintf(st->interface, sizeof st->interface, "%s", d.u.s);
    free(d.u.s);

    for (size_t i = 0; i < cfg->ndevices; i++) {
        tdot_device_t *dev = &cfg->devices[i];
        co_device_t *cd = calloc(1, sizeof *cd);
        dev->proto = cd;

        d = toml_int_in(dev->protocol_address, "node_id");
        if (!d.ok) {
            snprintf(err, errlen, "device %s: protocol_address requires "
                     "node_id", dev->name);
            return -1;
        }
        if (d.u.i < 1 || d.u.i > 127) {
            snprintf(err, errlen,
                     "device %s: node_id %ld is out of range (must be 1-127)",
                     dev->name, (long)d.u.i);
            return -1;
        }
        cd->node_id = (int)d.u.i;

        for (size_t j = 0; j < dev->npoints; j++) {
            tdot_point_t *pt = &dev->points[j];
            d = toml_int_in(pt->address, "index");
            if (!d.ok) {
                snprintf(err, errlen, "point %s/%s: address requires index",
                         dev->name, pt->id);
                return -1;
            }
            if (d.u.i < 0x0001 || d.u.i > 0xFFFF) {
                snprintf(err, errlen,
                         "point %s/%s: index %ld is out of range "
                         "(must be 0x0001-0xFFFF)",
                         dev->name, pt->id, (long)d.u.i);
                return -1;
            }
            co_point_t *cp = calloc(1, sizeof *cp);
            pt->proto = cp;
            cp->index = (uint16_t)d.u.i;

            d = toml_int_in(pt->address, "subindex");
            if (d.ok) {
                if (d.u.i < 0 || d.u.i > 255) {
                    snprintf(err, errlen,
                             "point %s/%s: subindex %ld is out of range "
                             "(must be 0-255)",
                             dev->name, pt->id, (long)d.u.i);
                    return -1;
                }
                cp->subindex = (uint8_t)d.u.i;
            }

            char addr[64];
            snprintf(addr, sizeof addr, "{\"index\":%u,\"subindex\":%u}",
                     cp->index, cp->subindex);
            pt->addr_json = strdup(addr);
        }
    }
    return 0;
}

static void disconnect_device(tdot_connector_t *self, tdot_device_t *dev) {
    co_state_t *st = self->state;
    co_device_t *cd = dev->proto;
    if (!cd || !cd->connected)
        return;
    cd->connected = false;
    if (st->nconnected > 0)
        st->nconnected--;
    if (st->nconnected == 0 && st->fd >= 0) {
        close(st->fd);
        st->fd = -1;
    }
}

static int connect_device(tdot_connector_t *self, tdot_device_t *dev,
                          char *err, size_t errlen) {
    co_state_t *st = self->state;
    co_device_t *cd = dev->proto;
    disconnect_device(self, dev);

    if (co_open_bus(st, err, errlen) != 0)
        return -1;

    /* Probe: expedited upload of the identity object (0x1018:0). */
    uint8_t req[8], resp[8], data[4];
    size_t n;
    sdo_upload_request(CO_IDENTITY_INDEX, 0, req);
    char reason[TDOT_ERR_MAX];
    int rc = co_sdo_transfer(st, cd->node_id, req, resp, reason,
                             sizeof reason);
    if (rc != 0) {
        snprintf(err, errlen, "probe node %d on %s: %s", cd->node_id,
                 st->interface, reason);
        if (st->nconnected == 0 && st->fd >= 0) {
            close(st->fd);
            st->fd = -1;
        }
        return -1;
    }
    if (sdo_parse_upload(resp, data, &n, reason, sizeof reason) != 0) {
        snprintf(err, errlen, "probe node %d on %s: %s", cd->node_id,
                 st->interface, reason);
        if (st->nconnected == 0 && st->fd >= 0) {
            close(st->fd);
            st->fd = -1;
        }
        return -1;
    }

    cd->connected = true;
    st->nconnected++;
    return 0;
}

static int read_point(tdot_connector_t *self, tdot_device_t *dev,
                      tdot_point_t *pt, tdot_sample_t *out) {
    co_state_t *st = self->state;
    co_device_t *cd = dev->proto;
    co_point_t *cp = pt->proto;

    if (!cd->connected || st->fd < 0) {
        tdot_sample_bad(out, "device not connected");
        return -1;
    }

    uint8_t req[8], resp[8];
    char reason[TDOT_ERR_MAX];
    sdo_upload_request(cp->index, cp->subindex, req);
    int rc = co_sdo_transfer(st, cd->node_id, req, resp, reason,
                             sizeof reason);
    if (rc != 0) {
        /* Timeout means the node is gone; socket errors mean the bus is
         * down. Both trigger the runtime's reconnect backoff. */
        tdot_sample_bad(out, "%s", reason);
        return -1;
    }

    uint8_t data[4];
    size_t n;
    if (sdo_parse_upload(resp, data, &n, reason, sizeof reason) != 0) {
        /* An SDO abort is a healthy transport answering "no". */
        tdot_sample_bad(out, "%s", reason);
        return 0;
    }

    memcpy(out->raw, data, n); /* wire bytes, little-endian order */
    out->raw_len = n;
    out->raw_group = 1;

    if (pt->datatype == TDOT_DT_NONE) {
        out->value.kind = TDOT_VAL_NONE;
        return 0;
    }

    /* CANopen is little-endian on the wire regardless of the per-point
     * endianness config; (little, little) reorders to full byte reversal. */
    char err[TDOT_ERR_MAX];
    if (tdot_decode(pt->datatype, data, n, TDOT_ORDER_LITTLE,
                    TDOT_ORDER_LITTLE, &out->value, err, sizeof err) != 0) {
        tdot_sample_bad(out, "decode error: %s", err);
        return 0;
    }
    if (out->value.kind == TDOT_VAL_NUM && pt->has_transform)
        out->value.num = tdot_transform_apply(&pt->transform, out->value.num);
    return 0;
}

static int write_point(tdot_connector_t *self, tdot_device_t *dev,
                       tdot_point_t *pt, const tdot_value_t *value, char *err,
                       size_t errlen) {
    co_state_t *st = self->state;
    co_device_t *cd = dev->proto;
    co_point_t *cp = pt->proto;

    if (!cd->connected || st->fd < 0) {
        snprintf(err, errlen, "device not connected");
        return -1;
    }

    uint8_t bytes[16];
    size_t len = sizeof bytes;
    if (tdot_encode(pt->datatype, value, TDOT_ORDER_LITTLE, TDOT_ORDER_LITTLE,
                    bytes, &len, err, errlen) != 0)
        return -1;

    uint8_t req[8], resp[8];
    if (sdo_download_request(cp->index, cp->subindex, bytes, len, req) != 0) {
        snprintf(err, errlen,
                 "expedited SDO supports 1-4 bytes, %s is %zu bytes",
                 tdot_datatype_str(pt->datatype), len);
        return -1;
    }
    if (co_sdo_transfer(st, cd->node_id, req, resp, err, errlen) != 0)
        return -1;
    if (resp[0] == 0x80) {
        snprintf(err, errlen, "SDO abort 0x%08x", sdo_abort_code(resp));
        return -1;
    }
    if ((resp[0] & 0xE0) != 0x60) {
        snprintf(err, errlen, "unexpected SDO response 0x%02x", resp[0]);
        return -1;
    }
    return 0;
}

static void destroy(tdot_connector_t *self) {
    co_state_t *st = self->state;
    if (st && st->fd >= 0)
        close(st->fd);
    free(st);
    free(self);
}

tdot_connector_t *tdot_connector_canopen_new(void) {
    tdot_connector_t *c = calloc(1, sizeof *c);
    c->protocol = "canopen";
    c->capabilities_json = CAPABILITIES;
    co_state_t *st = calloc(1, sizeof *st);
    st->fd = -1;
    c->state = st;
    c->configure = configure;
    c->connect_device = connect_device;
    c->read_point = read_point;
    c->write_point = write_point;
    c->disconnect_device = disconnect_device;
    c->destroy = destroy;
    return c;
}
