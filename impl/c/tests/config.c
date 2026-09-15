/* Config-loader semantics that the runtime depends on but no higher layer
 * pins down precisely.
 *
 * The liveness bounds (contract §8.1) and the sampling-interval hint both have
 * rules that are easy to get subtly wrong and expensive to notice: a
 * stall_timeout below the per-call bound restarts a healthy connector in a
 * loop, and confusing a point's OWN poll_interval with its resolved one turns
 * push delivery back into polling at the device's rate. Both mirror the Rust
 * implementation (impl/rust/src/main.rs `stall_timeout`, and
 * impl/rust/crates/sdk/src/runtime.rs `point_ref`/`setup_subscriptions`).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "tedge_dot/config.h"
#include "tedge_dot/runtime.h"

static int failures = 0;

#define CHECK(cond, ...)                                                       \
    do {                                                                       \
        if (cond) {                                                            \
            /* pass */                                                         \
        } else {                                                               \
            failures++;                                                        \
            printf("FAIL %s:%d: ", __FILE__, __LINE__);                        \
            printf(__VA_ARGS__);                                               \
            printf("\n");                                                      \
        }                                                                      \
    } while (0)

/* A minimal modbus config; `extra` is spliced into [connector] and
 * `point_extra` into the single point. */
static tdot_config_t *load(const char *extra, const char *point_extra) {
    char template[] = "/tmp/tdot-config-XXXXXX";
    char *dir = mkdtemp(template);
    if (!dir) {
        perror("mkdtemp");
        exit(2);
    }
    char path[256];
    snprintf(path, sizeof path, "%s/modbus.toml", dir);
    FILE *fp = fopen(path, "w");
    fprintf(fp,
            "[connector]\n"
            "protocol = \"modbus\"\n"
            "poll_interval = \"2s\"\n"
            "%s"
            "\n"
            "[[device]]\n"
            "name = \"plc1\"\n"
            "poll_interval = \"30s\"\n"
            "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
            "port = 502, unit_id = 1 }\n"
            "\n"
            "  [[device.point]]\n"
            "  id = \"temp_u16\"\n"
            "  datatype = \"uint16\"\n"
            "  address = { table = \"holding\", address = 3, count = 1 }\n"
            "%s",
            extra, point_extra);
    fclose(fp);

    char err[256];
    tdot_config_t *cfg = tdot_config_load(path, err, sizeof err);
    if (!cfg) {
        printf("FAIL config did not load: %s\n", err);
        failures++;
    }
    unlink(path);
    rmdir(dir);
    return cfg;
}

static void check_timeout_defaults(void) {
    tdot_config_t *cfg = load("", "");
    if (!cfg)
        return;
    CHECK(cfg->operation_timeout_s == 30.0,
          "default operation_timeout should be 30s, got %.1f",
          cfg->operation_timeout_s);
    CHECK(cfg->stall_timeout_s == 120.0,
          "default stall_timeout should be 120s, got %.1f",
          cfg->stall_timeout_s);
    tdot_config_free(cfg);
}

/* The service name addresses the connector's management commands (contract
 * §6.3) and the flows default to tedge-dot-<protocol> when a command names
 * none, so that is the connector default too (mirrors the Rust SDK test). */
static void check_service_name_default(void) {
    tdot_config_t *cfg = load("", "");
    if (cfg) {
        CHECK(strcmp(cfg->service_name, "tedge-dot-modbus") == 0,
              "default service_name should be tedge-dot-modbus, got %s",
              cfg->service_name);
        tdot_config_free(cfg);
    }
    cfg = load("service_name = \"plant-a\"\n", "");
    if (cfg) {
        CHECK(strcmp(cfg->service_name, "plant-a") == 0,
              "configured service_name should be kept, got %s",
              cfg->service_name);
        tdot_config_free(cfg);
    }
}

static void check_timeouts_are_parsed(void) {
    tdot_config_t *cfg =
        load("operation_timeout = \"5s\"\nstall_timeout = \"90s\"\n", "");
    if (!cfg)
        return;
    CHECK(cfg->operation_timeout_s == 5.0, "operation_timeout 5s, got %.1f",
          cfg->operation_timeout_s);
    CHECK(cfg->stall_timeout_s == 90.0, "stall_timeout 90s, got %.1f",
          cfg->stall_timeout_s);
    tdot_config_free(cfg);
}

static void check_stall_timeout_is_floored(void) {
    /* Below 2x operation_timeout a single slow-but-legitimate call would look
     * like a hang, so the value is raised rather than honoured. */
    tdot_config_t *cfg =
        load("operation_timeout = \"20s\"\nstall_timeout = \"25s\"\n", "");
    if (!cfg)
        return;
    CHECK(cfg->stall_timeout_s == 40.0,
          "stall_timeout below the 2x floor should be raised to 40s, got %.1f",
          cfg->stall_timeout_s);
    tdot_config_free(cfg);
}

static void check_stall_timeout_zero_disables(void) {
    /* 0 means "no watchdog" and must survive the floor, which would otherwise
     * silently re-arm it. */
    tdot_config_t *cfg =
        load("operation_timeout = \"20s\"\nstall_timeout = \"0\"\n", "");
    if (!cfg)
        return;
    CHECK(cfg->stall_timeout_s == 0.0,
          "stall_timeout = 0 must stay disabled, got %.1f",
          cfg->stall_timeout_s);
    tdot_config_free(cfg);
}

static void check_invalid_timeouts_fall_back(void) {
    /* An unparseable bound warns and uses the default rather than refusing to
     * start: a typo must not take a gateway's connectors offline. */
    tdot_config_t *cfg = load(
        "operation_timeout = \"soon\"\nstall_timeout = \"later\"\n", "");
    if (!cfg)
        return;
    CHECK(cfg->operation_timeout_s == 30.0,
          "invalid operation_timeout should fall back to 30s, got %.1f",
          cfg->operation_timeout_s);
    CHECK(cfg->stall_timeout_s == 120.0,
          "invalid stall_timeout should fall back to 120s, got %.1f",
          cfg->stall_timeout_s);
    tdot_config_free(cfg);
}

static void check_point_interval_resolution(void) {
    /* point ?? device ?? connector, resolved once at load. This single value is
     * BOTH the polling period and the subscription's sampling-interval hint, so
     * it has to match what the Rust runtime resolves (runtime.rs, `point_ref`
     * and `subscribe_device`): the same config must sample at the same rate in
     * both builds, or one of them quietly coalesces away value changes the
     * other reports. */
    tdot_config_t *cfg = load("", "");
    if (!cfg)
        return;
    tdot_point_t *pt = &cfg->devices[0].points[0];
    CHECK(pt->poll_interval_s == 30.0,
          "a point without its own poll_interval must inherit the device's "
          "(30s), not fall back to a module default, got %.1f",
          pt->poll_interval_s);
    tdot_config_free(cfg);

    /* The point's own value wins over the device's. */
    cfg = load("", "  poll_interval = \"250ms\"\n");
    if (!cfg)
        return;
    pt = &cfg->devices[0].points[0];
    CHECK(pt->poll_interval_s == 0.25,
          "the point's own poll_interval must win (250ms), got %.3f",
          pt->poll_interval_s);
    tdot_config_free(cfg);
}

static void check_connector_interval_is_the_last_resort(void) {
    /* With no device-level value either, the connector default applies -- the
     * third step of the same chain. */
    char template[] = "/tmp/tdot-config-XXXXXX";
    char *dir = mkdtemp(template);
    if (!dir) {
        perror("mkdtemp");
        exit(2);
    }
    char path[256];
    snprintf(path, sizeof path, "%s/modbus.toml", dir);
    FILE *fp = fopen(path, "w");
    fputs("[connector]\n"
          "protocol = \"modbus\"\n"
          "poll_interval = \"7s\"\n"
          "\n"
          "[[device]]\n"
          "name = \"plc1\"\n"
          "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
          "port = 502, unit_id = 1 }\n"
          "\n"
          "  [[device.point]]\n"
          "  id = \"temp_u16\"\n"
          "  datatype = \"uint16\"\n"
          "  address = { table = \"holding\", address = 3, count = 1 }\n",
          fp);
    fclose(fp);
    char err[256];
    tdot_config_t *cfg = tdot_config_load(path, err, sizeof err);
    unlink(path);
    rmdir(dir);
    if (!cfg) {
        printf("FAIL config did not load: %s\n", err);
        failures++;
        return;
    }
    CHECK(cfg->devices[0].points[0].poll_interval_s == 7.0,
          "with no point or device value the connector's 7s must apply, got %.1f",
          cfg->devices[0].points[0].poll_interval_s);
    tdot_config_free(cfg);
}

static void check_subscribe_defaults_on(void) {
    /* Push delivery is opt-out, not opt-in (contract §4.2). */
    tdot_config_t *cfg = load("", "");
    if (!cfg)
        return;
    CHECK(cfg->devices[0].points[0].subscribe,
          "subscribe should default to true");
    CHECK(!cfg->devices[0].points[0].subscribed,
          "subscribed is runtime state and must start false");
    tdot_config_free(cfg);

    cfg = load("", "  subscribe = false\n");
    if (!cfg)
        return;
    CHECK(!cfg->devices[0].points[0].subscribe,
          "subscribe = false must be honoured");
    tdot_config_free(cfg);
}

