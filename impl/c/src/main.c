/* tedge-dot — CLI entry point (C implementation).
 *
 *   tedge-dot read  -c <config> [-d <device-glob>] [-p <point-glob>]...
 *                   [--poll] [--interval 1s] [--count N] [--json]
 *   tedge-dot write -c <config> -d <device> -p <point> --value <v>
 *   tedge-dot run   -c <config> [--output stdout|mqtt] [--duration 10s]
 *   tedge-dot describe [-c <config>] [-d <device-glob>] [--set <name>]
 *                      [--format c8y-dtm] [--compact]
 */
#include <ctype.h>
#include <dirent.h>
#include <fnmatch.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include "tedge_dot/connector.h"
#include "tedge_dot/decode.h"
#include "tedge_dot/descriptor.h"
#include "tedge_dot/runtime.h"

static void usage(void) {
    fputs(
        "tedge-dot (C) — OT protocol connectors for thin-edge.io\n"
        "\n"
        "USAGE:\n"
        "  tedge-dot read  -c <config> [-d <device>] [-p <point>]... "
        "[--poll] [--interval <dur>] [--count <n>] [--json]\n"
        "  tedge-dot write -c <config> -d <device> -p <point> --value <v>\n"
        "  tedge-dot run   -c <config> [--output stdout|mqtt] "
        "[--duration <dur>]\n"
        "  tedge-dot describe [-c <config-or-dir>]... [-d <device>] "
        "[--set <name>] [--format c8y-dtm] [--compact]\n"
        "      (default: every config in /etc/tedge/plugins/ot)\n"
        "  tedge-dot <config-or-dir> [run options]      (same as run)\n"
        "  tedge-dot --version\n",
        stderr);
}

/* Same default config directory as the Rust binary: the one the packaged
 * service runs. */
#define DEFAULT_CONFIG_DIR "/etc/tedge/plugins/ot"

static volatile sig_atomic_t g_stop = 0;
static void on_signal(int sig) {
    (void)sig;
    g_stop = 1;
}

typedef struct {
    const char *config;   /* the first of `configs` */
    const char **configs; /* every -c/--config and positional path, as many as
                             given (only `describe` takes more than one) */
    int nconfigs;
    int capconfigs;
    const char *device;
    const char *points[16];
    int npoints;
    const char *value;
    const char *output;
    const char *format;
    const char *set;
    double interval_s;
    double duration_s;
    int count;
    bool poll;
    bool json;
    bool compact;
} args_t;

static int add_config(args_t *a, const char *path) {
    if (a->nconfigs == a->capconfigs) {
        int cap = a->capconfigs ? a->capconfigs * 2 : 4;
        const char **grown = realloc(a->configs, (size_t)cap * sizeof *grown);
        if (!grown) {
            fputs("out of memory\n", stderr);
            return -1;
        }
        a->configs = grown;
        a->capconfigs = cap;
    }
    a->configs[a->nconfigs++] = path;
    if (!a->config)
        a->config = path;
    return 0;
}

