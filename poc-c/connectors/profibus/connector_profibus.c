/* tedge-dot C PoC — PROFIBUS-DP connector (minimal single-master DP-V0
 * class-1 master). Mirrors crates/connector-profibus, replacing the profirust
 * stack with a hand-rolled FDL/DP subset that speaks to DP-V0 slaves over a
 * serial-over-TCP byte stream ("tcp://host:port" — RS-485 device servers or
 * the containerised slave simulator). No FDL token timing is implemented; the
 * PoC drives one master, one bus, request/response only.
 *
 * Init sequence per peripheral: Slave_Diag (SAP 60) -> Set_Prm (SAP 61) ->
 * Chk_Cfg (SAP 62) -> Slave_Diag -> cyclic Data_Exchange (default SAP).
 *
 * Like the Rust connector, a dedicated bus thread owns the socket and runs
 * the DP cycle (~50 ms); the vtable methods exchange data with it through a
 * mutex-guarded input snapshot / output buffer / link state per peripheral.
 */
#include <errno.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#include "tedge_dot/connector.h"
#include "tedge_dot/decode.h"

/* ---- protocol constants -------------------------------------------------- */

#define PB_SD1 0x10 /* fixed frame, no data */
#define PB_SD2 0x68 /* variable length frame */
#define PB_SC 0xE5  /* short acknowledge */
#define PB_ED 0x16  /* end delimiter */

#define PB_SAP_DIAG 0x3C    /* 60 — Slave_Diag */
#define PB_SAP_SET_PRM 0x3D /* 61 — Set_Prm */
#define PB_SAP_CHK_CFG 0x3E /* 62 — Chk_Cfg */
#define PB_SAP_MASTER 0x3E  /* 62 — master's response SAP (SSAP) */

#define PB_FC_SRD 0x7D /* Send-and-Request-Data, high priority */

#define PB_MAX_IO 244  /* DP-V0 I/O buffer ceiling */
#define PB_MAX_CFG 64  /* Chk_Cfg / Set_Prm user payload ceiling */
#define PB_MAX_DEV 16
#define PB_MAX_FRAME 512

#define PB_CYCLE_MS 50        /* data-exchange period */
#define PB_RESP_TIMEOUT_MS 500
#define PB_RETRIES 3
#define PB_RECONNECT_MS 2000
#define PB_CONNECT_WAIT_MS 10000 /* connect_device: wait for data exchange */

/* ---- per-point / per-device state ---------------------------------------- */

typedef struct {
    bool is_input;
    size_t byte_offset;
    int bit_offset; /* -1 when absent */
    int bit_count;  /* valid when bit_offset >= 0; default 1 */
} pb_point_t;

typedef enum {
    PB_LINK_UNKNOWN = 0, /* not yet through Diag/Prm/Cfg */
    PB_LINK_DX,          /* in cyclic data exchange */
    PB_LINK_FAULT,       /* bus or peripheral fault (see fault[]) */
} pb_link_t;

/* Per-device state (dev->proto, flat, freed by config_free). All runtime
 * fields below `active` are guarded by pb_state_t.lock. */
typedef struct {
    uint8_t station;
    uint16_t ident_number;
    uint16_t max_tsdr;
    uint8_t cfg[PB_MAX_CFG];
    size_t cfg_len;
    uint8_t prm[PB_MAX_CFG];
    size_t prm_len;
    size_t input_len;
    size_t output_len;

    bool active; /* set by connect_device, cleared by disconnect_device */
    pb_link_t link;
    char fault[TDOT_ERR_MAX];
    uint8_t inputs[PB_MAX_IO];  /* latest PI_I snapshot from the bus thread */
    uint8_t outputs[PB_MAX_IO]; /* pending PI_Q, sent every cycle */
} pb_slave_t;

/* Connector state (self->state, flat, freed in destroy). */
typedef struct {
    char host[128];
    int port;
    uint8_t master_address;

    pb_slave_t *slaves[PB_MAX_DEV]; /* borrowed from dev->proto */
    size_t nslaves;

    pthread_mutex_t lock;
    pthread_t thread;
    bool thread_running;
    volatile int stop;

    /* guarded by lock: */
    bool bus_up;          /* TCP socket currently connected */
    uint64_t cycle_count; /* completed DP cycles (write flush detection) */

    /* bus thread private: */
    int sock;
    uint8_t rx[PB_MAX_FRAME * 2];
    size_t rx_len;
} pb_state_t;

static const char CAPABILITIES[] =
    "{\"protocol\":\"profibus\",\"version\":\"0.1.0-poc\","
    "\"modes\":[\"typed\"],"
    "\"datatypes\":[\"bool\",\"int8\",\"uint8\",\"int16\",\"uint16\","
    "\"int32\",\"uint32\",\"float32\"],"
    "\"point_kinds\":[\"io_byte\",\"io_bit\"],"
    "\"command_verbs\":[\"write\"],"
    "\"features\":[\"polling\"],\"subscribe\":false}";