/* ---- point libraries (contract §3.4) -------------------------------------
 * These mirror impl/rust/crates/sdk/src/library.rs's tests case for case: the
 * two implementations must resolve the same references to the same points, in
 * the same order, with the same merge rules -- a divergence here means the
 * same config samples different signals depending on which package is
 * installed. */

/* Scratch directory for one library test; `write_file` creates parents. */
typedef struct {
    char dir[256];
} scratch_t;

static void scratch_init(scratch_t *s) {
    char template[] = "/tmp/tdot-library-XXXXXX";
    char *dir = mkdtemp(template);
    if (!dir) {
        perror("mkdtemp");
        exit(2);
    }
    snprintf(s->dir, sizeof s->dir, "%s", dir);
}

/* rm -rf, deep enough for the two levels these tests create. */
static void scratch_free(scratch_t *s) {
    char cmd[512];
    snprintf(cmd, sizeof cmd, "rm -rf '%s'", s->dir);
    if (system(cmd) != 0)
        printf("warn: could not clean %s\n", s->dir);
}

static char *scratch_path(scratch_t *s, const char *rel) {
    static char path[512];
    snprintf(path, sizeof path, "%s/%s", s->dir, rel);
    return path;
}

/* Write `contents` to `rel` inside the scratch dir, creating parent dirs. */
static void write_file(scratch_t *s, const char *rel, const char *contents) {
    char path[512];
    snprintf(path, sizeof path, "%s/%s", s->dir, rel);
    char *slash = strrchr(path, '/');
    if (slash) {
        *slash = '\0';
        char cmd[640];
        snprintf(cmd, sizeof cmd, "mkdir -p '%s'", path);
        if (system(cmd) != 0) {
            printf("FAIL mkdir -p %s\n", path);
            failures++;
            return;
        }
        *slash = '/';
    }
    FILE *fp = fopen(path, "w");
    if (!fp) {
        printf("FAIL cannot write %s\n", path);
        failures++;
        return;
    }
    fputs(contents, fp);
    fclose(fp);
}

static const char *LIBRARY =
    "[library]\n"
    "protocol = \"modbus\"\n"
    "\n"
    "[[point]]\n"
    "id          = \"boiler_temp\"\n"
    "datatype    = \"float32\"\n"
    "unit        = \"C\"\n"
    "name        = \"Boiler temp\"\n"
    "description = \"Outlet temperature after the heat exchanger\"\n"
    "address     = { table = \"holding\", address = 7, count = 2 }\n"
    "\n"
    "[[point]]\n"
    "id       = \"pump_run\"\n"
    "datatype = \"bool\"\n"
    "access   = \"read_write\"\n"
    "address  = { table = \"coil\", address = 0, count = 1 }\n";

/* A connector config whose single device references `refs` and inlines
 * `inline_points`, with the library search path pinned to the scratch dir. */
static tdot_config_t *load_with_libs(scratch_t *s, const char *refs,
                                     const char *inline_points, char *err,
                                     size_t errlen) {
    char body[4096];
    snprintf(body, sizeof body,
             "[connector]\n"
             "protocol = \"modbus\"\n"
             "point_library_path = [\"%s\"]\n"
             "\n"
             "[[device]]\n"
             "name = \"plc1\"\n"
             "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
             "port = 502, unit_id = 1 }\n"
             "points_from = [%s]\n"
             "%s",
             s->dir, refs, inline_points);
    write_file(s, "etc/modbus.toml", body);
    return tdot_config_load(scratch_path(s, "etc/modbus.toml"), err, errlen);
}

static void check_named_library_is_inherited(void) {
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/acme-meter.toml", LIBRARY);

    char err[256] = "";
    tdot_config_t *cfg = load_with_libs(&s, "\"acme-meter\"", "", err, sizeof err);
    if (!cfg) {
        printf("FAIL named library did not load: %s\n", err);
        failures++;
        scratch_free(&s);
        return;
    }
    tdot_device_t *dev = &cfg->devices[0];
    CHECK(dev->npoints == 2, "expected 2 inherited points, got %zu", dev->npoints);
    if (dev->npoints == 2) {
        CHECK(strcmp(dev->points[0].id, "boiler_temp") == 0,
              "first point should be boiler_temp, got %s", dev->points[0].id);
        CHECK(strcmp(dev->points[1].id, "pump_run") == 0,
              "second point should be pump_run, got %s", dev->points[1].id);
        CHECK(dev->points[0].datatype == TDOT_DT_FLOAT32,
              "inherited datatype should be float32");
        CHECK(dev->points[0].address != NULL,
              "the address table must stay reachable after the library doc is owned "
              "by the config");
        CHECK(dev->points[1].access == (TDOT_ACCESS_READ | TDOT_ACCESS_WRITE),
              "inherited access should be read_write");
    }
    CHECK(dev->npoints_from == 1, "points_from should be recorded, got %zu",
          dev->npoints_from);
    tdot_config_free(cfg);
    scratch_free(&s);
}

/* A library is the point list of one device *type*, so it is where the type is
 * named (§3.1): every instance that references it inherits it. A device's own
 * `type` wins, and a second library extends the type rather than redefining it.
 * Mirrors library.rs::device_inherits_the_type_of_the_first_library_that_names_one. */
