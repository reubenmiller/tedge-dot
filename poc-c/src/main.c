/* tedge-dot C PoC — CLI entry point.
 *
 *   tedge-dot read  -c <config> [-d <device-glob>] [-p <point-glob>]...
 *                   [--poll] [--interval 1s] [--count N] [--json]
 *   tedge-dot write -c <config> -d <device> -p <point> --value <v>
 *   tedge-dot run   -c <config> [--output stdout|mqtt] [--duration 10s]
 */
#include <fnmatch.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "tedge_dot/connector.h"
#include "tedge_dot/decode.h"
#include "tedge_dot/runtime.h"

static void usage(void) {
    fputs(
        "tedge-dot (C PoC) — OT protocol connectors for thin-edge.io\n"
        "\n"
        "USAGE:\n"
        "  tedge-dot read  -c <config> [-d <device>] [-p <point>]... "
        "[--poll] [--interval <dur>] [--count <n>] [--json]\n"
        "  tedge-dot write -c <config> -d <device> -p <point> --value <v>\n"
        "  tedge-dot run   -c <config> [--output stdout|mqtt] "
        "[--duration <dur>]\n",
        stderr);
}

static volatile sig_atomic_t g_stop = 0;
static void on_signal(int sig) {
    (void)sig;
    g_stop = 1;
}

typedef struct {
    const char *config;
    const char *device;
    const char *points[16];
    int npoints;
    const char *value;
    const char *output;
    double interval_s;
    double duration_s;
    int count;
    bool poll;
    bool json;
} args_t;

static int parse_args(int argc, char **argv, args_t *a) {
    memset(a, 0, sizeof *a);
    a->interval_s = 1.0;
    a->output = "mqtt";
    for (int i = 2; i < argc; i++) {
        const char *arg = argv[i];
        const char *next = (i + 1 < argc) ? argv[i + 1] : NULL;
        if ((!strcmp(arg, "-c") || !strcmp(arg, "--config")) && next)
            a->config = argv[++i];
        else if ((!strcmp(arg, "-d") || !strcmp(arg, "--device")) && next)
            a->device = argv[++i];
        else if ((!strcmp(arg, "-p") || !strcmp(arg, "--point")) && next) {
            if (a->npoints < 16)
                a->points[a->npoints++] = argv[++i];
        } else if (!strcmp(arg, "--value") && next)
            a->value = argv[++i];
        else if (!strcmp(arg, "--output") && next)
            a->output = argv[++i];
        else if (!strcmp(arg, "--interval") && next)
            a->interval_s = tdot_duration_parse(argv[++i]);
        else if (!strcmp(arg, "--duration") && next)
            a->duration_s = tdot_duration_parse(argv[++i]);
        else if (!strcmp(arg, "--count") && next)
            a->count = atoi(argv[++i]);
        else if (!strcmp(arg, "--poll"))
            a->poll = true;
        else if (!strcmp(arg, "--json"))
            a->json = true;
        else if (arg[0] != '-' && !a->config)
            a->config = arg; /* positional config path */
        else {
            fprintf(stderr, "unknown argument: %s\n", arg);
            return -1;
        }
    }
    if (!a->config) {
        fputs("missing --config\n", stderr);
        return -1;
    }
    return 0;
}

static bool point_matches(const args_t *a, const tdot_point_t *pt) {
    if (a->npoints == 0)
        return true;
    for (int i = 0; i < a->npoints; i++)
        if (fnmatch(a->points[i], pt->id, 0) == 0)
            return true;
    return false;
}

static bool device_matches(const args_t *a, const tdot_device_t *dev) {
    return !a->device || fnmatch(a->device, dev->name, 0) == 0;
}