/* ---- small utilities ------------------------------------------------------ */

static double mono_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

static void ms_sleep(int ms) {
    struct timespec ts = {ms / 1000, (long)(ms % 1000) * 1000000L};
    nanosleep(&ts, NULL);
}

/* ---- framing -------------------------------------------------------------- */

typedef struct {
    uint8_t kind; /* PB_SC, PB_SD1 or PB_SD2 */
    uint8_t da, sa, fc; /* SAP flag stripped from da/sa */
    int dsap, ssap;     /* -1 when absent */
    uint8_t pdu[PB_MAX_FRAME];
    size_t pdu_len;
} pb_frame_t;

static uint8_t pb_fcs(const uint8_t *p, size_t n) {
    unsigned s = 0;
    for (size_t i = 0; i < n; i++)
        s += p[i];
    return (uint8_t)(s & 0xFF);
}

/* Build an SD2 frame. dsap/ssap of -1 build a default-SAP frame
 * (Data_Exchange); otherwise the 0x80 SAP-extension flag is set on both
 * addresses. Returns total frame length. */
static size_t pb_build_sd2(uint8_t *out, uint8_t da, uint8_t sa, uint8_t fc,
                           int dsap, int ssap, const uint8_t *pdu,
                           size_t pdu_len) {
    size_t i = 4; /* header written last, payload starts at [4] */
    out[i++] = dsap >= 0 ? (uint8_t)(da | 0x80) : da;
    out[i++] = ssap >= 0 ? (uint8_t)(sa | 0x80) : sa;
    out[i++] = fc;
    if (dsap >= 0)
        out[i++] = (uint8_t)dsap;
    if (ssap >= 0)
        out[i++] = (uint8_t)ssap;
    memcpy(&out[i], pdu, pdu_len);
    i += pdu_len;
    uint8_t le = (uint8_t)(i - 4); /* DA..end of PDU */
    out[0] = PB_SD2;
    out[1] = le;
    out[2] = le;
    out[3] = PB_SD2;
    out[i] = pb_fcs(&out[4], le);
    out[i + 1] = PB_ED;
    return i + 2;
}

/* Try to parse one complete frame out of the accumulation buffer. Handles
 * TCP fragmentation: returns 0 when more bytes are needed (never assumes one
 * recv == one frame), 1 when a frame was extracted, skipping garbage bytes. */
static int pb_try_parse(pb_state_t *st, pb_frame_t *f) {
    while (st->rx_len > 0) {
        uint8_t sd = st->rx[0];
        size_t consume = 1; /* default: skip unknown byte */

        if (sd == PB_SC) {
            memset(f, 0, sizeof *f);
            f->kind = PB_SC;
            memmove(st->rx, st->rx + 1, --st->rx_len);
            return 1;
        }

        if (sd == PB_SD1) {
            if (st->rx_len < 6)
                return 0;
            if (st->rx[5] == PB_ED &&
                pb_fcs(&st->rx[1], 3) == st->rx[4]) {
                memset(f, 0, sizeof *f);
                f->kind = PB_SD1;
                f->da = st->rx[1] & 0x7F;
                f->sa = st->rx[2] & 0x7F;
                f->fc = st->rx[3];
                f->dsap = f->ssap = -1;
                st->rx_len -= 6;
                memmove(st->rx, st->rx + 6, st->rx_len);
                return 1;
            }
            /* corrupt SD1: fall through and skip one byte */
        } else if (sd == PB_SD2) {
            if (st->rx_len < 4)
                return 0;
            uint8_t le = st->rx[1];
            if (st->rx[2] == le && st->rx[3] == PB_SD2 && le >= 3) {
                size_t total = 4 + (size_t)le + 2;
                if (st->rx_len < total)
                    return 0;
                if (st->rx[4 + le + 1] == PB_ED &&
                    pb_fcs(&st->rx[4], le) == st->rx[4 + le]) {
                    memset(f, 0, sizeof *f);
                    f->kind = PB_SD2;
                    uint8_t da_raw = st->rx[4];
                    uint8_t sa_raw = st->rx[5];
                    f->da = da_raw & 0x7F;
                    f->sa = sa_raw & 0x7F;
                    f->fc = st->rx[6];
                    f->dsap = f->ssap = -1;
                    size_t p = 7;
                    if (da_raw & 0x80)
                        f->dsap = st->rx[p++];
                    if (sa_raw & 0x80)
                        f->ssap = st->rx[p++];
                    size_t end = 4 + le;
                    f->pdu_len = end > p ? end - p : 0;
                    memcpy(f->pdu, &st->rx[p], f->pdu_len);
                    st->rx_len -= total;
                    memmove(st->rx, st->rx + total, st->rx_len);
                    return 1;
                }
                /* bad FCS/ED: skip the SD byte and resync */
            } else if (st->rx[2] != le || st->rx[3] != PB_SD2) {
                /* header mismatch: skip and resync */
            } else {
                return 0;
            }
        }

        st->rx_len -= consume;
        memmove(st->rx, st->rx + consume, st->rx_len);
    }
    return 0;
}