static void check_device_type_is_inherited_from_the_first_library(void) {
    scratch_t s;
    scratch_init(&s);
    char typed[2048];
    snprintf(typed, sizeof typed, "[library]\ntype = \"acme-meter-v2\"\n%s",
             LIBRARY + strlen("[library]\n"));
    write_file(&s, "modbus/acme-meter.toml", typed);
    write_file(&s, "modbus/site-extras.toml",
               "[library]\nprotocol = \"modbus\"\ntype = \"site-extras\"\n\n"
               "[[point]]\nid = \"spare\"\ndatatype = \"bool\"\n"
               "address = { table = \"coil\", address = 9, count = 1 }\n");
    write_file(&s, "modbus/untyped.toml", LIBRARY);

    char err[256] = "";
    tdot_config_t *cfg = load_with_libs(&s, "\"acme-meter\", \"site-extras\"", "",
                                        err, sizeof err);
    if (cfg) {
        CHECK(cfg->devices[0].type &&
                  strcmp(cfg->devices[0].type, "acme-meter-v2") == 0,
              "the first library that names a type must give it, got %s",
              cfg->devices[0].type ? cfg->devices[0].type : "<none>");
        tdot_config_free(cfg);
    } else {
        printf("FAIL typed library did not load: %s\n", err);
        failures++;
    }

    /* A library that names no type leaves the device without one: the file name
     * is not guessed at, because the type becomes a tenant-wide identifier. */
    cfg = load_with_libs(&s, "\"untyped\"", "", err, sizeof err);
    if (cfg) {
        CHECK(cfg->devices[0].type == NULL,
              "an untyped library must not invent a type, got %s",
              cfg->devices[0].type);
        tdot_config_free(cfg);
    } else {
        printf("FAIL untyped library did not load: %s\n", err);
        failures++;
    }

    /* The device's own declaration wins over the library's. */
    char body[1024];
    snprintf(body, sizeof body,
             "[connector]\n"
             "protocol = \"modbus\"\n"
             "point_library_path = [\"%s\"]\n"
             "\n"
             "[[device]]\n"
             "name = \"plc1\"\n"
             "type = \"site-special\"\n"
             "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
             "port = 502, unit_id = 1 }\n"
             "points_from = [\"acme-meter\"]\n",
             s.dir);
    write_file(&s, "etc/own-type.toml", body);
    cfg = tdot_config_load(scratch_path(&s, "etc/own-type.toml"), err, sizeof err);
    if (cfg) {
        CHECK(cfg->devices[0].type &&
                  strcmp(cfg->devices[0].type, "site-special") == 0,
              "the device's own type must win, got %s",
              cfg->devices[0].type ? cfg->devices[0].type : "<none>");
        tdot_config_free(cfg);
    } else {
        printf("FAIL own-type config did not load: %s\n", err);
        failures++;
    }

    /* A padded type is normalised at load, exactly as the Rust loader does: the set
     * names, the sample envelope and the link status all read the stored value,
     * so they cannot spell it differently.
     * Mirrors library.rs::a_declared_type_is_trimmed_once_at_load. */
    char padded[2048];
    snprintf(padded, sizeof padded, "[library]\ntype = \"  acme-meter-v2 \"\n%s",
             LIBRARY + strlen("[library]\n"));
    write_file(&s, "modbus/padded.toml", padded);
    cfg = load_with_libs(&s, "\"padded\"", "", err, sizeof err);
    if (cfg) {
        CHECK(cfg->devices[0].type &&
                  strcmp(cfg->devices[0].type, "acme-meter-v2") == 0,
              "an inherited type must be trimmed, got '%s'",
              cfg->devices[0].type ? cfg->devices[0].type : "<none>");
        tdot_config_free(cfg);
    } else {
        printf("FAIL padded library did not load: %s\n", err);
        failures++;
    }
    char padded_dev[1024];
    snprintf(padded_dev, sizeof padded_dev,
             "[connector]\n"
             "protocol = \"modbus\"\n"
             "point_library_path = [\"%s\"]\n"
             "\n"
             "[[device]]\n"
             "name = \"plc1\"\n"
             "type = \" site-special \"\n"
             "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
             "port = 502, unit_id = 1 }\n"
             "points_from = [\"acme-meter\"]\n",
             s.dir);
    write_file(&s, "etc/padded-type.toml", padded_dev);
    /* A non-breaking space is NOT isspace(), so it stays part of the type -- and
     * the Rust loader must keep it too, or one config names two different
     * tenant-wide sets depending on the package installed.
     * Mirrors library.rs::a_declared_type_is_trimmed_once_at_load. */
    char nbsp_dev[1024];
    snprintf(nbsp_dev, sizeof nbsp_dev,
             "[connector]\n"
             "protocol = \"modbus\"\n"
             "point_library_path = [\"%s\"]\n"
             "\n"
             "[[device]]\n"
             "name = \"plc1\"\n"
             "type = \"\xc2\xa0" "acme" "\xc2\xa0\"\n"
             "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
             "port = 502, unit_id = 1 }\n"
             "points_from = [\"acme-meter\"]\n",
             s.dir);
    write_file(&s, "etc/nbsp-type.toml", nbsp_dev);
    cfg = tdot_config_load(scratch_path(&s, "etc/nbsp-type.toml"), err, sizeof err);
    if (cfg) {
        CHECK(cfg->devices[0].type &&
                  strcmp(cfg->devices[0].type, "\xc2\xa0" "acme" "\xc2\xa0") == 0,
              "a non-breaking space must survive trimming, got '%s'",
              cfg->devices[0].type ? cfg->devices[0].type : "<none>");
        tdot_config_free(cfg);
    } else {
        printf("FAIL nbsp device type did not load: %s\n", err);
        failures++;
    }
    cfg = tdot_config_load(scratch_path(&s, "etc/padded-type.toml"), err, sizeof err);
    if (cfg) {
        CHECK(cfg->devices[0].type &&
                  strcmp(cfg->devices[0].type, "site-special") == 0,
              "a declared type must be trimmed, got '%s'",
              cfg->devices[0].type ? cfg->devices[0].type : "<none>");
        tdot_config_free(cfg);
    } else {
        printf("FAIL padded device type did not load: %s\n", err);
        failures++;
    }

    /* A device `type` that is present but unusable is an error, not an absent
     * type -- and the Rust loader must reject the same files. */
    /* An array or a table is present but unusable, not absent: tomlc99's
     * scalar-only lookup would drop it silently while Rust rejects the file. */
    /* "\\u000B" is a vertical tab: whitespace to C's isspace(), and the Rust
     * loader is held to the same definition so both reject it. (Upper-case hex
     * deliberately: tomlc99 only accepts A-F in a \\u escape.) */
    static const char *bad_types[] = {"\"\"",   "\"  \"",     "\"\\u000B\"",
                                      "7",    "true",     "[\"acme\"]",
                                      "{ a = 1 }"};
    for (size_t i = 0; i < sizeof bad_types / sizeof *bad_types; i++) {
        char bad_body[1024];
        snprintf(bad_body, sizeof bad_body,
                 "[connector]\n"
                 "protocol = \"modbus\"\n"
                 "point_library_path = [\"%s\"]\n"
                 "\n"
                 "[[device]]\n"
                 "name = \"plc1\"\n"
                 "type = %s\n"
                 "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
                 "port = 502, unit_id = 1 }\n"
                 "points_from = [\"acme-meter\"]\n",
                 s.dir, bad_types[i]);
        write_file(&s, "etc/bad-type.toml", bad_body);
        tdot_config_t *bad_cfg =
            tdot_config_load(scratch_path(&s, "etc/bad-type.toml"), err, sizeof err);
        CHECK(bad_cfg == NULL && strstr(err, "type must be a non-empty string") != NULL,
              "device type %s must be rejected, got '%s'", bad_types[i], err);
        tdot_config_free(bad_cfg);
    }

    /* Same for a library type: present but unusable is an error, not absent. */
    for (size_t i = 0; i < sizeof bad_types / sizeof *bad_types; i++) {
        char bad[2048];
        snprintf(bad, sizeof bad, "[library]\ntype = %s\n%s", bad_types[i],
                 LIBRARY + strlen("[library]\n"));
        write_file(&s, "modbus/bad-type.toml", bad);
        cfg = load_with_libs(&s, "\"bad-type\"", "", err, sizeof err);
        CHECK(cfg == NULL && strstr(err, "[library] type") != NULL,
              "[library] type %s must be rejected, got '%s'", bad_types[i], err);
        tdot_config_free(cfg);
    }
    scratch_free(&s);
}

static void check_library_is_protocol_scoped(void) {
    /* The same library name for two protocols; only the connector's own must
     * be picked up. */
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/shared-name.toml", LIBRARY);
    write_file(&s, "opcua/shared-name.toml",
               "[library]\nprotocol = \"opcua\"\n\n[[point]]\nid = \"wrong\"\n"
               "datatype = \"bool\"\naddress = { node = \"ns=1;i=1\" }\n");

    char err[256] = "";
    tdot_config_t *cfg = load_with_libs(&s, "\"shared-name\"", "", err, sizeof err);
    if (cfg) {
        CHECK(cfg->devices[0].npoints == 2 &&
                  strcmp(cfg->devices[0].points[0].id, "boiler_temp") == 0,
              "the modbus library must be chosen, not the opcua one");
        tdot_config_free(cfg);
    } else {
        printf("FAIL protocol-scoped lookup failed: %s\n", err);
        failures++;
    }
    scratch_free(&s);
}

static void check_search_path_order(void) {
    /* An earlier search-path entry shadows a later one, which is how a site
     * copy overrides a packaged list of the same name. */
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "site/modbus/acme-meter.toml",
               "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"site_point\"\n"
               "datatype = \"bool\"\naddress = { table = \"coil\", address = 1, count = 1 }\n");
    write_file(&s, "packaged/modbus/acme-meter.toml", LIBRARY);

    char body[1024];
    snprintf(body, sizeof body,
             "[connector]\n"
             "protocol = \"modbus\"\n"
             "point_library_path = [\"%s/site\", \"%s/packaged\"]\n"
             "\n"
             "[[device]]\n"
             "name = \"plc1\"\n"
             "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
             "port = 502, unit_id = 1 }\n"
             "points_from = [\"acme-meter\"]\n",
             s.dir, s.dir);
    write_file(&s, "etc/modbus.toml", body);
    char err[256] = "";
    tdot_config_t *cfg =
        tdot_config_load(scratch_path(&s, "etc/modbus.toml"), err, sizeof err);
    if (cfg) {
        CHECK(cfg->devices[0].npoints == 1 &&
                  strcmp(cfg->devices[0].points[0].id, "site_point") == 0,
              "the site copy must shadow the packaged one");
        tdot_config_free(cfg);
    } else {
        printf("FAIL search-path order: %s\n", err);
        failures++;
    }
    scratch_free(&s);
}

static void check_relative_path_reference(void) {
    /* A path reference resolves against the config file's own directory. */
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "lists/meter.toml", LIBRARY);

    char err[256] = "";
    tdot_config_t *cfg =
        load_with_libs(&s, "\"../lists/meter.toml\"", "", err, sizeof err);
    if (cfg) {
        CHECK(cfg->devices[0].npoints == 2,
              "a relative path reference should resolve against the config dir, got %zu points",
              cfg->devices[0].npoints);
        tdot_config_free(cfg);
    } else {
        printf("FAIL relative path reference: %s\n", err);
        failures++;
    }
    scratch_free(&s);
}

static void check_libraries_apply_in_order(void) {
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/base.toml", LIBRARY);
    write_file(&s, "modbus/extras.toml",
               "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"site_extra\"\n"
               "datatype = \"uint16\"\naddress = { table = \"holding\", address = 99, count = 1 }\n");

    char err[256] = "";
    tdot_config_t *cfg =
        load_with_libs(&s, "\"base\", \"extras\"", "", err, sizeof err);
    if (cfg) {
        tdot_device_t *dev = &cfg->devices[0];
        CHECK(dev->npoints == 3, "expected 3 points from two libraries, got %zu",
              dev->npoints);
        if (dev->npoints == 3)
            CHECK(strcmp(dev->points[2].id, "site_extra") == 0,
                  "the second library's point must come last, got %s", dev->points[2].id);
        tdot_config_free(cfg);
    } else {
        printf("FAIL multiple libraries: %s\n", err);
        failures++;
    }
    scratch_free(&s);
}