/* Load config + build connector + configure. */
static int setup(const args_t *a, tdot_config_t **cfg,
                 tdot_connector_t **conn) {
    char err[256];
    *cfg = tdot_config_load(a->config, err, sizeof err);
    if (!*cfg) {
        fprintf(stderr, "error: %s\n", err);
        return -1;
    }
    *conn = tdot_connector_factory((*cfg)->protocol);
    if (!*conn) {
        fprintf(stderr, "error: unknown protocol '%s'\n", (*cfg)->protocol);
        tdot_config_free(*cfg);
        return -1;
    }
    return 0;
}

static void print_sample(const args_t *a, tdot_config_t *cfg,
                         tdot_device_t *dev, tdot_point_t *pt,
                         const tdot_sample_t *s) {
    pt->seq++;
    if (a->json) {
        char *json = tdot_envelope_sample(cfg, dev, pt, s);
        puts(json);
        free(json);
        return;
    }
    char hex[TDOT_RAW_MAX * 3 + 1];
    tdot_hex_format(s->raw, s->raw_len, s->raw_group, hex, sizeof hex);
    if (s->quality == TDOT_Q_BAD) {
        printf("%-8s %-14s quality=bad  error=%s\n", dev->name, pt->id,
               s->error);
        return;
    }
    char val[80] = "-";
    switch (s->value.kind) {
    case TDOT_VAL_BOOL:
        snprintf(val, sizeof val, "%s", s->value.b ? "true" : "false");
        break;
    case TDOT_VAL_NUM:
        snprintf(val, sizeof val, "%g", s->value.num);
        break;
    case TDOT_VAL_STR:
        snprintf(val, sizeof val, "\"%s\"", s->value.str);
        break;
    default:
        break;
    }
    printf("%-8s %-14s %-12s %s%s%s (raw: %s)\n", dev->name, pt->id, val,
           pt->unit ? "" : "", pt->unit ? pt->unit : "",
           pt->unit ? " " : "", hex);
}

static int cmd_read(const args_t *a) {
    tdot_config_t *cfg;
    tdot_connector_t *conn;
    char err[256];
    if (setup(a, &cfg, &conn) != 0)
        return 1;
    if (conn->configure(conn, cfg, err, sizeof err) != 0) {
        fprintf(stderr, "error: %s\n", err);
        return 1;
    }

    struct sigaction sa = {.sa_handler = on_signal};
    sigaction(SIGINT, &sa, NULL);

    int exit_code = 0;
    bool any_matched = false;
    for (size_t i = 0; i < cfg->ndevices; i++) {
        tdot_device_t *dev = &cfg->devices[i];
        if (!device_matches(a, dev))
            continue;
        bool has_point = false;
        for (size_t j = 0; j < dev->npoints; j++)
            if (point_matches(a, &dev->points[j]) &&
                (dev->points[j].access & TDOT_ACCESS_READ))
                has_point = true;
        if (!has_point)
            continue;
        any_matched = true;
        if (conn->connect_device(conn, dev, err, sizeof err) != 0) {
            fprintf(stderr, "error: device %s: %s\n", dev->name, err);
            exit_code = 1;
            continue;
        }
    }
    if (!any_matched) {
        fprintf(stderr, "error: no matching readable points\n");
        return 1;
    }

    int rounds = 0;
    do {
        for (size_t i = 0; i < cfg->ndevices && !g_stop; i++) {
            tdot_device_t *dev = &cfg->devices[i];
            if (!device_matches(a, dev) || !dev->proto)
                continue;
            for (size_t j = 0; j < dev->npoints; j++) {
                tdot_point_t *pt = &dev->points[j];
                if (!point_matches(a, pt) ||
                    !(pt->access & TDOT_ACCESS_READ))
                    continue;
                tdot_sample_t s;
                tdot_sample_init(&s);
                conn->read_point(conn, dev, pt, &s);
                print_sample(a, cfg, dev, pt, &s);
                if (s.quality == TDOT_Q_BAD)
                    exit_code = 1;
            }
        }
        rounds++;
        if (a->poll && !g_stop && (a->count == 0 || rounds < a->count)) {
            struct timespec ts = {
                .tv_sec = (time_t)a->interval_s,
                .tv_nsec = (long)((a->interval_s -
                                   (double)(time_t)a->interval_s) *
                                  1e9)};
            nanosleep(&ts, NULL);
        }
    } while (a->poll && !g_stop && (a->count == 0 || rounds < a->count));

    for (size_t i = 0; i < cfg->ndevices; i++)
        conn->disconnect_device(conn, &cfg->devices[i]);
    conn->destroy(conn);
    tdot_config_free(cfg);
    return exit_code;
}