/* ---- socket I/O (bus thread only) ----------------------------------------- */

static int pb_tcp_connect(pb_state_t *st, char *err, size_t errlen) {
    char portstr[16];
    snprintf(portstr, sizeof portstr, "%d", st->port);
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    int rc = getaddrinfo(st->host, portstr, &hints, &res);
    if (rc != 0) {
        snprintf(err, errlen, "resolve %s: %s", st->host, gai_strerror(rc));
        return -1;
    }
    int fd = -1;
    for (struct addrinfo *ai = res; ai; ai = ai->ai_next) {
        fd = socket(ai->ai_family, ai->ai_socktype, ai->ai_protocol);
        if (fd < 0)
            continue;
        if (connect(fd, ai->ai_addr, ai->ai_addrlen) == 0)
            break;
        close(fd);
        fd = -1;
    }
    freeaddrinfo(res);
    if (fd < 0) {
        snprintf(err, errlen, "connect %s:%d: %s", st->host, st->port,
                 strerror(errno));
        return -1;
    }
    int one = 1; /* frames are latency-sensitive request/response: no Nagle */
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    return fd;
}

static int pb_send_all(pb_state_t *st, const uint8_t *buf, size_t len) {
    size_t off = 0;
    while (off < len) {
        ssize_t n = send(st->sock, buf + off, len - off, 0);
        if (n <= 0) {
            if (n < 0 && (errno == EINTR || errno == EAGAIN))
                continue;
            return -1;
        }
        off += (size_t)n;
    }
    return 0;
}

/* Read one frame within timeout_ms. Returns 1 frame, 0 timeout, -1 socket
 * dead. */
static int pb_recv_frame(pb_state_t *st, pb_frame_t *f, int timeout_ms) {
    double deadline = mono_ms() + timeout_ms;
    for (;;) {
        if (pb_try_parse(st, f))
            return 1;
        double left = deadline - mono_ms();
        if (left <= 0)
            return 0;
        struct pollfd pfd = {.fd = st->sock, .events = POLLIN};
        int pr = poll(&pfd, 1, (int)left);
        if (pr < 0) {
            if (errno == EINTR)
                continue;
            return -1;
        }
        if (pr == 0)
            return 0;
        uint8_t tmp[256];
        ssize_t n = recv(st->sock, tmp, sizeof tmp, 0);
        if (n == 0)
            return -1; /* peer closed */
        if (n < 0) {
            if (errno == EINTR || errno == EAGAIN)
                continue;
            return -1;
        }
        if (st->rx_len + (size_t)n > sizeof st->rx)
            st->rx_len = 0; /* overflow: drop and resync */
        memcpy(st->rx + st->rx_len, tmp, (size_t)n);
        st->rx_len += (size_t)n;
    }
}

/* One request/response transaction with retries. expect_sc: the slave answers
 * with a short ack; otherwise an SD2 from the slave's station is awaited.
 * Returns 0 ok (resp filled unless SC), -1 socket dead, -2 no valid reply. */
static int pb_transact(pb_state_t *st, pb_slave_t *sl, int dsap, int ssap,
                       const uint8_t *pdu, size_t pdu_len, bool expect_sc,
                       pb_frame_t *resp) {
    uint8_t frame[PB_MAX_FRAME];
    size_t flen = pb_build_sd2(frame, sl->station, st->master_address,
                               PB_FC_SRD, dsap, ssap, pdu, pdu_len);
    for (int attempt = 0; attempt < PB_RETRIES; attempt++) {
        if (pb_send_all(st, frame, flen) != 0)
            return -1;
        double deadline = mono_ms() + PB_RESP_TIMEOUT_MS;
        for (;;) {
            int left = (int)(deadline - mono_ms());
            if (left <= 0)
                break;
            pb_frame_t f;
            int rc = pb_recv_frame(st, &f, left);
            if (rc < 0)
                return -1;
            if (rc == 0)
                break;
            if (expect_sc && f.kind == PB_SC)
                return 0;
            if (!expect_sc && f.kind == PB_SD2 && f.sa == sl->station) {
                if (resp)
                    *resp = f;
                return 0;
            }
            /* unrelated frame (stale reply, other station): keep waiting */
        }
    }
    return -2;
}

/* ---- DP state machine (bus thread) ----------------------------------------- */

/* Derive a standard identifier-format Chk_Cfg payload from the I/O sizes:
 * 0x1L = input module of L+1 bytes, 0x2L = output module (the sim accepts any
 * payload; real DP-V0 slaves want their GSD's config_bytes instead). */