static void check_repeated_id_patches(void) {
    /* A repeated id patches the inherited definition instead of adding a
     * second point, so a tweak need not restate the address. */
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/base.toml", LIBRARY);
    write_file(&s, "modbus/tweaks.toml",
               "[library]\nprotocol = \"modbus\"\n\n[[point]]\n"
               "id = \"boiler_temp\"\npoll_interval = \"30s\"\n");

    char err[256] = "";
    tdot_config_t *cfg =
        load_with_libs(&s, "\"base\", \"tweaks\"", "", err, sizeof err);
    if (cfg) {
        tdot_device_t *dev = &cfg->devices[0];
        CHECK(dev->npoints == 2, "a patch must not add a point, got %zu", dev->npoints);
        if (dev->npoints >= 1) {
            CHECK(dev->points[0].poll_interval_s == 30.0,
                  "the patch's poll_interval should apply, got %.1f",
                  dev->points[0].poll_interval_s);
            CHECK(dev->points[0].datatype == TDOT_DT_FLOAT32,
                  "unpatched fields must be inherited");
            CHECK(dev->points[0].unit && strcmp(dev->points[0].unit, "C") == 0,
                  "the inherited unit must survive the patch");
        }
        tdot_config_free(cfg);
    } else {
        printf("FAIL repeated id patch: %s\n", err);
        failures++;
    }
    scratch_free(&s);
}

static void check_inline_points_win(void) {
    /* The device's own points are applied last: extend a packaged list, or
     * adjust one of its points, without touching the packaged file. */
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/acme-meter.toml", LIBRARY);

    char err[256] = "";
    tdot_config_t *cfg = load_with_libs(
        &s, "\"acme-meter\"",
        "\n  [[device.point]]\n"
        "  id = \"boiler_temp\"\n"
        "  unit = \"K\"\n"
        "\n  [[device.point]]\n"
        "  id = \"local_only\"\n"
        "  datatype = \"uint16\"\n"
        "  address = { table = \"holding\", address = 42, count = 1 }\n",
        err, sizeof err);
    if (cfg) {
        tdot_device_t *dev = &cfg->devices[0];
        CHECK(dev->npoints == 3, "expected 2 inherited + 1 inline, got %zu", dev->npoints);
        if (dev->npoints == 3) {
            CHECK(dev->points[0].unit && strcmp(dev->points[0].unit, "K") == 0,
                  "the inline unit must win over the library's, got %s",
                  dev->points[0].unit ? dev->points[0].unit : "(null)");
            CHECK(dev->points[0].datatype == TDOT_DT_FLOAT32,
                  "the inline override must inherit what it does not restate");
            CHECK(strcmp(dev->points[2].id, "local_only") == 0,
                  "the inline-only point must be appended, got %s", dev->points[2].id);
        }
        tdot_config_free(cfg);
    } else {
        printf("FAIL inline override: %s\n", err);
        failures++;
    }
    scratch_free(&s);
}

static void check_meta_merges_and_address_replaces(void) {
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/base.toml",
               "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"flow\"\n"
               "datatype = \"uint16\"\n"
               "address = { table = \"holding\", address = 7, count = 1 }\n"
               "transform = { multiplier = 2, decimal_shift = -3 }\n"
               "meta = { on_change = true, deadband = 0.5, parameter = { title = \"Flow\" } }\n");

    char err[256] = "";
    tdot_config_t *cfg = load_with_libs(
        &s, "\"base\"",
        "\n  [[device.point]]\n"
        "  id = \"flow\"\n"
        "  address = { table = \"input\", address = 9 }\n"
        "  transform = { decimal_shift = -1 }\n"
        "  meta = { deadband = 2.0, parameter = { max = 100 } }\n",
        err, sizeof err);
    if (!cfg) {
        printf("FAIL merge rules: %s\n", err);
        failures++;
        scratch_free(&s);
        return;
    }
    tdot_point_t *pt = &cfg->devices[0].points[0];

    /* transform merges key by key. */
    CHECK(pt->transform.multiplier == 2.0,
          "the inherited multiplier must survive, got %.1f", pt->transform.multiplier);
    CHECK(pt->transform.decimal_shift == -1,
          "the override's decimal_shift must apply, got %d", pt->transform.decimal_shift);

    /* address replaces wholesale: a half-inherited protocol address is not
     * meaningful. `count` is gone, and the table is the override's. */
    toml_datum_t table = toml_string_in(pt->address, "table");
    CHECK(table.ok && strcmp(table.u.s, "input") == 0, "address.table should be 'input'");
    if (table.ok)
        free(table.u.s);
    CHECK(!toml_int_in(pt->address, "count").ok,
          "address must replace wholesale, so the inherited count is gone");

    /* meta merges recursively, so meta.parameter keeps both fields. */
    CHECK(pt->meta_json != NULL, "meta should be present");
    if (pt->meta_json) {
        CHECK(strstr(pt->meta_json, "\"on_change\":true") != NULL,
              "inherited meta.on_change must survive: %s", pt->meta_json);
        CHECK(strstr(pt->meta_json, "\"deadband\":2") != NULL,
              "meta.deadband must be overridden: %s", pt->meta_json);
        CHECK(strstr(pt->meta_json, "\"title\":\"Flow\"") != NULL,
              "meta.parameter.title must survive the nested merge: %s", pt->meta_json);
        CHECK(strstr(pt->meta_json, "\"max\":100") != NULL,
              "meta.parameter.max must be added by the nested merge: %s", pt->meta_json);
    }
    tdot_config_free(cfg);
    scratch_free(&s);
}

static void check_one_library_shared_by_two_devices(void) {
    /* Two instances of the same device type: one list, two connections, and a
     * per-device override that must not leak into the other. */
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/acme-meter.toml", LIBRARY);

    char body[1536];
    snprintf(body, sizeof body,
             "[connector]\n"
             "protocol = \"modbus\"\n"
             "point_library_path = [\"%s\"]\n"
             "\n"
             "[[device]]\n"
             "name = \"plc1\"\n"
             "protocol_address = { transport = \"tcp\", host = \"10.0.0.1\", "
             "port = 502, unit_id = 1 }\n"
             "points_from = [\"acme-meter\"]\n"
             "\n"
             "[[device]]\n"
             "name = \"plc2\"\n"
             "protocol_address = { transport = \"tcp\", host = \"10.0.0.2\", "
             "port = 502, unit_id = 1 }\n"
             "points_from = [\"acme-meter\"]\n"
             "\n"
             "  [[device.point]]\n"
             "  id = \"boiler_temp\"\n"
             "  unit = \"K\"\n",
             s.dir);
    write_file(&s, "etc/modbus.toml", body);
    char err[256] = "";
    tdot_config_t *cfg =
        tdot_config_load(scratch_path(&s, "etc/modbus.toml"), err, sizeof err);
    if (!cfg) {
        printf("FAIL shared library: %s\n", err);
        failures++;
        scratch_free(&s);
        return;
    }
    CHECK(cfg->ndevices == 2, "expected 2 devices, got %zu", cfg->ndevices);
    CHECK(cfg->nlibs == 1, "a shared library must be parsed once, got %zu", cfg->nlibs);
    if (cfg->ndevices == 2) {
        CHECK(cfg->devices[0].npoints == 2 && cfg->devices[1].npoints == 2,
              "both devices should get the library's points");
        CHECK(cfg->devices[0].points[0].unit &&
                  strcmp(cfg->devices[0].points[0].unit, "C") == 0,
              "the second device's override must not leak into the first, got %s",
              cfg->devices[0].points[0].unit ? cfg->devices[0].points[0].unit : "(null)");
        CHECK(cfg->devices[1].points[0].unit &&
                  strcmp(cfg->devices[1].points[0].unit, "K") == 0,
              "the second device's own override must apply");
    }
    tdot_config_free(cfg);
    scratch_free(&s);
}

/* Load with the given refs/inline and expect failure, with `needle` in the
 * message: a wrong reference must be reported, not silently yield no points. */
static void check_rejects(const char *what, scratch_t *s, const char *refs,
                          const char *needle) {
    char err[256] = "";
    tdot_config_t *cfg = load_with_libs(s, refs, "", err, sizeof err);
    if (cfg) {
        printf("FAIL %s: expected a failure, config loaded with %zu point(s)\n", what,
               cfg->devices[0].npoints);
        failures++;
        tdot_config_free(cfg);
        return;
    }
    CHECK(strstr(err, needle) != NULL, "%s: expected '%s' in the error, got: %s", what,
          needle, err);
}