static int parse_args(int argc, char **argv, args_t *a) {
    memset(a, 0, sizeof *a);
    a->interval_s = 1.0;
    a->output = "mqtt";
    a->format = "c8y-dtm";
    for (int i = 2; i < argc; i++) {
        const char *arg = argv[i];
        const char *next = (i + 1 < argc) ? argv[i + 1] : NULL;
        if ((!strcmp(arg, "-c") || !strcmp(arg, "--config")) && next) {
            if (add_config(a, argv[++i]) != 0)
                return -1;
        } else if ((!strcmp(arg, "-d") || !strcmp(arg, "--device")) && next)
            a->device = argv[++i];
        else if ((!strcmp(arg, "-p") || !strcmp(arg, "--point")) && next) {
            if (a->npoints < 16)
                a->points[a->npoints++] = argv[++i];
        } else if (!strcmp(arg, "--value") && next)
            a->value = argv[++i];
        else if (!strcmp(arg, "--output") && next)
            a->output = argv[++i];
        else if (!strcmp(arg, "--format") && next)
            a->format = argv[++i];
        else if (!strcmp(arg, "--set") && next)
            a->set = argv[++i];
        else if (!strcmp(arg, "--compact"))
            a->compact = true;
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
        else if (arg[0] != '-') {
            if (add_config(a, arg) != 0) /* positional config path */
                return -1;
        } else {
            fprintf(stderr, "unknown argument: %s\n", arg);
            return -1;
        }
    }
    bool describe = !strcmp(argv[1], "describe");
    if (!a->config) {
        /* `describe` needs neither a device nor a broker, so — like the Rust
         * binary — it falls back to the directory the packaged service runs. */
        if (describe) {
            if (add_config(a, DEFAULT_CONFIG_DIR) != 0)
                return -1;
        } else {
            fputs("missing --config\n", stderr);
            return -1;
        }
    }
    /* Only describe renders several configs at once. Every other command acts on
     * one, and quietly taking one of several would act on the wrong file. */
    if (a->nconfigs > 1 && !describe) {
        fprintf(stderr, "%s takes a single config path\n", argv[1]);
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

static int cmp_str(const void *a, const void *b) {
    return strcmp(*(const char *const *)a, *(const char *const *)b);
}

static void free_paths(char **paths, size_t n) {
    for (size_t i = 0; i < n; i++)
        free(paths[i]);
    free(paths);
}

static void push_string(char ***list, size_t *n, size_t *cap, char *owned) {
    if (*n == *cap) {
        *cap = *cap ? *cap * 2 : 8;
        *list = realloc(*list, *cap * sizeof **list);
    }
    (*list)[(*n)++] = owned;
}

/* The config files found so far, each with the canonical path it resolves to:
 * the same file under two spellings (`dir` and `dir//a.toml`, or a symlink) is
 * one config, kept under the spelling it was first named by. */
typedef struct {
    char **paths, **keys;
    size_t n, cap, nkeys, capkeys;
} config_list_t;

static void config_list_push(config_list_t *l, const char *path) {
    char *key = realpath(path, NULL);
    if (!key)
        key = strdup(path);
    for (size_t i = 0; i < l->nkeys; i++)
        if (strcmp(l->keys[i], key) == 0) {
            free(key);
            return;
        }
    push_string(&l->keys, &l->nkeys, &l->capkeys, key);
    push_string(&l->paths, &l->n, &l->cap, strdup(path));
}

/* Expand config arguments into the files they name, by the same rules as the
 * Rust binary's discover_configs() (describe-parity.sh pins them): a directory
 * contributes its *.toml regular files in sorted order, anything else that
 * exists is taken as is (a file, or a pipe such as `-c <(generate-config)`),
 * and a file named twice is kept once. Returns 0 with an array the caller
 * releases with free_paths(), or -1 after printing why (a path that does not
 * exist, a directory that cannot be read). */
static int collect_configs(const char *const *args, size_t nargs,
                           char ***out, size_t *nout) {
    config_list_t list = {0};
    int rc = 0;
    for (size_t i = 0; i < nargs && rc == 0; i++) {
        struct stat st;
        if (stat(args[i], &st) != 0) {
            fprintf(stderr, "error: config path '%s' does not exist\n", args[i]);
            rc = -1;
            break;
        }
        if (!S_ISDIR(st.st_mode)) {
            config_list_push(&list, args[i]);
            continue;
        }
        DIR *d = opendir(args[i]);
        if (!d) {
            fprintf(stderr, "error: cannot read config directory %s\n", args[i]);
            rc = -1;
            break;
        }
        /* Without the trailing '/', so `dir/` and `dir` spell the same files. */
        int dirlen = (int)strlen(args[i]);
        while (dirlen > 1 && args[i][dirlen - 1] == '/')
            dirlen--;
        char **found = NULL;
        size_t nfound = 0, capfound = 0;
        struct dirent *e;
        while ((e = readdir(d))) {
            /* A name that is only `.toml` has no extension (a hidden file), as
             * Rust's Path::extension() sees it, so it is not a config. */
            const char *dot = strrchr(e->d_name, '.');
            if (!dot || dot == e->d_name || strcmp(dot, ".toml") != 0)
                continue;
            char full[1024];
            snprintf(full, sizeof full, "%.*s/%s", dirlen, args[i], e->d_name);
            struct stat fst;
            if (stat(full, &fst) != 0 || !S_ISREG(fst.st_mode))
                continue;
            push_string(&found, &nfound, &capfound, strdup(full));
        }
        closedir(d);
        qsort(found, nfound, sizeof *found, cmp_str); /* stable, predictable order */
        for (size_t j = 0; j < nfound; j++)
            config_list_push(&list, found[j]);
        free_paths(found, nfound);
    }
    free_paths(list.keys, list.nkeys);
    if (rc != 0) {
        free_paths(list.paths, list.n);
        return rc;
    }
    *out = list.paths;
    *nout = list.n;
    return 0;
}

/* Lists the configs a `run` argument names again, for a reload (SIGHUP), by
 * the rules it was listed by at startup. `ctx` is the argument. A path that is
 * gone is an error, which keeps the running connectors, as in the Rust build. */
static int rediscover_configs(void *ctx, char ***paths, size_t *npaths) {
    const char *arg = ctx;
    return collect_configs(&arg, 1, paths, npaths);
}

/* Run every config the argument names -- a directory's *.toml files, or the
 * one file -- each connector in its own thread of this one process (the
 * packaged systemd unit points ExecStart at /etc/tedge/plugins/ot). On SIGHUP
 * the runtime lists the argument again through `discover`, so connectors start
 * and stop as files come and go, and every running one re-reads its file. */
static int cmd_run(const args_t *a) {
    tdot_run_opts_t opts = {
        .output = strcmp(a->output, "stdout") == 0 ? TDOT_OUTPUT_STDOUT
                                                   : TDOT_OUTPUT_MQTT,
        .duration_s = a->duration_s,
        .discover = rediscover_configs,
        .discover_ctx = (void *)a->config, /* only ever read */
    };
    /* Files and directories only, as in the Rust build: a connector re-reads its
     * config -- on a reload, and to restart one -- which a pipe such as
     * `-c <(generate-config)` cannot provide twice. `describe` reads once, so it
     * takes pipes too. */
    struct stat st;
    if (a->config && stat(a->config, &st) == 0 && !S_ISDIR(st.st_mode) &&
        !S_ISREG(st.st_mode)) {
        fprintf(stderr,
                "error: config path '%s' is not a regular file; `run` re-reads its "
                "configs, so it needs files or directories\n",
                a->config);
        return 1;
    }
    char **paths = NULL;
    size_t n = 0;
    if (collect_configs(&a->config, 1, &paths, &n) != 0)
        return 1;
    if (n == 0) {
        fprintf(stderr, "error: no *.toml configs in %s\n", a->config);
        free_paths(paths, n);
        return 1;
    }
    int rc = tdot_runtime_run_configs((const char *const *)paths, n, &opts);
    free_paths(paths, n);
    return rc == 0 ? 0 : 1;
}

/* Render the Cumulocity DTM property definitions derived from every connector
 * configuration the arguments name (mirrors cmd_describe in src/main.rs).
 * Needs no device, broker or protocol module — only the config files.
 *
 * One service runs every config in its directory and a DTM identifier is
 * tenant-wide, so the definitions and the warnings about them are computed
 * across all of the configs: a set declared in several files is rendered once. */
static int cmd_describe(const args_t *a) {
    if (strcmp(a->format, "c8y-dtm") != 0) {
        fprintf(stderr, "error: unknown --format '%s' (expected c8y-dtm)\n",
                a->format);
        return 1;
    }
    char **paths = NULL;
    size_t npaths = 0;
    if (collect_configs(a->configs, (size_t)a->nconfigs, &paths, &npaths) != 0)
        return 1;
    if (npaths == 0) {
        fprintf(stderr, "error: no connector configs (*.toml) found in");
        for (int i = 0; i < a->nconfigs; i++)
            fprintf(stderr, "%s %s", i ? "," : "", a->configs[i]);
        fputc('\n', stderr);
        free_paths(paths, npaths);
        return 1;
    }

    int rc = 1;
    char *forced_owned = NULL;
    char err[256];
    tdot_config_t **cfgs = calloc(npaths, sizeof *cfgs);
    size_t *all = calloc(npaths, sizeof *all); /* each config's own ndevices */
    for (size_t c = 0; c < npaths; c++) {
        cfgs[c] = tdot_config_load(paths[c], err, sizeof err);
        if (!cfgs[c]) {
            fprintf(stderr, "error: %s\n", err);
            goto out;
        }
        all[c] = cfgs[c]->ndevices;
    }
    const tdot_config_t *const *view = (const tdot_config_t *const *)cfgs;

    /* Restrict to the matching devices by moving them to the front of each
     * config; ndevices is restored before the free so nothing leaks. */
    size_t kept = 0;
    for (size_t c = 0; c < npaths; c++) {
        tdot_config_t *cfg = cfgs[c];
        size_t keep = 0;
        for (size_t i = 0; i < all[c]; i++) {
            if (!device_matches(a, &cfg->devices[i]))
                continue;
            tdot_device_t tmp = cfg->devices[keep];
            cfg->devices[keep] = cfg->devices[i];
            cfg->devices[i] = tmp;
            keep++;
        }
        cfg->ndevices = keep;
        kept += keep;
    }
    if (kept == 0 && a->device) {
        fprintf(stderr, "error: no device matches '%s'\n", a->device);
        goto out;
    }

    /* Parameter ids become fragment keys on the device twin, so they must be
     * plain identifiers. */
    /* A blank --set means "none given", as an empty `default_set` does in the
     * flow: an unset variable in a provisioning script (--set "$PARAM_SET")
     * must not force every point into a nameless set. The Rust build applies
     * the same rule. */
    const char *forced = NULL;
    if (a->set) {
        /* Trimmed, not just tested for blankness: `--set "$(cat name.txt)"`
         * carries a trailing newline, and the Rust CLI trims the same way, so
         * the two must not disagree on a padded value either. */
        forced_owned = strdup(a->set);
        const char *start = forced_owned;
        while (*start && isspace((unsigned char)*start))
            start++;
        size_t end = strlen(start);
        while (end && isspace((unsigned char)start[end - 1]))
            end--;
        memmove(forced_owned, start, end);
        forced_owned[end] = '\0';
        forced = *forced_owned ? forced_owned : NULL;
    }

    char *bad = tdot_param_invalid_keys_across(view, npaths, forced);
    if (bad) {
        fprintf(stderr, "error: parameter keys must match [A-Za-z0-9_]: %s\n",
                bad);
        free(bad);
        goto out;
    }
    /* A fragment holds one value per key, and a key naming its set leaves no
     * room for `set` or `group`. */
    char *conflicts = tdot_param_key_conflicts_across(view, npaths, forced);
    if (conflicts) {
        fprintf(stderr, "error: conflicting parameter keys: %s\n", conflicts);
        free(conflicts);
        goto out;
    }

    /* A DTM identifier is tenant-wide, so a set named after the protocol is
     * shared with every other device type that speaks it. Declaring the device
     * type is what keeps them apart. */
    if (!forced) {
        char *collisions = tdot_param_type_warnings_across(view, npaths);
        if (collisions) {
            fprintf(stderr, "%s\n", collisions);
            free(collisions);
        }
        /* One untyped-device warning per protocol, in the order the protocols
         * first appear: such a device's sets are named after its protocol, so
         * that is what it collides with. */
        for (size_t c = 0; c < npaths; c++) {
            const char *protocol = cfgs[c]->protocol;
            bool seen = false;
            for (size_t p = 0; p < c && !seen; p++)
                seen = strcmp(cfgs[p]->protocol, protocol) == 0;
            if (seen)
                continue;
            char *untyped =
                tdot_param_untyped_devices_across(view, npaths, protocol);
            if (untyped) {
                fprintf(stderr,
                        "warning: device(s) %s declare no `type`, so their "
                        "parameter sets are named after the protocol ('%s_...') "
                        "and collide with every other %s device type in the "
                        "tenant; set `type` on the device or in its point "
                        "library\n",
                        untyped, protocol, protocol);
                free(untyped);
            }
        }
    }

    cJSON *docs = tdot_c8y_dtm_definitions_across(view, npaths, forced);
    if (a->compact) {
        cJSON *doc;
        cJSON_ArrayForEach(doc, docs) {
            char *line = cJSON_PrintUnformatted(doc);
            puts(line);
            free(line);
        }
    } else {
        char *out = cJSON_Print(docs);
        puts(out);
        free(out);
    }
    cJSON_Delete(docs);
    rc = 0;

out:
    free(forced_owned);
    for (size_t c = 0; c < npaths; c++) {
        if (!cfgs[c])
            continue; /* not loaded: an earlier config failed */
        cfgs[c]->ndevices = all[c];
        tdot_config_free(cfgs[c]);
    }
    free(cfgs);
    free(all);
    free_paths(paths, npaths);
    return rc;
}

static bool is_subcommand(const char *s) {
    return !strcmp(s, "read") || !strcmp(s, "write") || !strcmp(s, "run") ||
           !strcmp(s, "describe");
}

int main(int argc, char **argv) {
    if (argc < 2) {
        usage();
        return 2;
    }
    /* Same output as the Rust binary's clap `--version`. TDOT_BUILD_VERSION is the
     * release version (stamped from the tag), not the contract TDOT_VERSION. */
    if (!strcmp(argv[1], "-V") || !strcmp(argv[1], "--version")) {
        printf("tedge-dot %s\n", TDOT_BUILD_VERSION);
        return 0;
    }
    /* Like the Rust binary: invoked with just config paths/options and no
     * subcommand (`tedge-dot /etc/connector.toml`, the systemd unit and the e2e
     * entrypoints do this), behave as `run`. */
    char **args = argv;
    int nargs = argc;
    char **shifted = NULL;
    if (!is_subcommand(argv[1]) && strcmp(argv[1], "-h") != 0 &&
        strcmp(argv[1], "--help") != 0) {
        shifted = calloc((size_t)argc + 2, sizeof *shifted);
        shifted[0] = argv[0];
        shifted[1] = "run";
        for (int i = 1; i < argc; i++)
            shifted[i + 1] = argv[i];
        args = shifted;
        nargs = argc + 1;
    }
    args_t a;
    int rc = 2;
    if (parse_args(nargs, args, &a) != 0)
        usage();
    else if (!strcmp(args[1], "read"))
        rc = cmd_read(&a);
    else if (!strcmp(args[1], "write"))
        rc = cmd_write(&a);
    else if (!strcmp(args[1], "run"))
        rc = cmd_run(&a);
    else if (!strcmp(args[1], "describe"))
        rc = cmd_describe(&a);
    else
        usage();
    free(shifted);
    free(a.configs);
    return rc;
}