static size_t pb_derive_cfg(uint8_t *out, size_t in_len, size_t out_len) {
    size_t n = 0;
    for (size_t rem = in_len; rem > 0 && n < PB_MAX_CFG;) {
        size_t take = rem > 16 ? 16 : rem;
        out[n++] = (uint8_t)(0x10 | (take - 1));
        rem -= take;
    }
    for (size_t rem = out_len; rem > 0 && n < PB_MAX_CFG;) {
        size_t take = rem > 16 ? 16 : rem;
        out[n++] = (uint8_t)(0x20 | (take - 1));
        rem -= take;
    }
    return n;
}

static void pb_set_link(pb_state_t *st, pb_slave_t *sl, pb_link_t link,
                        const char *fault) {
    pthread_mutex_lock(&st->lock);
    sl->link = link;
    if (fault)
        snprintf(sl->fault, sizeof sl->fault, "%s", fault);
    else
        sl->fault[0] = '\0';
    pthread_mutex_unlock(&st->lock);
}

static int pb_exchange(pb_state_t *st, pb_slave_t *sl);

/* Bring one peripheral into data exchange: Diag -> Set_Prm -> Chk_Cfg ->
 * Diag. Returns 0 ok, -1 socket dead, -2 protocol failure (retried on the
 * next bus loop iteration). */
static int pb_init_slave(pb_state_t *st, pb_slave_t *sl) {
    pb_frame_t resp;
    int rc;

    /* 1. Slave_Diag: is anyone there, what state is it in. */
    rc = pb_transact(st, sl, PB_SAP_DIAG, PB_SAP_MASTER, NULL, 0, false,
                     &resp);
    if (rc != 0)
        return rc;
    if (resp.pdu_len < 6)
        return -2;

    /* 2. Set_Prm: standard 7-byte parameter header + user param bytes.
     * [station_status, WD1, WD2, min_Tsdr, ident_hi, ident_lo, group]. */
    uint8_t prm[7 + PB_MAX_CFG];
    prm[0] = 0x80; /* Lock_Req: bind the slave to this master */
    prm[1] = 0x00; /* watchdog disabled */
    prm[2] = 0x00;
    prm[3] = sl->max_tsdr ? (uint8_t)(sl->max_tsdr & 0xFF) : 11;
    prm[4] = (uint8_t)(sl->ident_number >> 8);
    prm[5] = (uint8_t)(sl->ident_number & 0xFF);
    prm[6] = 0x00; /* group ident */
    memcpy(&prm[7], sl->prm, sl->prm_len);
    rc = pb_transact(st, sl, PB_SAP_SET_PRM, PB_SAP_MASTER, prm,
                     7 + sl->prm_len, true, NULL);
    if (rc != 0)
        return rc;

    /* 3. Chk_Cfg: configured or derived identifier bytes. */
    rc = pb_transact(st, sl, PB_SAP_CHK_CFG, PB_SAP_MASTER, sl->cfg,
                     sl->cfg_len, true, NULL);
    if (rc != 0)
        return rc;

    /* 4. Diag again: the DP standard requires confirming readiness before
     * data exchange. Station-not-ready or a cfg/prm fault -> retry later. */
    rc = pb_transact(st, sl, PB_SAP_DIAG, PB_SAP_MASTER, NULL, 0, false,
                     &resp);
    if (rc != 0)
        return rc;
    if (resp.pdu_len < 6 || (resp.pdu[0] & 0x07) != 0)
        return -2;

    /* Run the first exchange before reporting data exchange, so a reader
     * woken by the link transition never sees the zero-initialised input
     * snapshot. */
    rc = pb_exchange(st, sl);
    if (rc != 0)
        return rc;

    pb_set_link(st, sl, PB_LINK_DX, NULL);
    return 0;
}

/* One Data_Exchange cycle: send the whole output buffer, snapshot the input
 * bytes from the reply. */
static int pb_exchange(pb_state_t *st, pb_slave_t *sl) {
    uint8_t out[PB_MAX_IO];
    pthread_mutex_lock(&st->lock);
    memcpy(out, sl->outputs, sl->output_len);
    pthread_mutex_unlock(&st->lock);

    pb_frame_t resp;
    int rc = pb_transact(st, sl, -1, -1, out, sl->output_len, false, &resp);
    if (rc != 0)
        return rc;

    pthread_mutex_lock(&st->lock);
    size_t n = resp.pdu_len < sl->input_len ? resp.pdu_len : sl->input_len;
    memcpy(sl->inputs, resp.pdu, n);
    pthread_mutex_unlock(&st->lock);
    return 0;
}