static void check_bad_references_are_reported(void) {
    scratch_t s;
    scratch_init(&s);

    check_rejects("unknown name", &s, "\"nope\"", "unknown point library 'nope'");
    check_rejects("missing path", &s, "\"../lists/gone.toml\"", "gone.toml");

    write_file(&s, "modbus/wrong.toml",
               "[library]\nprotocol = \"opcua\"\n\n[[point]]\nid = \"x\"\n"
               "datatype = \"bool\"\naddress = { node = \"ns=1;i=1\" }\n");
    check_rejects("protocol mismatch", &s, "\"wrong\"", "not 'modbus'");

    write_file(&s, "modbus/bare.toml",
               "[[point]]\nid = \"x\"\ndatatype = \"bool\"\n"
               "address = { table = \"coil\", address = 1, count = 1 }\n");
    check_rejects("no [library] protocol", &s, "\"bare\"", "missing [library] protocol");

    write_file(&s, "modbus/oops.toml",
               "[connector]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"x\"\n"
               "datatype = \"bool\"\naddress = { table = \"coil\", address = 1, count = 1 }\n");
    check_rejects("a connector config", &s, "\"oops\"",
                  "is a connector configuration, not a point library");

    write_file(&s, "modbus/empty.toml", "[library]\nprotocol = \"modbus\"\n");
    check_rejects("no points", &s, "\"empty\"", "declares no [[point]] entries");

    write_file(&s, "modbus/dup.toml",
               "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"x\"\n"
               "datatype = \"bool\"\naddress = { table = \"coil\", address = 1, count = 1 }\n"
               "\n[[point]]\nid = \"x\"\ndatatype = \"bool\"\n"
               "address = { table = \"coil\", address = 2, count = 1 }\n");
    check_rejects("duplicate id", &s, "\"dup\"", "declares point 'x' twice");

    /* A patch nobody defines a base for stays a point with no address, and the
     * loader must reject it rather than the connector failing later. */
    write_file(&s, "modbus/orphan.toml",
               "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"stray\"\n"
               "datatype = \"uint16\"\npoll_interval = \"30s\"\n");
    check_rejects("orphan patch", &s, "\"orphan\"", "address");

    scratch_free(&s);
}

/* Load a config body verbatim and expect it to fail with `needle` in the error. */
static void check_body_rejected(const char *what, scratch_t *s, const char *body,
                                const char *needle) {
    write_file(s, "etc/modbus.toml", body);
    char err[256] = "";
    tdot_config_t *cfg =
        tdot_config_load(scratch_path(s, "etc/modbus.toml"), err, sizeof err);
    if (cfg) {
        printf("FAIL %s: expected a failure, config loaded\n", what);
        failures++;
        tdot_config_free(cfg);
        return;
    }
    CHECK(strstr(err, needle) != NULL, "%s: expected '%s' in the error, got: %s", what,
          needle, err);
}

/* An explicit search path REPLACES the default, so it has to name somewhere.
 * Treating an empty or unusable list as "unset" and falling back is what made
 * this loader resolve a config the Rust one rejected -- the same file starting
 * under one package and failing under the other. */
static void check_explicit_search_path_must_name_somewhere(void) {
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/acme-meter.toml", LIBRARY);

    /* Set the env var too: falling back to it is exactly what is being ruled out. */
    setenv("TEDGE_DOT_POINT_LIBRARY_PATH", s.dir, 1);

    const char *head = "[connector]\nprotocol = \"modbus\"\n";
    const char *tail = "\n[[device]]\nname = \"plc1\"\n"
                       "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
                       "port = 502, unit_id = 1 }\npoints_from = [\"acme-meter\"]\n";
    char body[1024];

    snprintf(body, sizeof body, "%spoint_library_path = []\n%s", head, tail);
    check_body_rejected("empty search path", &s, body, "point_library_path is empty");

    snprintf(body, sizeof body, "%spoint_library_path = \"/etc/points.d\"\n%s", head, tail);
    check_body_rejected("scalar search path", &s, body, "must be an array");

    snprintf(body, sizeof body, "%spoint_library_path = [42]\n%s", head, tail);
    check_body_rejected("non-string entry", &s, body, "must be directory strings");

    unsetenv("TEDGE_DOT_POINT_LIBRARY_PATH");
    scratch_free(&s);
}

/* A library declaring an empty list, not a missing one. Left to resolve, the
 * device would come up healthy with no points and publish nothing -- the
 * failure mode contract §3.4 calls out, and what a generated library that
 * found nothing looks like. */
static void check_library_with_empty_point_list_is_rejected(void) {
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/generated.toml",
               "point = []\n\n[library]\nprotocol = \"modbus\"\n");
    check_rejects("empty point list", &s, "\"generated\"", "declares no [[point]] entries");
    scratch_free(&s);
}

/* The field is validated even by a config that references no library at all: a
 * typo belongs in the load error, and the Rust loader validates at the same
 * point -- when one was eager and the other lazy, the same file loaded under
 * one package and failed under the other. */
/* §3.3: unique within a connector -- see the Rust counterpart
 * (library.rs, a_repeated_device_name_is_rejected). */
/* `name` and `description` (§3.1) are ordinary scalars, so they patch like
 * `unit` does: a site can relabel an inherited point without restating its
 * address or its other label. Mirrors the Rust counterpart
 * (library.rs, labels_are_inherited_and_patch_one_at_a_time). */
static void check_labels_are_inherited_and_patch_one_at_a_time(void) {
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/acme-meter.toml", LIBRARY);

    char err[256] = "";
    tdot_config_t *cfg = load_with_libs(&s, "\"acme-meter\"", "", err, sizeof err);
    if (!cfg) {
        printf("FAIL labels did not load: %s\n", err);
        failures++;
        scratch_free(&s);
        return;
    }
    tdot_point_t *pt = &cfg->devices[0].points[0];
    CHECK(pt->name && strcmp(pt->name, "Boiler temp") == 0,
          "inherited name = %s", pt->name ? pt->name : "(null)");
    CHECK(pt->description &&
              strcmp(pt->description, "Outlet temperature after the heat exchanger") == 0,
          "inherited description = %s", pt->description ? pt->description : "(null)");
    tdot_config_free(cfg);

    /* A site relabels just the short name; the description and the rest stay. */
    cfg = load_with_libs(&s, "\"acme-meter\"",
                         "\n  [[device.point]]\n"
                         "  id = \"boiler_temp\"\n"
                         "  name = \"Flow temp (site label)\"\n",
                         err, sizeof err);
    if (cfg) {
        pt = &cfg->devices[0].points[0];
        CHECK(pt->name && strcmp(pt->name, "Flow temp (site label)") == 0,
              "patched name = %s", pt->name ? pt->name : "(null)");
        CHECK(pt->description &&
                  strcmp(pt->description, "Outlet temperature after the heat exchanger") == 0,
              "patching the name must not drop the inherited description, got %s",
              pt->description ? pt->description : "(null)");
        CHECK(pt->unit && strcmp(pt->unit, "C") == 0, "inherited unit survived");
        CHECK(pt->datatype == TDOT_DT_FLOAT32, "inherited datatype survived");
        tdot_config_free(cfg);
    } else {
        printf("FAIL label patch did not load: %s\n", err);
        failures++;
    }
    scratch_free(&s);
}

static void check_repeated_device_name_is_rejected(void) {
    scratch_t s;
    scratch_init(&s);
    const char *body =
        "[connector]\nprotocol = \"modbus\"\n"
        "\n[[device]]\nname = \"plc1\"\n"
        "protocol_address = { transport = \"tcp\", host = \"10.0.0.1\", port = 502, "
        "unit_id = 1 }\n"
        "\n  [[device.point]]\n  id = \"a\"\n  datatype = \"uint16\"\n"
        "  address = { table = \"holding\", address = 1, count = 1 }\n"
        "\n[[device]]\nname = \"plc1\"\n"
        "protocol_address = { transport = \"tcp\", host = \"10.0.0.2\", port = 502, "
        "unit_id = 1 }\n"
        "\n  [[device.point]]\n  id = \"b\"\n  datatype = \"uint16\"\n"
        "  address = { table = \"holding\", address = 2, count = 1 }\n";
    check_body_rejected("repeated device name", &s, body, "defined more than once");
    scratch_free(&s);
}

static void check_search_path_validated_without_any_reference(void) {
    scratch_t s;
    scratch_init(&s);
    const char *body =
        "[connector]\nprotocol = \"modbus\"\npoint_library_path = []\n"
        "\n[[device]]\nname = \"plc1\"\n"
        "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
        "port = 502, unit_id = 1 }\n"
        "\n  [[device.point]]\n  id = \"inline_only\"\n  datatype = \"uint16\"\n"
        "  address = { table = \"holding\", address = 3, count = 1 }\n";
    check_body_rejected("empty search path, no references", &s, body,
                        "point_library_path is empty");

    /* ...and a usable one still loads a library-free config. */
    char ok[1024];
    snprintf(ok, sizeof ok,
             "[connector]\nprotocol = \"modbus\"\npoint_library_path = [\"%s\"]\n"
             "\n[[device]]\nname = \"plc1\"\n"
             "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
             "port = 502, unit_id = 1 }\n"
             "\n  [[device.point]]\n  id = \"inline_only\"\n  datatype = \"uint16\"\n"
             "  address = { table = \"holding\", address = 3, count = 1 }\n",
             s.dir);
    write_file(&s, "etc/modbus.toml", ok);
    char err[256] = "";
    tdot_config_t *cfg =
        tdot_config_load(scratch_path(&s, "etc/modbus.toml"), err, sizeof err);
    if (cfg) {
        CHECK(cfg->devices[0].npoints == 1,
              "a library-free config with a usable search path should load its inline point, "
              "got %zu",
              cfg->devices[0].npoints);
        tdot_config_free(cfg);
    } else {
        printf("FAIL usable search path without references: %s\n", err);
        failures++;
    }
    scratch_free(&s);
}