static void parse_value(const char *s, tdot_value_t *v) {
    memset(v, 0, sizeof *v);
    if (!strcmp(s, "true") || !strcmp(s, "false")) {
        v->kind = TDOT_VAL_BOOL;
        v->b = !strcmp(s, "true");
        return;
    }
    char *end = NULL;
    double num = strtod(s, &end);
    if (end && *end == '\0' && end != s) {
        v->kind = TDOT_VAL_NUM;
        v->num = num;
        return;
    }
    v->kind = TDOT_VAL_STR;
    snprintf(v->str, sizeof v->str, "%s", s);
}

static int cmd_write(const args_t *a) {
    if (!a->device || a->npoints != 1 || !a->value) {
        fputs("write requires -d <device> -p <point> --value <v>\n", stderr);
        return 1;
    }
    tdot_config_t *cfg;
    tdot_connector_t *conn;
    char err[256];
    if (setup(a, &cfg, &conn) != 0)
        return 1;
    if (conn->configure(conn, cfg, err, sizeof err) != 0) {
        fprintf(stderr, "error: %s\n", err);
        return 1;
    }
    tdot_device_t *dev = tdot_config_device(cfg, a->device);
    if (!dev) {
        fprintf(stderr, "error: unknown device: %s\n", a->device);
        return 1;
    }
    tdot_point_t *pt = tdot_device_point(dev, a->points[0]);
    if (!pt) {
        fprintf(stderr, "error: unknown point: %s\n", a->points[0]);
        return 1;
    }
    if (!(pt->access & TDOT_ACCESS_WRITE)) {
        fprintf(stderr, "error: point %s is not writable\n", pt->id);
        return 1;
    }
    if (conn->connect_device(conn, dev, err, sizeof err) != 0) {
        fprintf(stderr, "error: device %s: %s\n", dev->name, err);
        return 1;
    }
    tdot_value_t value;
    parse_value(a->value, &value);
    int rc = conn->write_point(conn, dev, pt, &value, err, sizeof err);
    if (rc != 0)
        fprintf(stderr, "error: write %s/%s: %s\n", dev->name, pt->id, err);
    else
        printf("wrote %s/%s = %s\n", dev->name, pt->id, a->value);
    conn->disconnect_device(conn, dev);
    conn->destroy(conn);
    tdot_config_free(cfg);
    return rc == 0 ? 0 : 1;
}

static int cmd_run(const args_t *a) {
    tdot_config_t *cfg;
    tdot_connector_t *conn;
    if (setup(a, &cfg, &conn) != 0)
        return 1;
    tdot_run_opts_t opts = {
        .output = strcmp(a->output, "stdout") == 0 ? TDOT_OUTPUT_STDOUT
                                                   : TDOT_OUTPUT_MQTT,
        .duration_s = a->duration_s,
    };
    int rc = tdot_runtime_run(conn, cfg, &opts);
    conn->destroy(conn);
    tdot_config_free(cfg);
    return rc == 0 ? 0 : 1;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        usage();
        return 2;
    }
    args_t a;
    if (parse_args(argc, argv, &a) != 0) {
        usage();
        return 2;
    }
    if (!strcmp(argv[1], "read"))
        return cmd_read(&a);
    if (!strcmp(argv[1], "write"))
        return cmd_write(&a);
    if (!strcmp(argv[1], "run"))
        return cmd_run(&a);
    usage();
    return 2;
}