static void pb_fault_all(pb_state_t *st, const char *fault) {
    for (size_t i = 0; i < st->nslaves; i++)
        pb_set_link(st, st->slaves[i], PB_LINK_FAULT, fault);
}

static void *pb_bus_thread(void *arg) {
    pb_state_t *st = arg;
    double next_attempt = 0;

    while (!st->stop) {
        /* (Re)connect the TCP transport, backing off every ~2 s. */
        if (st->sock < 0) {
            if (mono_ms() < next_attempt) {
                ms_sleep(50);
                continue;
            }
            char err[TDOT_ERR_MAX];
            int fd = pb_tcp_connect(st, err, sizeof err);
            next_attempt = mono_ms() + PB_RECONNECT_MS;
            if (fd < 0) {
                pb_fault_all(st, err);
                continue;
            }
            st->sock = fd;
            st->rx_len = 0;
            pthread_mutex_lock(&st->lock);
            st->bus_up = true;
            pthread_mutex_unlock(&st->lock);
            /* force every peripheral back through Diag/Prm/Cfg */
            for (size_t i = 0; i < st->nslaves; i++)
                pb_set_link(st, st->slaves[i], PB_LINK_UNKNOWN, NULL);
        }

        bool sock_dead = false;
        for (size_t i = 0; i < st->nslaves && !st->stop; i++) {
            pb_slave_t *sl = st->slaves[i];
            pthread_mutex_lock(&st->lock);
            bool active = sl->active;
            pb_link_t link = sl->link;
            pthread_mutex_unlock(&st->lock);
            if (!active)
                continue;

            int rc = (link == PB_LINK_DX) ? pb_exchange(st, sl)
                                          : pb_init_slave(st, sl);
            if (rc == -1) {
                sock_dead = true;
                break;
            }
            if (rc == -2)
                pb_set_link(st, sl, PB_LINK_FAULT,
                            link == PB_LINK_DX
                                ? "data exchange timed out"
                                : "peripheral init (diag/prm/cfg) failed");
        }

        if (sock_dead) {
            close(st->sock);
            st->sock = -1;
            pthread_mutex_lock(&st->lock);
            st->bus_up = false;
            pthread_mutex_unlock(&st->lock);
            pb_fault_all(st, "bus connection lost");
            next_attempt = mono_ms() + PB_RECONNECT_MS;
            continue;
        }

        pthread_mutex_lock(&st->lock);
        st->cycle_count++;
        pthread_mutex_unlock(&st->lock);
        ms_sleep(PB_CYCLE_MS);
    }

    if (st->sock >= 0) {
        close(st->sock);
        st->sock = -1;
    }
    return NULL;
}

static void pb_stop_thread(pb_state_t *st) {
    if (!st->thread_running)
        return;
    st->stop = 1;
    pthread_join(st->thread, NULL);
    st->thread_running = false;
    st->stop = 0;
    pthread_mutex_lock(&st->lock);
    st->bus_up = false;
    pthread_mutex_unlock(&st->lock);
}

/* ---- configure ------------------------------------------------------------ */

static int parse_byte_array(toml_table_t *tab, const char *key, uint8_t *out,
                            size_t cap, size_t *len) {
    *len = 0;
    toml_array_t *arr = toml_array_in(tab, key);
    if (!arr)
        return 0;
    int n = toml_array_nelem(arr);
    if (n < 0 || (size_t)n > cap)
        return -1;
    for (int i = 0; i < n; i++) {
        toml_datum_t d = toml_int_at(arr, i);
        if (!d.ok || d.u.i < 0 || d.u.i > 255)
            return -1;
        out[i] = (uint8_t)d.u.i;
    }
    *len = (size_t)n;
    return 0;
}