static void check_malformed_points_from(void) {
    scratch_t s;
    scratch_init(&s);
    char body[1024];
    snprintf(body, sizeof body,
             "[connector]\n"
             "protocol = \"modbus\"\n"
             "point_library_path = [\"%s\"]\n"
             "\n"
             "[[device]]\n"
             "name = \"plc1\"\n"
             "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
             "port = 502, unit_id = 1 }\n"
             "points_from = \"acme-meter\"\n",
             s.dir);
    write_file(&s, "etc/modbus.toml", body);
    char err[256] = "";
    tdot_config_t *cfg =
        tdot_config_load(scratch_path(&s, "etc/modbus.toml"), err, sizeof err);
    if (cfg) {
        printf("FAIL a scalar points_from should be rejected\n");
        failures++;
        tdot_config_free(cfg);
    } else {
        CHECK(strstr(err, "points_from must be an array") != NULL,
              "expected an array-shape error, got: %s", err);
    }
    scratch_free(&s);
}

static void check_config_without_references_is_unchanged(void) {
    /* The common case must be untouched by the feature. */
    tdot_config_t *cfg = load("", "");
    if (!cfg)
        return;
    CHECK(cfg->devices[0].npoints == 1, "expected the single inline point, got %zu",
          cfg->devices[0].npoints);
    CHECK(cfg->devices[0].npoints_from == 0, "no libraries should be recorded");
    CHECK(cfg->nlibs == 0, "no library documents should be parsed");
    tdot_config_free(cfg);
}

static void check_stall_decision(void) {
    /* Not stalled yet: idle is below the limit. */
    CHECK(tdot_runtime_stall_idle(1000, 3000, 5.0) < 0,
          "2s idle under a 5s limit must not fire");
    /* Stalled: idle has reached the limit, and the idle time is reported so the
     * log line can say how long. */
    CHECK(tdot_runtime_stall_idle(1000, 8000, 5.0) == 7.0,
          "7s idle under a 5s limit must fire and report 7s, got %.1f",
          tdot_runtime_stall_idle(1000, 8000, 5.0));
    /* Exactly at the limit counts as stalled. */
    CHECK(tdot_runtime_stall_idle(1000, 6000, 5.0) == 5.0,
          "idle exactly at the limit must fire");
    /* A disabled watchdog must never fire, however long the loop is idle. */
    CHECK(tdot_runtime_stall_idle(1000, 900000, 0.0) < 0,
          "limit 0 disables the watchdog and must never fire");
    /* A loop that has not started ticking has not stalled -- otherwise every
     * connector would be killed moments after launch. */
    CHECK(tdot_runtime_stall_idle(0, 900000, 5.0) < 0,
          "a loop that has not started ticking must not fire");
}

static void check_watchdog_period(void) {
    double none[] = {0.0, 0.0};
    CHECK(tdot_runtime_watchdog_period(none, 2) == 0.0,
          "no armed slot needs no watchdog");
    /* A quarter of the TIGHTEST enabled limit, ignoring disabled slots. */
    double mixed[] = {120.0, 0.0, 20.0};
    CHECK(tdot_runtime_watchdog_period(mixed, 3) == 5.0,
          "period should be a quarter of the tightest limit (20s -> 5s), got %.2f",
          tdot_runtime_watchdog_period(mixed, 3));
    /* Clamped so a tight limit cannot spin the CPU... */
    double tight[] = {1.0};
    CHECK(tdot_runtime_watchdog_period(tight, 1) == 0.5,
          "period must be clamped to a 0.5s floor, got %.2f",
          tdot_runtime_watchdog_period(tight, 1));
    /* ...and a huge one still checks regularly. */
    double loose[] = {3600.0};
    CHECK(tdot_runtime_watchdog_period(loose, 1) == 10.0,
          "period must be clamped to a 10s ceiling, got %.2f",
          tdot_runtime_watchdog_period(loose, 1));
}

/* `enabled = false` (contract §3.3) takes a device out of the configuration
 * before anything else about it is read, but its name still counts. Mirrors
 * library.rs::a_disabled_device_is_left_out_without_resolving_its_libraries and
 * enabled_must_be_a_boolean_and_disabled_names_still_count. */
static void check_disabled_devices(void) {
    scratch_t s;
    scratch_init(&s);
    char body[2048];
    snprintf(body, sizeof body,
             "[connector]\nprotocol = \"modbus\"\npoint_library_path = [\"%s\"]\n"
             "\n[[device]]\nname = \"plc-1\"\nenabled = true\n"
             "protocol_address = { transport = \"tcp\", host = \"10.0.0.1\", port = 502, "
             "unit_id = 1 }\n"
             "\n  [[device.point]]\n  id = \"only\"\n  datatype = \"uint16\"\n"
             "  address = { table = \"holding\", address = 1, count = 1 }\n"
             /* Nothing about it is valid beyond its name, and nothing has to be. */
             "\n[[device]]\nname = \"plc-2\"\nenabled = false\ntype = \"\"\n"
             "points_from = [\"not-installed\"]\n",
             s.dir);
    write_file(&s, "etc/modbus.toml", body);
    char err[256] = "";
    tdot_config_t *cfg = tdot_config_load(scratch_path(&s, "etc/modbus.toml"), err, sizeof err);
    CHECK(cfg != NULL, "a config with a disabled device must load: %s", err);
    if (cfg) {
        CHECK(cfg->ndevices == 1 && strcmp(cfg->devices[0].name, "plc-1") == 0,
              "only the enabled device is loaded (got %zu device(s))", cfg->ndevices);
        CHECK(tdot_config_device(cfg, "plc-2") == NULL, "the disabled device is not loaded");
        tdot_config_free(cfg);
    }

    const char *not_bool =
        "[connector]\nprotocol = \"modbus\"\n"
        "\n[[device]]\nname = \"plc-1\"\nenabled = \"no\"\n"
        "protocol_address = { unit_id = 1 }\n";
    check_body_rejected("enabled as a string", &s, not_bool, "enabled must be true or false");

    const char *duplicate =
        "[connector]\nprotocol = \"modbus\"\n"
        "\n[[device]]\nname = \"plc-1\"\n"
        "protocol_address = { unit_id = 1 }\n"
        "\n[[device]]\nname = \"plc-1\"\nenabled = false\n";
    check_body_rejected("a disabled device named like an enabled one", &s, duplicate,
                        "defined more than once");
    scratch_free(&s);
}

/* A key the contract does not define is refused (contract §3.3), naming the
 * table and, when one is close enough to be what was meant, the key it
 * resembles. Mirrors library.rs::unknown_keys_are_refused_with_the_key_they_resemble
 * and unknown_keys_in_a_point_library_are_refused, messages included. */
static void check_unknown_keys(void) {
    scratch_t s;
    scratch_init(&s);
    static const struct {
        const char *what, *connector, *device, *point, *tail, *message;
    } cases[] = {
        {"a device key", "", "polling_interval = \"10s\"\n", "", "",
         "unknown key 'polling_interval' in device 'plc-1' (did you mean 'poll_interval'?)"},
        {"a connector key", "log_levle = \"debug\"\n", "", "", "",
         "unknown key 'log_levle' in [connector] (did you mean 'log_level'?)"},
        {"a point key", "", "", "datatyp = \"uint16\"\n", "",
         "unknown key 'datatyp' in point 't' (did you mean 'datatype'?)"},
        {"a transform key", "", "", "transform = { multiplyer = 2 }\n", "",
         "unknown key 'multiplyer' in the transform of point 't' (did you mean 'multiplier'?)"},
        {"a top-level key", "", "", "", "[mqqt]\nhost = \"broker\"\n",
         "unknown key 'mqqt' in the top level (did you mean 'mqtt'?)"},
        {"a key like none", "", "colour = \"red\"\n", "", "",
         "unknown key 'colour' in device 'plc-1'"},
        {"a disabled device's key", "", "enabled = false\nenabeld = true\n", "", "",
         "unknown key 'enabeld' in device 'plc-1' (did you mean 'enabled'?)"},
        /* Every unknown key is listed, in byte order: tomlc99 keeps file order
         * and puts tables last, the Rust parser sorts. */
        {"several keys", "",
         "zeta = 1\npolling_interval = \"10s\"\nalpha_tbl = { a = 1 }\n", "", "",
         "unknown keys in device 'plc-1': 'alpha_tbl', "
         "'polling_interval' (did you mean 'poll_interval'?), 'zeta'"},
    };
    for (size_t i = 0; i < sizeof cases / sizeof *cases; i++) {
        char body[1024];
        snprintf(body, sizeof body,
                 "[connector]\nprotocol = \"modbus\"\n%s"
                 "[[device]]\nname = \"plc-1\"\nprotocol_address = { unit_id = 1 }\n%s"
                 "[[device.point]]\nid = \"t\"\ndatatype = \"uint16\"\n"
                 "address = { address = 1 }\n%s%s",
                 cases[i].connector, cases[i].device, cases[i].point, cases[i].tail);
        check_body_rejected(cases[i].what, &s, body, cases[i].message);
    }

    /* The free-form objects are not checked. */
    write_file(&s, "etc/modbus.toml",
               "[connector]\nprotocol = \"modbus\"\n[connection]\nwhatever = 1\n"
               "[[device]]\nname = \"plc-1\"\nprotocol_address = { unit_id = 1 }\n"
               "[[device.point]]\nid = \"t\"\ndatatype = \"uint16\"\n"
               "address = { address = 1 }\nmeta = { anything = 1 }\n");
    char err[256] = "";
    tdot_config_t *cfg = tdot_config_load(scratch_path(&s, "etc/modbus.toml"), err, sizeof err);
    CHECK(cfg != NULL, "connection, protocol_address, address and meta are free-form: %s", err);
    tdot_config_free(cfg);

    /* A point library's keys are checked too. */
    write_file(&s, "modbus/acme.toml",
               "[library]\nprotocol = \"modbus\"\nvendor = \"acme\"\n\n"
               "[[point]]\nid = \"t\"\naddress = {}\n");
    cfg = load_with_libs(&s, "\"acme\"", "", err, sizeof err);
    CHECK(!cfg && strstr(err, "unknown key 'vendor' in [library] of point library"),
          "a [library] key must be refused, got: %s", cfg ? "<loaded>" : err);
    tdot_config_free(cfg);
    write_file(&s, "modbus/acme.toml",
               "[library]\nprotocol = \"modbus\"\n\n"
               "[[point]]\nid = \"t\"\naddress = {}\nunits = \"K\"\n");
    cfg = load_with_libs(&s, "\"acme\"", "", err, sizeof err);
    CHECK(!cfg && strstr(err, "unknown key 'units' in point 't' (did you mean 'unit'?)"),
          "a library point key must be refused, got: %s", cfg ? "<loaded>" : err);
    tdot_config_free(cfg);
    scratch_free(&s);
}