static int configure(tdot_connector_t *self, tdot_config_t *cfg, char *err,
                     size_t errlen) {
    pb_state_t *st = self->state;
    toml_datum_t d;

    if (!cfg->connection) {
        snprintf(err, errlen, "[connection] with a port is required");
        return -1;
    }
    d = toml_string_in(cfg->connection, "port");
    if (!d.ok) {
        snprintf(err, errlen, "[connection].port is required");
        return -1;
    }
    /* PoC transport: TCP byte stream only (no serial / pty phys). */
    if (strncmp(d.u.s, "tcp://", 6) != 0) {
        snprintf(err, errlen, "PoC supports tcp:// transport only");
        free(d.u.s);
        return -1;
    }
    const char *hp = d.u.s + 6;
    const char *colon = strrchr(hp, ':');
    if (!colon || colon == hp || atoi(colon + 1) <= 0) {
        snprintf(err, errlen, "port must be tcp://host:port, got '%s'",
                 d.u.s);
        free(d.u.s);
        return -1;
    }
    size_t hlen = (size_t)(colon - hp);
    if (hlen >= sizeof st->host)
        hlen = sizeof st->host - 1;
    memcpy(st->host, hp, hlen);
    st->host[hlen] = '\0';
    st->port = atoi(colon + 1);
    free(d.u.s);

    /* baudrate / slot_bits are line-level FDL timing parameters — parsed and
     * ignored over TCP (the device server owns the serial line timing). */
    st->master_address = 2;
    if ((d = toml_int_in(cfg->connection, "master_address")).ok)
        st->master_address = (uint8_t)d.u.i;

    if (cfg->ndevices > PB_MAX_DEV) {
        snprintf(err, errlen, "PoC supports at most %d devices", PB_MAX_DEV);
        return -1;
    }

    st->nslaves = 0;
    for (size_t i = 0; i < cfg->ndevices; i++) {
        tdot_device_t *dev = &cfg->devices[i];
        toml_table_t *pa = dev->protocol_address;
        pb_slave_t *sl = calloc(1, sizeof *sl);
        dev->proto = sl;
        st->slaves[st->nslaves++] = sl;

        d = toml_int_in(pa, "station_address");
        if (!d.ok || d.u.i < 1 || d.u.i > 125) {
            snprintf(err, errlen,
                     "device %s: station_address (1-125) is required",
                     dev->name);
            return -1;
        }
        sl->station = (uint8_t)d.u.i;
        if ((d = toml_int_in(pa, "ident_number")).ok)
            sl->ident_number = (uint16_t)d.u.i;
        if ((d = toml_int_in(pa, "max_tsdr")).ok)
            sl->max_tsdr = (uint16_t)d.u.i;

        toml_datum_t ib = toml_int_in(pa, "input_bytes");
        toml_datum_t ob = toml_int_in(pa, "output_bytes");
        if (!ib.ok || !ob.ok || ib.u.i < 0 || ob.u.i < 0 ||
            ib.u.i > PB_MAX_IO || ob.u.i > PB_MAX_IO) {
            snprintf(err, errlen,
                     "device %s: input_bytes and output_bytes (0-%d) are "
                     "required",
                     dev->name, PB_MAX_IO);
            return -1;
        }
        sl->input_len = (size_t)ib.u.i;
        sl->output_len = (size_t)ob.u.i;

        if (parse_byte_array(pa, "config_bytes", sl->cfg, sizeof sl->cfg,
                             &sl->cfg_len) != 0 ||
            parse_byte_array(pa, "param_bytes", sl->prm, sizeof sl->prm,
                             &sl->prm_len) != 0) {
            snprintf(err, errlen,
                     "device %s: config_bytes/param_bytes must be arrays of "
                     "0-255 (max %d entries)",
                     dev->name, PB_MAX_CFG);
            return -1;
        }
        if (sl->cfg_len == 0)
            sl->cfg_len =
                pb_derive_cfg(sl->cfg, sl->input_len, sl->output_len);

        for (size_t j = 0; j < dev->npoints; j++) {
            tdot_point_t *pt = &dev->points[j];
            pb_point_t *pp = calloc(1, sizeof *pp);
            pt->proto = pp;

            d = toml_string_in(pt->address, "direction");
            if (!d.ok) {
                snprintf(err, errlen,
                         "point %s/%s: address requires direction",
                         dev->name, pt->id);
                return -1;
            }
            if (strcmp(d.u.s, "input") == 0) {
                pp->is_input = true;
            } else if (strcmp(d.u.s, "output") == 0) {
                pp->is_input = false;
            } else {
                snprintf(err, errlen, "point %s/%s: unknown direction '%s'",
                         dev->name, pt->id, d.u.s);
                free(d.u.s);
                return -1;
            }
            free(d.u.s);

            d = toml_int_in(pt->address, "byte_offset");
            if (!d.ok || d.u.i < 0) {
                snprintf(err, errlen,
                         "point %s/%s: address requires byte_offset",
                         dev->name, pt->id);
                return -1;
            }
            pp->byte_offset = (size_t)d.u.i;

            pp->bit_offset = -1;
            pp->bit_count = 1;
            if ((d = toml_int_in(pt->address, "bit_offset")).ok) {
                if (d.u.i < 0 || d.u.i > 7) {
                    snprintf(err, errlen,
                             "point %s/%s: bit_offset must be 0-7",
                             dev->name, pt->id);
                    return -1;
                }
                pp->bit_offset = (int)d.u.i;
                if ((d = toml_int_in(pt->address, "bit_count")).ok) {
                    if (d.u.i < 1 || d.u.i + pp->bit_offset > 8) {
                        snprintf(err, errlen,
                                 "point %s/%s: bit_count must fit the byte",
                                 dev->name, pt->id);
                        return -1;
                    }
                    pp->bit_count = (int)d.u.i;
                }
            }

            if ((pt->access & TDOT_ACCESS_WRITE) && pp->is_input) {
                snprintf(err, errlen,
                         "point %s/%s: writable but direction is 'input'",
                         dev->name, pt->id);
                return -1;
            }

            char addr[128];
            if (pp->bit_offset >= 0)
                snprintf(addr, sizeof addr,
                         "{\"direction\":\"%s\",\"byte_offset\":%zu,"
                         "\"bit_offset\":%d,\"bit_count\":%d}",
                         pp->is_input ? "input" : "output", pp->byte_offset,
                         pp->bit_offset, pp->bit_count);
            else
                snprintf(addr, sizeof addr,
                         "{\"direction\":\"%s\",\"byte_offset\":%zu}",
                         pp->is_input ? "input" : "output", pp->byte_offset);
            pt->addr_json = strdup(addr);
        }
    }
    return 0;
}

/* ---- connect / disconnect -------------------------------------------------- */

static int connect_device(tdot_connector_t *self, tdot_device_t *dev,
                          char *err, size_t errlen) {
    pb_state_t *st = self->state;
    pb_slave_t *sl = dev->proto;

    pthread_mutex_lock(&st->lock);
    sl->active = true;
    pthread_mutex_unlock(&st->lock);

    /* One bus thread for the whole connector; started on first connect. */
    if (!st->thread_running) {
        st->stop = 0;
        st->sock = -1;
        st->rx_len = 0;
        if (pthread_create(&st->thread, NULL, pb_bus_thread, st) != 0) {
            snprintf(err, errlen, "failed to start bus thread: %s",
                     strerror(errno));
            return -1;
        }
        st->thread_running = true;
    }

    /* Wait for the peripheral to come through Diag/Prm/Cfg into DX. */
    double deadline = mono_ms() + PB_CONNECT_WAIT_MS;
    char fault[TDOT_ERR_MAX] = "";
    while (mono_ms() < deadline) {
        pthread_mutex_lock(&st->lock);
        pb_link_t link = sl->link;
        snprintf(fault, sizeof fault, "%s", sl->fault);
        pthread_mutex_unlock(&st->lock);
        if (link == PB_LINK_DX)
            return 0;
        ms_sleep(50);
    }
    snprintf(err, errlen, "station %u did not reach data exchange in %d s%s%s",
             sl->station, PB_CONNECT_WAIT_MS / 1000, fault[0] ? ": " : "",
             fault);
    return -1;
}

static void disconnect_device(tdot_connector_t *self, tdot_device_t *dev) {
    pb_state_t *st = self->state;
    pb_slave_t *sl = dev->proto;
    if (!sl)
        return;

    pthread_mutex_lock(&st->lock);
    sl->active = false;
    sl->link = PB_LINK_UNKNOWN;
    bool any_active = false;
    for (size_t i = 0; i < st->nslaves; i++)
        if (st->slaves[i]->active)
            any_active = true;
    pthread_mutex_unlock(&st->lock);

    /* Last device out stops the bus thread (which closes the socket). */
    if (!any_active)
        pb_stop_thread(st);
}

/* ---- read / write ----------------------------------------------------------- */