/* `enabled = false` on a point (contract §3.3) leaves the resolved point out of
 * its device: a bare `{ id, enabled = false }` switches off a library point, a
 * later definition switches it back on, and a disabled point need not be
 * complete but is still checked for shape. Mirrors
 * library.rs::a_disabled_point_is_left_out_of_its_device and
 * a_disabled_point_is_still_checked. */
static void check_disabled_points(void) {
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/acme-meter.toml", LIBRARY);
    write_file(&s, "modbus/site-off.toml",
               "[library]\nprotocol = \"modbus\"\n\n[[point]]\n"
               "id = \"pump_run\"\nenabled = false\n");

    char err[256] = "";
    tdot_config_t *cfg = load_with_libs(
        &s, "\"acme-meter\"",
        "\n  [[device.point]]\n  id = \"boiler_temp\"\n  enabled = false\n"
        /* Incomplete, and loads only because it is switched off. */
        "\n  [[device.point]]\n  id = \"draft\"\n  enabled = false\n",
        err, sizeof err);
    CHECK(cfg != NULL, "a config with disabled points must load: %s", err);
    if (cfg) {
        tdot_device_t *dev = &cfg->devices[0];
        CHECK(dev->npoints == 1 && strcmp(dev->points[0].id, "pump_run") == 0,
              "only pump_run is left (got %zu point(s))", dev->npoints);
        CHECK(tdot_device_point(dev, "boiler_temp") == NULL,
              "the library point is switched off");
        CHECK(tdot_device_point(dev, "draft") == NULL, "the disabled draft is dropped");
        tdot_config_free(cfg);
    }

    /* Switched off by one library, back on by the device's own definition. */
    cfg = load_with_libs(&s, "\"acme-meter\", \"site-off\"",
                         "\n  [[device.point]]\n  id = \"pump_run\"\n  enabled = true\n",
                         err, sizeof err);
    CHECK(cfg != NULL, "re-enabling a point must load: %s", err);
    if (cfg) {
        CHECK(cfg->devices[0].npoints == 2 && tdot_device_point(&cfg->devices[0], "pump_run"),
              "a later definition switches the point back on (got %zu point(s))",
              cfg->devices[0].npoints);
        tdot_config_free(cfg);
    }
    cfg = load_with_libs(&s, "\"acme-meter\", \"site-off\"", "", err, sizeof err);
    CHECK(cfg && cfg->devices[0].npoints == 1,
          "a library can switch off a point an earlier one supplies: %s",
          cfg ? "" : err);
    tdot_config_free(cfg);

    cfg = load_with_libs(&s, "\"acme-meter\"",
                         "\n  [[device.point]]\n  id = \"boiler_temp\"\n  enabled = \"no\"\n",
                         err, sizeof err);
    CHECK(!cfg && strstr(err, "enabled must be true or false"),
          "a non-boolean enabled must be refused, got: %s", cfg ? "<loaded>" : err);
    tdot_config_free(cfg);

    cfg = load_with_libs(&s, "\"acme-meter\"",
                         "\n  [[device.point]]\n  id = \"draft\"\n  enabled = false\n"
                         "  datatype = \"not_a_datatype\"\n",
                         err, sizeof err);
    CHECK(!cfg && strstr(err, "not_a_datatype"),
          "a disabled point is still checked for shape, got: %s", cfg ? "<loaded>" : err);
    tdot_config_free(cfg);

    /* A device with no libraries drops its disabled points too. */
    char body[1024];
    snprintf(body, sizeof body,
             "[connector]\nprotocol = \"modbus\"\n"
             "\n[[device]]\nname = \"plc-1\"\nprotocol_address = { unit_id = 1 }\n"
             "\n  [[device.point]]\n  id = \"only\"\n  datatype = \"uint16\"\n"
             "  address = { table = \"holding\", address = 1, count = 1 }\n"
             "\n  [[device.point]]\n  id = \"draft\"\n  enabled = false\n");
    write_file(&s, "etc/modbus.toml", body);
    cfg = tdot_config_load(scratch_path(&s, "etc/modbus.toml"), err, sizeof err);
    CHECK(cfg && cfg->devices[0].npoints == 1 && strcmp(cfg->devices[0].points[0].id, "only") == 0,
          "an inline-only device drops its disabled point: %s", cfg ? "" : err);
    tdot_config_free(cfg);
    scratch_free(&s);
}

/* True when `s` ends with `suffix`. */
static bool ends_with(const char *s, const char *suffix) {
    size_t n = strlen(s), m = strlen(suffix);
    return n >= m && strcmp(s + n - m, suffix) == 0;
}

/* The duration grammar both SDKs share: the same verdict for every string.
 * Mirrors config.rs::durations, invalid_durations_are_none_not_panics and
 * duration_grammar_matches_the_c_sdk. */
static void check_duration_grammar(void) {
    static const struct {
        const char *text;
        double secs;
    } good[] = {{"500ms", 0.5}, {"2s", 2},   {"5m", 300},  {"2h", 7200},
                {"3", 3},       {"1.5m", 90}, {" 2 s\t", 2}, {"0", 0}};
    for (size_t i = 0; i < sizeof good / sizeof *good; i++)
        CHECK(tdot_duration_parse(good[i].text) == good[i].secs,
              "duration '%s' should be %.3fs, got %.3f", good[i].text, good[i].secs,
              tdot_duration_parse(good[i].text));
    CHECK(tdot_duration_parse("18446744073709551615ms") > 0,
          "the largest millisecond count is a duration");

    static const char *const bad[] = {
        "-66", "-5s", "NaN", "inf", "1e300h", "", "abc", "1.5ms", "1e3s", "+2s",
        ".5s", "5.s", "0x10", "2 fortnights", "2s x", "s", "ms", "2 3s",
        "\xc2\xa0" "2s", "100000000000000000000s", "18446744073709551616ms"};
    for (size_t i = 0; i < sizeof bad / sizeof *bad; i++)
        CHECK(tdot_duration_parse(bad[i]) < 0, "duration '%s' must be refused, got %.3f",
              bad[i], tdot_duration_parse(bad[i]));
}

#define DURATION_MSG "must be a duration such as \"500ms\", \"2s\" or \"5m\""

/* A refused point field value and the message naming it (after `point 't': `).
 * Mirrors POINT_FIELD_CASES in impl/rust/crates/sdk/src/library.rs, messages
 * included. */
static const struct {
    const char *field, *message;
} POINT_FIELD_CASES[] = {
    {"mode = \"cooked\"", "mode must be one of \"raw\", \"typed\" (got 'cooked')"},
    {"mode = 1", "mode must be one of \"raw\", \"typed\""},
    {"datatype = \"float\"",
     "datatype must be one of \"bool\", \"int8\", \"uint8\", \"int16\", \"uint16\", "
     "\"int32\", \"uint32\", \"int64\", \"uint64\", \"float32\", \"float64\", \"string\", "
     "\"bytes\" (got 'float')"},
    {"endianness = \"middle\"", "endianness must be one of \"big\", \"little\" (got 'middle')"},
    {"word_order = \"LITTLE\"", "word_order must be one of \"big\", \"little\" (got 'LITTLE')"},
    {"word_order = 1", "word_order must be one of \"big\", \"little\""},
    {"poll_interval = \"abc\"", "poll_interval " DURATION_MSG " (got 'abc')"},
    {"poll_interval = \"1.5ms\"", "poll_interval " DURATION_MSG " (got '1.5ms')"},
    {"poll_interval = 5", "poll_interval " DURATION_MSG},
    {"address = \"holding:1\"", "address must be a table"},
    {"access = \"bogus\"",
     "access must be one of \"read\", \"write\", \"read_write\" (got 'bogus')"},
    {"access = \"readwrite\"",
     "access must be one of \"read\", \"write\", \"read_write\" (got 'readwrite')"},
    {"unit = 7", "unit must be a string"},
    {"name = true", "name must be a string"},
    {"description = [\"pump\"]", "description must be a string"},
    {"transform = 2", "transform must be a table"},
    {"transform = { multiplier = \"x\" }", "transform.multiplier must be a number"},
    {"transform = { decimal_shift = 1.5 }",
     "transform.decimal_shift must be an integer between -2147483648 and 2147483647"},
    {"transform = { decimal_shift = 3000000000 }",
     "transform.decimal_shift must be an integer between -2147483648 and 2147483647"},
    {"meta = \"x\"", "meta must be a table"},
    {"subscribe = \"yes\"", "subscribe must be true or false"},
    {"enabled = 0", "enabled must be true or false"},
};

/* A point field's value is checked at load wherever the definition is written
 * -- an inline point, a patch of a library point, a switched-off point, the
 * library itself -- so a typo is refused instead of read as a default (an
 * unknown access as "read", an unparseable poll_interval as the device's).
 * Mirrors library.rs::invalid_point_field_values_are_refused. */
static void check_invalid_point_field_values(void) {
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "modbus/base.toml",
               "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"t\"\n"
               "datatype = \"uint16\"\naddress = { address = 1 }\n");
    static const struct {
        const char *what, *refs, *tail;
    } placements[] = {{"inline", "", ""},
                      {"patch", "\"base\"", ""},
                      {"disabled", "", "enabled = false\n"}};
    for (size_t i = 0; i < sizeof POINT_FIELD_CASES / sizeof *POINT_FIELD_CASES; i++) {
        const char *field = POINT_FIELD_CASES[i].field;
        const char *message = POINT_FIELD_CASES[i].message;
        char expected[512], definition[512], err[1024];
        snprintf(expected, sizeof expected, "device 'plc1': point 't': %s", message);
        for (size_t p = 0; p < sizeof placements / sizeof *placements; p++) {
            if (*placements[p].tail && strncmp(field, "enabled", 7) == 0)
                continue;
            snprintf(definition, sizeof definition, "[[device.point]]\nid = \"t\"\n%s\n%s",
                     field, placements[p].tail);
            tdot_config_t *cfg =
                load_with_libs(&s, placements[p].refs, definition, err, sizeof err);
            CHECK(!cfg && strcmp(err, expected) == 0, "%s: %s: expected \"%s\", got: %s",
                  placements[p].what, field, expected, cfg ? "<loaded>" : err);
            tdot_config_free(cfg);
        }

        snprintf(definition, sizeof definition,
                 "[library]\nprotocol = \"modbus\"\n\n[[point]]\nid = \"t\"\n%s\n", field);
        write_file(&s, "modbus/bad.toml", definition);
        char tail[512];
        snprintf(tail, sizeof tail, ": point 't': %s", message);
        tdot_config_t *cfg = load_with_libs(&s, "\"bad\"", "", err, sizeof err);
        CHECK(!cfg && strncmp(err, "point library '", 15) == 0 && ends_with(err, tail),
              "library: %s: got: %s", field, cfg ? "<loaded>" : err);
        tdot_config_free(cfg);
    }
    scratch_free(&s);
}

/* The device- and connector-level fields the two loaders interpret. Mirrors
 * library.rs::invalid_device_and_connector_field_values_are_refused. */
static void check_invalid_device_field_values(void) {
    scratch_t s;
    scratch_init(&s);
    static const struct {
        const char *connector, *device, *message;
    } cases[] = {
        {"", "default_mode = \"cooked\"\n",
         "device 'plc-1': default_mode must be one of \"raw\", \"typed\" (got 'cooked')"},
        {"", "default_mode = 1\n", "device 'plc-1': default_mode must be one of \"raw\", \"typed\""},
        {"", "poll_interval = \"abc\"\n", "device 'plc-1': poll_interval " DURATION_MSG " (got 'abc')"},
        {"", "poll_interval = 5\n", "device 'plc-1': poll_interval " DURATION_MSG},
        {"poll_interval = \"2 fortnights\"\n", "",
         "[connector] poll_interval " DURATION_MSG " (got '2 fortnights')"},
        {"poll_interval = 5\n", "", "[connector] poll_interval " DURATION_MSG},
    };
    for (size_t i = 0; i < sizeof cases / sizeof *cases; i++) {
        char body[1024];
        snprintf(body, sizeof body,
                 "[connector]\nprotocol = \"modbus\"\n%s\n"
                 "[[device]]\nname = \"plc-1\"\nprotocol_address = { unit_id = 1 }\n%s",
                 cases[i].connector, cases[i].device);
        write_file(&s, "etc/modbus.toml", body);
        char err[1024] = "";
        tdot_config_t *cfg =
            tdot_config_load(scratch_path(&s, "etc/modbus.toml"), err, sizeof err);
        CHECK(!cfg && ends_with(err, cases[i].message), "expected \"...%s\", got: %s",
              cases[i].message, cfg ? "<loaded>" : err);
        tdot_config_free(cfg);
    }
    scratch_free(&s);
}

/* Every value the contract allows still loads -- a duration with whitespace or
 * a fraction included -- and means what it says. Mirrors
 * library.rs::valid_field_values_load. */
static void check_valid_field_values(void) {
    scratch_t s;
    scratch_init(&s);
    write_file(&s, "etc/modbus.toml",
               "[connector]\nprotocol = \"modbus\"\npoll_interval = \"1.5s\"\n\n"
               "[[device]]\nname = \"plc-1\"\nprotocol_address = { unit_id = 1 }\n"
               "default_mode = \"raw\"\npoll_interval = \" 250ms \"\n\n"
               "  [[device.point]]\n  id = \"t\"\n  mode = \"typed\"\n"
               "  datatype = \"float32\"\n  endianness = \"little\"\n  word_order = \"big\"\n"
               "  poll_interval = \"1.5 m\"\n  access = \"write\"\n  unit = \"\"\n"
               "  transform = { multiplier = 2, divisor = 0.5, decimal_shift = -1, offset = 1 }\n"
               "  meta = {}\n  subscribe = false\n  address = { address = 1 }\n");
    char err[512] = "";
    tdot_config_t *cfg = tdot_config_load(scratch_path(&s, "etc/modbus.toml"), err, sizeof err);
    CHECK(cfg != NULL, "every allowed value must load: %s", err);
    if (cfg) {
        tdot_device_t *dev = &cfg->devices[0];
        tdot_point_t *p = &dev->points[0];
        CHECK(cfg->poll_interval_s == 1.5, "connector interval 1.5s, got %.3f", cfg->poll_interval_s);
        CHECK(dev->poll_interval_s == 0.25, "device interval 250ms, got %.3f", dev->poll_interval_s);
        CHECK(p->poll_interval_s == 90.0, "point interval 1.5m, got %.3f", p->poll_interval_s);
        CHECK(p->access == TDOT_ACCESS_WRITE, "access write, got %d", (int)p->access);
        CHECK(p->endianness == TDOT_ORDER_LITTLE, "endianness little");
        tdot_config_free(cfg);
    }
    scratch_free(&s);
}

int main(void) {
    check_duration_grammar();
    check_invalid_point_field_values();
    check_invalid_device_field_values();
    check_valid_field_values();
    check_unknown_keys();
    check_disabled_devices();
    check_disabled_points();
    check_timeout_defaults();
    check_service_name_default();
    check_timeouts_are_parsed();
    check_stall_timeout_is_floored();
    check_stall_timeout_zero_disables();
    check_invalid_timeouts_fall_back();
    check_point_interval_resolution();
    check_connector_interval_is_the_last_resort();
    check_subscribe_defaults_on();
    check_config_without_references_is_unchanged();
    check_named_library_is_inherited();
    check_device_type_is_inherited_from_the_first_library();
    check_library_is_protocol_scoped();
    check_search_path_order();
    check_relative_path_reference();
    check_libraries_apply_in_order();
    check_repeated_id_patches();
    check_inline_points_win();
    check_meta_merges_and_address_replaces();
    check_one_library_shared_by_two_devices();
    check_bad_references_are_reported();
    check_malformed_points_from();
    check_explicit_search_path_must_name_somewhere();
    check_search_path_validated_without_any_reference();
    check_repeated_device_name_is_rejected();
    check_labels_are_inherited_and_patch_one_at_a_time();
    check_library_with_empty_point_list_is_rejected();
    check_stall_decision();
    check_watchdog_period();

    if (failures) {
        printf("%d check(s) failed\n", failures);
        return 1;
    }
    printf("config: all checks passed\n");
    return 0;
}