static int read_point(tdot_connector_t *self, tdot_device_t *dev,
                      tdot_point_t *pt, tdot_sample_t *out) {
    pb_state_t *st = self->state;
    pb_slave_t *sl = dev->proto;
    pb_point_t *pp = pt->proto;

    if (!pp->is_input) {
        tdot_sample_bad(out, "point direction is output; not readable");
        return 0;
    }

    pthread_mutex_lock(&st->lock);
    pb_link_t link = sl->link;
    bool bus_up = st->bus_up;
    char fault[TDOT_ERR_MAX];
    snprintf(fault, sizeof fault, "%s", sl->fault);
    uint8_t snapshot[PB_MAX_IO];
    memcpy(snapshot, sl->inputs, sl->input_len);
    pthread_mutex_unlock(&st->lock);

    if (link != PB_LINK_DX) {
        if (link == PB_LINK_FAULT && fault[0])
            tdot_sample_bad(out, "peripheral fault: %s", fault);
        else
            tdot_sample_bad(out, "peripheral not in data exchange");
        /* only a dead transport should trigger the reconnect backoff */
        return bus_up ? 0 : -1;
    }

    /* Bit-level extraction: raw echoes the whole containing byte. */
    if (pp->bit_offset >= 0) {
        if (pp->byte_offset >= sl->input_len) {
            tdot_sample_bad(out, "byte_offset %zu out of range (%zu bytes)",
                            pp->byte_offset, sl->input_len);
            return 0;
        }
        uint8_t byte = snapshot[pp->byte_offset];
        out->raw[0] = byte;
        out->raw_len = 1;
        out->raw_group = 1;
        uint8_t mask = (uint8_t)((1u << pp->bit_count) - 1);
        uint8_t field = (uint8_t)((byte >> pp->bit_offset) & mask);
        if (pt->datatype == TDOT_DT_BOOL || pp->bit_count == 1) {
            out->value.kind = TDOT_VAL_BOOL;
            out->value.b = field != 0;
        } else {
            out->value.kind = TDOT_VAL_NUM;
            out->value.num = (double)field;
            if (pt->has_transform)
                out->value.num =
                    tdot_transform_apply(&pt->transform, out->value.num);
        }
        return 0;
    }

    size_t len = tdot_datatype_len(pt->datatype);
    if (len == 0) {
        tdot_sample_bad(out, "point needs a fixed-length datatype");
        return 0;
    }
    if (pp->byte_offset + len > sl->input_len) {
        tdot_sample_bad(out,
                        "point needs bytes %zu..%zu but input buffer is %zu "
                        "bytes",
                        pp->byte_offset, pp->byte_offset + len,
                        sl->input_len);
        return 0;
    }

    memcpy(out->raw, &snapshot[pp->byte_offset], len);
    out->raw_len = len;
    out->raw_group = 1;

    char err[TDOT_ERR_MAX];
    if (tdot_decode(pt->datatype, out->raw, len, pt->endianness,
                    pt->word_order, &out->value, err, sizeof err) != 0) {
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
    pb_state_t *st = self->state;
    pb_slave_t *sl = dev->proto;
    pb_point_t *pp = pt->proto;

    if (pp->is_input) {
        snprintf(err, errlen, "point direction is input; not writable");
        return -1;
    }
    if (!st->thread_running) {
        snprintf(err, errlen, "device not connected");
        return -1;
    }

    pthread_mutex_lock(&st->lock);

    if (pp->bit_offset >= 0) {
        /* bitfield: read-modify-write of the containing output byte */
        if (pp->byte_offset >= sl->output_len) {
            pthread_mutex_unlock(&st->lock);
            snprintf(err, errlen, "byte_offset %zu out of range (%zu bytes)",
                     pp->byte_offset, sl->output_len);
            return -1;
        }
        uint64_t field = value->kind == TDOT_VAL_BOOL ? (uint64_t)value->b
                         : value->kind == TDOT_VAL_NUM
                             ? (uint64_t)value->num
                             : (uint64_t)strtoull(value->str, NULL, 10);
        uint8_t mask =
            (uint8_t)(((1u << pp->bit_count) - 1) << pp->bit_offset);
        sl->outputs[pp->byte_offset] =
            (uint8_t)((sl->outputs[pp->byte_offset] & ~mask) |
                      (((uint8_t)field << pp->bit_offset) & mask));
    } else {
        uint8_t bytes[16];
        size_t len = sizeof bytes;
        if (tdot_encode(pt->datatype, value, pt->endianness, pt->word_order,
                        bytes, &len, err, errlen) != 0) {
            pthread_mutex_unlock(&st->lock);
            return -1;
        }
        if (pp->byte_offset + len > sl->output_len) {
            pthread_mutex_unlock(&st->lock);
            snprintf(err, errlen,
                     "point needs bytes %zu..%zu but output buffer is %zu "
                     "bytes",
                     pp->byte_offset, pp->byte_offset + len, sl->output_len);
            return -1;
        }
        memcpy(&sl->outputs[pp->byte_offset], bytes, len);
    }

    uint64_t start = st->cycle_count;
    bool in_dx = sl->link == PB_LINK_DX;
    pthread_mutex_unlock(&st->lock);

    /* The write already succeeded (Rust semantics: buffer updated, the bus
     * thread sends it every cycle). Best-effort: wait for two more cycles so
     * a one-shot CLI write is actually flushed before disconnect. */
    if (in_dx) {
        double deadline = mono_ms() + 1000;
        while (mono_ms() < deadline) {
            pthread_mutex_lock(&st->lock);
            uint64_t cc = st->cycle_count;
            pthread_mutex_unlock(&st->lock);
            if (cc >= start + 2)
                break;
            ms_sleep(10);
        }
    }
    return 0;
}

/* ---- lifecycle -------------------------------------------------------------- */

static void destroy(tdot_connector_t *self) {
    pb_state_t *st = self->state;
    if (st) {
        pb_stop_thread(st); /* safety: normally stopped by last disconnect */
        pthread_mutex_destroy(&st->lock);
        free(st);
    }
    free(self);
}

tdot_connector_t *tdot_connector_profibus_new(void) {
    tdot_connector_t *c = calloc(1, sizeof *c);
    pb_state_t *st = calloc(1, sizeof *st);
    pthread_mutex_init(&st->lock, NULL);
    st->sock = -1;
    c->protocol = "profibus";
    c->capabilities_json = CAPABILITIES;
    c->state = st;
    c->configure = configure;
    c->connect_device = connect_device;
    c->read_point = read_point;
    c->write_point = write_point;
    c->disconnect_device = disconnect_device;
    c->destroy = destroy;
    return c;
}
