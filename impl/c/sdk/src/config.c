#include "tedge_dot/config.h"
#include <stdbool.h>

#include <ctype.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

#include "cjson/cJSON.h"

/* ---- known keys (contract §3.3) -------------------------------------------
 * The keys a contract-level table may carry. Anything else is refused, with the
 * nearest known key suggested, so a misspelt setting -- `polling_interval` for
 * `poll_interval` -- is reported instead of silently doing nothing. The
 * protocol-specific objects (`connection`, `protocol_address`, `address`) and
 * `meta` are free-form and not checked. The Rust loader
 * (impl/rust/crates/sdk/src/library.rs `check_keys`) refuses the same keys with
 * the same message. */
static const char *const TOP_KEYS[] = {"connector", "mqtt", "connection", "device", NULL};
static const char *const CONNECTOR_KEYS[] = {
    "protocol",          "service_name",  "poll_interval",      "log_level",
    "operation_timeout", "stall_timeout", "point_library_path", NULL};
static const char *const MQTT_KEYS[] = {"host", "port", NULL};
static const char *const DEVICE_KEYS[] = {
    "name",         "type",        "protocol_address", "poll_interval",
    "default_mode", "points_from", "point",            "enabled",
    NULL};
static const char *const POINT_KEYS[] = {
    "id",      "mode",   "datatype", "endianness",  "word_order",
    "poll_interval", "address", "access", "unit", "name", "description",
    "transform", "meta", "subscribe", NULL};
static const char *const TRANSFORM_KEYS[] = {"multiplier", "divisor",
                                             "decimal_shift", "offset", NULL};
static const char *const LIBRARY_TOP_KEYS[] = {"library", "point", NULL};
static const char *const LIBRARY_KEYS[] = {"protocol", "type", "description",
                                           "version", NULL};

/* Levenshtein distance over bytes (as the Rust loader computes it). */
static size_t edit_distance(const char *a, const char *b) {
    size_t la = strlen(a), lb = strlen(b);
    size_t *prev = malloc((lb + 1) * sizeof *prev);
    size_t *cur = malloc((lb + 1) * sizeof *cur);
    if (!prev || !cur) {
        free(prev);
        free(cur);
        return (size_t)-1;
    }
    for (size_t j = 0; j <= lb; j++)
        prev[j] = j;
    for (size_t i = 1; i <= la; i++) {
        cur[0] = i;
        for (size_t j = 1; j <= lb; j++) {
            size_t best = prev[j - 1] + (a[i - 1] != b[j - 1]);
            if (prev[j] + 1 < best)
                best = prev[j] + 1;
            if (cur[j - 1] + 1 < best)
                best = cur[j - 1] + 1;
            cur[j] = best;
        }
        size_t *swap = prev;
        prev = cur;
        cur = swap;
    }
    size_t distance = prev[lb];
    free(prev);
    free(cur);
    return distance;
}

/* Refuse the first key of `tbl` that is not in `known` (NULL-terminated),
 * naming the table (`place`) and, when one is close enough to be what was
 * meant, the known key it most resembles: within an edit distance of a third
 * of the key's length, and at least 2. Returns 0, or -1 with `err` filled. */
static int check_keys(toml_table_t *tbl, const char *const *known,
                      const char *place, char *err, size_t errlen) {
    for (int i = 0;; i++) {
        const char *key = toml_key_in(tbl, i);
        if (!key)
            return 0;
        bool is_known = false;
        for (const char *const *k = known; *k && !is_known; k++)
            is_known = strcmp(*k, key) == 0;
        if (is_known)
            continue;
        const char *nearest = NULL;
        size_t nearest_distance = 0;
        for (const char *const *k = known; *k; k++) {
            size_t distance = edit_distance(key, *k);
            if (!nearest || distance < nearest_distance) {
                nearest = *k;
                nearest_distance = distance;
            }
        }
        size_t limit = strlen(key) / 3 < 2 ? 2 : strlen(key) / 3;
        if (nearest && nearest_distance <= limit)
            snprintf(err, errlen, "unknown key '%s' in %s (did you mean '%s'?)",
                     key, place, nearest);
        else
            snprintf(err, errlen, "unknown key '%s' in %s", key, place);
        return -1;
    }
}

/* The keys of one point definition, inline or in a point library, and of its
 * transform. */
static int check_point_keys(toml_table_t *pt, char *err, size_t errlen) {
    toml_datum_t id = toml_string_in(pt, "id");
    char place[256];
    snprintf(place, sizeof place, "point '%s'", id.ok ? id.u.s : "<unnamed>");
    int rc = check_keys(pt, POINT_KEYS, place, err, errlen);
    toml_table_t *transform = toml_table_in(pt, "transform");
    if (rc == 0 && transform) {
        snprintf(place, sizeof place, "the transform of point '%s'",
                 id.ok ? id.u.s : "<unnamed>");
        rc = check_keys(transform, TRANSFORM_KEYS, place, err, errlen);
    }
    if (id.ok)
        free(id.u.s);
    return rc;
}

/* The keys of a connector configuration, table by table (§3.3): the top level,
 * [connector], [mqtt], every device -- a disabled one included, since a
 * misspelt key is a mistake whether or not the device is switched on -- and
 * its inline points. Mirrors `check_document` in the Rust loader. */
static int check_document_keys(toml_table_t *root, char *err, size_t errlen) {
    if (check_keys(root, TOP_KEYS, "the top level", err, errlen) != 0)
        return -1;
    toml_table_t *conn = toml_table_in(root, "connector");
    if (conn && check_keys(conn, CONNECTOR_KEYS, "[connector]", err, errlen) != 0)
        return -1;
    toml_table_t *mqtt = toml_table_in(root, "mqtt");
    if (mqtt && check_keys(mqtt, MQTT_KEYS, "[mqtt]", err, errlen) != 0)
        return -1;
    toml_array_t *devices = toml_array_in(root, "device");
    int ndevices = devices ? toml_array_nelem(devices) : 0;
    for (int i = 0; i < ndevices; i++) {
        toml_table_t *dt = toml_table_at(devices, i);
        if (!dt)
            continue;
        toml_datum_t name = toml_string_in(dt, "name");
        char place[256];
        snprintf(place, sizeof place, "device '%s'", name.ok ? name.u.s : "<unnamed>");
        if (name.ok)
            free(name.u.s);
        if (check_keys(dt, DEVICE_KEYS, place, err, errlen) != 0)
            return -1;
        toml_array_t *points = toml_array_in(dt, "point");
        int npoints = points ? toml_array_nelem(points) : 0;
        for (int j = 0; j < npoints; j++) {
            toml_table_t *pt = toml_table_at(points, j);
            if (pt && check_point_keys(pt, err, errlen) != 0)
                return -1;
        }
    }
    return 0;
}

static char *dup_or(const char *s, const char *dflt) {
    return strdup(s ? s : dflt);
}

double tdot_duration_parse(const char *s) {
    if (!s || !*s)
        return -1.0;
    char *end = NULL;
    double v = strtod(s, &end);
    if (end == s || v < 0)
        return -1.0;
    while (*end == ' ')
        end++;
    if (*end == '\0')
        return v; /* bare seconds */
    if (strcmp(end, "ms") == 0)
        return v / 1000.0;
    if (strcmp(end, "s") == 0)
        return v;
    if (strcmp(end, "m") == 0)
        return v * 60.0;
    if (strcmp(end, "h") == 0)
        return v * 3600.0;
    return -1.0;
}

/* Convert an arbitrary toml value/table/array to a cJSON node (for the
 * free-form point `meta` echo). */
static cJSON *toml_to_json_table(toml_table_t *t);

static cJSON *toml_to_json_array(toml_array_t *a) {
    cJSON *arr = cJSON_CreateArray();
    for (int i = 0; i < toml_array_nelem(a); i++) {
        toml_datum_t d;
        toml_table_t *tt;
        toml_array_t *ta;
        if ((tt = toml_table_at(a, i)))
            cJSON_AddItemToArray(arr, toml_to_json_table(tt));
        else if ((ta = toml_array_at(a, i)))
            cJSON_AddItemToArray(arr, toml_to_json_array(ta));
        else if ((d = toml_string_at(a, i)).ok) {
            cJSON_AddItemToArray(arr, cJSON_CreateString(d.u.s));
            free(d.u.s);
        } else if ((d = toml_bool_at(a, i)).ok)
            cJSON_AddItemToArray(arr, cJSON_CreateBool(d.u.b));
        else if ((d = toml_int_at(a, i)).ok)
            cJSON_AddItemToArray(arr, cJSON_CreateNumber((double)d.u.i));
        else if ((d = toml_double_at(a, i)).ok)
            cJSON_AddItemToArray(arr, cJSON_CreateNumber(d.u.d));
    }
    return arr;
}

static cJSON *toml_to_json_table(toml_table_t *t) {
    cJSON *obj = cJSON_CreateObject();
    for (int i = 0;; i++) {
        const char *key = toml_key_in(t, i);
        if (!key)
            break;
        toml_datum_t d;
        toml_table_t *tt;
        toml_array_t *ta;
        if ((tt = toml_table_in(t, key)))
            cJSON_AddItemToObject(obj, key, toml_to_json_table(tt));
        else if ((ta = toml_array_in(t, key)))
            cJSON_AddItemToObject(obj, key, toml_to_json_array(ta));
        else if ((d = toml_string_in(t, key)).ok) {
            cJSON_AddItemToObject(obj, key, cJSON_CreateString(d.u.s));
            free(d.u.s);
        } else if ((d = toml_bool_in(t, key)).ok)
            cJSON_AddItemToObject(obj, key, cJSON_CreateBool(d.u.b));
        else if ((d = toml_int_in(t, key)).ok)
            cJSON_AddItemToObject(obj, key, cJSON_CreateNumber((double)d.u.i));
        else if ((d = toml_double_in(t, key)).ok)
            cJSON_AddItemToObject(obj, key, cJSON_CreateNumber(d.u.d));
    }
    return obj;
}

static char *toml_table_to_json_string(toml_table_t *t) {
    cJSON *obj = toml_to_json_table(t);
    char *s = cJSON_PrintUnformatted(obj);
    cJSON_Delete(obj);
    return s;
}

/* Byte/word order, assigned only when the table declares it so an overriding
 * definition cannot silently reset an inherited "little". */
static void apply_order(toml_table_t *t, const char *key, tdot_order_t *out) {
    toml_datum_t d = toml_string_in(t, key);
    if (!d.ok)
        return;
    *out = strcmp(d.u.s, "little") == 0 ? TDOT_ORDER_LITTLE : TDOT_ORDER_BIG;
    free(d.u.s);
}

static int parse_mode(toml_table_t *t, const char *key, tdot_mode_t *out) {
    toml_datum_t d = toml_string_in(t, key);
    if (!d.ok)
        return 1; /* absent */
    int rc = 0;
    if (strcmp(d.u.s, "raw") == 0)
        *out = TDOT_MODE_RAW;
    else if (strcmp(d.u.s, "typed") == 0)
        *out = TDOT_MODE_TYPED;
    else
        rc = -1;
    free(d.u.s);
    return rc;
}

/* ---- point parsing -------------------------------------------------------
 * Split in three so a point can be built from several definitions (contract
 * §3.4): a device's point libraries in order, then its own inline points, with
 * a repeated id patching the definition inherited so far. `apply_point_table`
 * therefore touches ONLY the keys the table actually declares, which is also
 * exactly what a single inline definition needs. */

/* Deep-merge `patch` into `target`: objects merge recursively, everything else
 * replaces. Mirrors the Rust resolver's rule for `meta`. */
static void json_deep_merge(cJSON *target, const cJSON *patch) {
    const cJSON *x;
    cJSON_ArrayForEach(x, patch) {
        cJSON *existing = cJSON_GetObjectItemCaseSensitive(target, x->string);
        if (existing && cJSON_IsObject(existing) && cJSON_IsObject(x))
            json_deep_merge(existing, x);
        else if (existing)
            cJSON_ReplaceItemInObjectCaseSensitive(target, x->string,
                                                   cJSON_Duplicate(x, 1));
        else
            cJSON_AddItemToObject(target, x->string, cJSON_Duplicate(x, 1));
    }
}

/* `meta` is merged key by key rather than replaced, so an override can add
 * meta.parameter.title without restating the rest of the point's meta. */
static void merge_meta(char **dst_json, toml_table_t *meta) {
    char *incoming = toml_table_to_json_string(meta);
    if (!*dst_json) {
        *dst_json = incoming;
        return;
    }
    cJSON *base = cJSON_Parse(*dst_json);
    cJSON *patch = cJSON_Parse(incoming);
    free(incoming);
    if (base && patch) {
        json_deep_merge(base, patch);
        char *merged = cJSON_PrintUnformatted(base);
        if (merged) {
            free(*dst_json);
            *dst_json = merged;
        }
    }
    cJSON_Delete(base);
    cJSON_Delete(patch);
}

/* Defaults for a point that has no definition yet. */
static void init_point(tdot_point_t *point, double device_interval,
                       tdot_mode_t device_mode) {
    memset(point, 0, sizeof *point);
    point->mode = device_mode;
    point->endianness = TDOT_ORDER_BIG;
    point->word_order = TDOT_ORDER_BIG;
    point->access = TDOT_ACCESS_READ;
    point->subscribe = true;
    point->poll_interval_s = device_interval;
    tdot_transform_init(&point->transform);
}

/* Apply one point definition onto `point`, leaving fields it does not declare
 * as they were. `meta` and `transform` merge key by key; everything else
 * (`address` included) replaces — a half-inherited protocol address is not a
 * meaningful thing, so an override that changes the address states all of it. */
static int apply_point_table(toml_table_t *pt, tdot_point_t *point, char *err,
                             size_t errlen) {
    toml_datum_t d = toml_string_in(pt, "id");
    if (d.ok) {
        free(point->id);
        point->id = d.u.s;
    }
    const char *id = point->id ? point->id : "<unnamed>";

    /* parse_mode leaves the mode untouched when the key is absent. */
    if (parse_mode(pt, "mode", &point->mode) < 0) {
        snprintf(err, errlen, "point %s: invalid mode (expected raw|typed)", id);
        return -1;
    }

    d = toml_string_in(pt, "datatype");
    if (d.ok) {
        tdot_datatype_t dt = tdot_datatype_parse(d.u.s);
        if (dt == TDOT_DT_NONE) {
            snprintf(err, errlen, "point %s: unknown datatype '%s'", id, d.u.s);
            free(d.u.s);
            return -1;
        }
        point->datatype = dt;
        free(d.u.s);
    }

    apply_order(pt, "endianness", &point->endianness);
    apply_order(pt, "word_order", &point->word_order);

    d = toml_string_in(pt, "access");
    if (d.ok) {
        if (strcmp(d.u.s, "read") == 0)
            point->access = TDOT_ACCESS_READ;
        else if (strcmp(d.u.s, "write") == 0)
            point->access = TDOT_ACCESS_WRITE;
        else if (strcmp(d.u.s, "read_write") == 0)
            point->access = TDOT_ACCESS_READ | TDOT_ACCESS_WRITE;
        else {
            snprintf(err, errlen, "point %s: invalid access '%s'", id, d.u.s);
            free(d.u.s);
            return -1;
        }
        free(d.u.s);
    }

    d = toml_string_in(pt, "unit");
    if (d.ok) {
        free(point->unit);
        point->unit = d.u.s;
    }

    d = toml_string_in(pt, "name");
    if (d.ok) {
        free(point->name);
        point->name = d.u.s;
    }

    d = toml_string_in(pt, "description");
    if (d.ok) {
        free(point->description);
        point->description = d.u.s;
    }

    toml_table_t *tr = toml_table_in(pt, "transform");
    if (tr) {
        point->has_transform = true;
        toml_datum_t td;
        if ((td = toml_double_in(tr, "multiplier")).ok)
            point->transform.multiplier = td.u.d;
        else if ((td = toml_int_in(tr, "multiplier")).ok)
            point->transform.multiplier = (double)td.u.i;
        if ((td = toml_double_in(tr, "divisor")).ok)
            point->transform.divisor = td.u.d;
        else if ((td = toml_int_in(tr, "divisor")).ok)
            point->transform.divisor = (double)td.u.i;
        if ((td = toml_int_in(tr, "decimal_shift")).ok)
            point->transform.decimal_shift = (int)td.u.i;
        if ((td = toml_double_in(tr, "offset")).ok)
            point->transform.offset = td.u.d;
        else if ((td = toml_int_in(tr, "offset")).ok)
            point->transform.offset = (double)td.u.i;
    }

    toml_table_t *meta = toml_table_in(pt, "meta");
    if (meta)
        merge_meta(&point->meta_json, meta);

    d = toml_bool_in(pt, "subscribe");
    if (d.ok)
        point->subscribe = d.u.b;

    d = toml_string_in(pt, "poll_interval");
    if (d.ok) {
        double v = tdot_duration_parse(d.u.s);
        if (v < 0) {
            snprintf(err, errlen, "point %s: invalid poll_interval '%s'", id, d.u.s);
            free(d.u.s);
            return -1;
        }
        point->poll_interval_s = v;
        free(d.u.s);
    }

    toml_table_t *address = toml_table_in(pt, "address");
    if (address)
        point->address = address;
    return 0;
}

/* Checks that only make sense once every definition of a point has been
 * applied. */
static int validate_point(const tdot_point_t *point, char *err, size_t errlen) {
    if (!point->id) {
        snprintf(err, errlen, "point missing required field: id");
        return -1;
    }
    if (point->mode == TDOT_MODE_TYPED && point->datatype == TDOT_DT_NONE) {
        snprintf(err, errlen, "point %s: typed point requires a datatype", point->id);
        return -1;
    }
    if (!point->address) {
        snprintf(err, errlen, "point %s: missing required field: address", point->id);
        return -1;
    }
    return 0;
}

/* ---- point libraries (contract §3.4) -------------------------------------
 * A point library is a protocol-scoped point list in its own file, with no
 * connection information:
 *
 *   [library]
 *   protocol = "modbus"
 *
 *   [[point]]
 *   id = "boiler_temp"
 *   ...
 *
 * and a device references it by name (resolved under the search path, in the
 * connector's protocol subdirectory) or by path:
 *
 *   points_from = ["acme-meter-v2", "./site-extras.toml"]
 *
 * Mirrors impl/rust/crates/sdk/src/library.rs; the two must agree on
 * resolution order and on the merge rules, so the same config yields the same
 * points in both implementations. */

#define TDOT_SITE_LIBRARY_DIR "/etc/tedge/plugins/ot/points.d"
#define TDOT_PACKAGED_LIBRARY_DIR "/usr/share/tedge-dot/points.d"
#define TDOT_LIBRARY_PATH_ENV "TEDGE_DOT_POINT_LIBRARY_PATH"

/* Keys a point library must not carry: their presence means the file is a
 * connector configuration, which is the mistake worth naming. */
static const char *const connector_only_keys[] = {"connector", "mqtt",
                                                  "connection", "device"};

typedef struct {
    char **dirs;
    size_t ndirs;
} search_path_t;

static void search_path_free(search_path_t *sp) {
    for (size_t i = 0; i < sp->ndirs; i++)
        free(sp->dirs[i]);
    free(sp->dirs);
    sp->dirs = NULL;
    sp->ndirs = 0;
}

static void search_path_push(search_path_t *sp, const char *dir,
                             const char *base_dir) {
    char *entry;
    if (dir[0] == '/') {
        entry = strdup(dir);
    } else {
        size_t n = strlen(base_dir) + 1 + strlen(dir) + 1;
        entry = malloc(n);
        snprintf(entry, n, "%s/%s", base_dir, dir);
    }
    sp->dirs = realloc(sp->dirs, (sp->ndirs + 1) * sizeof *sp->dirs);
    sp->dirs[sp->ndirs++] = entry;
}

/* The directories a bare library name is looked up in, most specific first:
 * [connector] point_library_path, else $TEDGE_DOT_POINT_LIBRARY_PATH (colon
 * separated), else the site directory then the packaged one. */
static int library_search_path(toml_table_t *connector, const char *base_dir,
                               search_path_t *out, char *err, size_t errlen) {
    search_path_t sp = {0};
    toml_array_t *configured =
        connector ? toml_array_in(connector, "point_library_path") : NULL;
    if (connector && !configured) {
        /* Present but not an array. */
        toml_datum_t bad = toml_string_in(connector, "point_library_path");
        if (bad.ok) {
            free(bad.u.s);
            snprintf(err, errlen,
                     "[connector] point_library_path must be an array of directories");
            return -1;
        }
    }
    if (configured) {
        int n = toml_array_nelem(configured);
        if (n == 0) {
            snprintf(err, errlen,
                     "[connector] point_library_path is empty; name at least one directory "
                     "or remove it to use the default path");
            return -1;
        }
        for (int i = 0; i < n; i++) {
            toml_datum_t d = toml_string_at(configured, i);
            if (!d.ok) {
                snprintf(err, errlen,
                         "[connector] point_library_path entries must be directory strings");
                search_path_free(&sp);
                return -1;
            }
            search_path_push(&sp, d.u.s, base_dir);
            free(d.u.s);
        }
        *out = sp;
        return 0;
    }
    const char *env = getenv(TDOT_LIBRARY_PATH_ENV);
    if (env && *env) {
        char *copy = strdup(env);
        for (char *tok = strtok(copy, ":"); tok; tok = strtok(NULL, ":"))
            if (*tok)
                search_path_push(&sp, tok, base_dir);
        free(copy);
        if (sp.ndirs) {
            *out = sp;
            return 0;
        }
    }
    search_path_push(&sp, TDOT_SITE_LIBRARY_DIR, base_dir);
    search_path_push(&sp, TDOT_PACKAGED_LIBRARY_DIR, base_dir);
    *out = sp;
    return 0;
}

static bool is_file(const char *path) {
    struct stat st;
    return stat(path, &st) == 0 && S_ISREG(st.st_mode);
}

/* True when a points_from entry is a path rather than a library name. */
bool tdot_is_path_reference(const char *ref) {
    size_t n = strlen(ref);
    /* >= 5, not > 5: ".toml" is itself a path, which is how the Rust
     * `ends_with(".toml")` classifies it. The two must agree, because this also
     * decides what a management command is allowed to name. */
    return strchr(ref, '/') != NULL || (n >= 5 && strcmp(ref + n - 5, ".toml") == 0);
}

static bool is_path_reference(const char *ref) { return tdot_is_path_reference(ref); }

/* Resolve one points_from entry to the file it names. Returns a malloc'd path,
 * or NULL with err filled. */
static char *locate_library(const char *ref, const char *protocol,
                            const char *base_dir, const search_path_t *sp,
                            char *err, size_t errlen) {
    char path[PATH_MAX];
    if (is_path_reference(ref)) {
        if (ref[0] == '/')
            snprintf(path, sizeof path, "%s", ref);
        else
            snprintf(path, sizeof path, "%s/%s", base_dir, ref);
        if (!is_file(path)) {
            snprintf(err, errlen, "point library '%s' not found at %s", ref, path);
            return NULL;
        }
        return strdup(path);
    }
    /* A bare name is protocol-scoped: libraries carry protocol-specific
     * addressing, so each protocol gets its own subdirectory. */
    size_t used = 0;
    char tried[512] = ""; /* only ever quoted into a 256-byte error message */
    for (size_t i = 0; i < sp->ndirs; i++) {
        snprintf(path, sizeof path, "%s/%s/%s.toml", sp->dirs[i], protocol, ref);
        if (is_file(path))
            return strdup(path);
        if (used < sizeof tried - 1)
            used += (size_t)snprintf(tried + used, sizeof tried - used, "%s%s",
                                     used ? ", " : "", path);
    }
    if (!used)
        snprintf(err, errlen,
                 "cannot resolve point library '%s': the library search path is empty", ref);
    else
        snprintf(err, errlen,
                 "unknown point library '%s' for protocol '%s' (looked for %s)", ref,
                 protocol, tried);
    return NULL;
}

/* Validate a parsed library document and return its [[point]] array. */
static toml_array_t *library_points(toml_table_t *root, const char *path,
                                    const char *protocol, char *err,
                                    size_t errlen) {
    for (size_t i = 0; i < sizeof connector_only_keys / sizeof *connector_only_keys; i++) {
        const char *key = connector_only_keys[i];
        if (toml_table_in(root, key) || toml_array_in(root, key)) {
            snprintf(err, errlen,
                     "'%s' is a connector configuration, not a point library (it has a "
                     "[%s] section); a point library holds only [library] and [[point]]",
                     path, key);
            return NULL;
        }
    }
    /* The known keys (§3.3), after the check above: a connector configuration
     * pointed at by mistake deserves that message rather than "unknown key
     * 'connector'". */
    char place[PATH_MAX + 64];
    snprintf(place, sizeof place, "point library '%s'", path);
    if (check_keys(root, LIBRARY_TOP_KEYS, place, err, errlen) != 0)
        return NULL;
    toml_table_t *library = toml_table_in(root, "library");
    snprintf(place, sizeof place, "[library] of point library '%s'", path);
    if (library && check_keys(library, LIBRARY_KEYS, place, err, errlen) != 0)
        return NULL;
    toml_datum_t d = library ? toml_string_in(library, "protocol")
                             : (toml_datum_t){.ok = 0};
    if (!d.ok) {
        snprintf(err, errlen,
                 "point library '%s' is missing [library] protocol = \"<protocol>\"", path);
        return NULL;
    }
    bool matches = strcmp(d.u.s, protocol) == 0;
    if (!matches)
        snprintf(err, errlen, "point library '%s' is for protocol '%s', not '%s'", path,
                 d.u.s, protocol);
    free(d.u.s);
    if (!matches)
        return NULL;

    /* An empty list is as unusable as a missing one, and far more dangerous: the
     * device would resolve to zero points, come up healthy and publish nothing
     * (§3.4). This is what a generated library that found nothing looks like. */
    toml_array_t *points = toml_array_in(root, "point");
    if (!points || toml_array_nelem(points) == 0) {
        snprintf(err, errlen, "point library '%s' declares no [[point]] entries", path);
        return NULL;
    }
    /* Within one library a repeated id is a mistake, not an override: there is
     * no order to apply it in and the second definition would silently win. */
    int n = toml_array_nelem(points);
    char **ids = calloc(n ? (size_t)n : 1, sizeof *ids);
    const char *dup = NULL;
    bool missing_id = false, bad_key = false;
    for (int i = 0; i < n && !dup && !missing_id && !bad_key; i++) {
        toml_table_t *pt = toml_table_at(points, i);
        char why[512];
        if (pt && check_point_keys(pt, why, sizeof why) != 0) {
            snprintf(err, errlen, "point library '%s': %s", path, why);
            bad_key = true;
            break;
        }
        toml_datum_t id = pt ? toml_string_in(pt, "id") : (toml_datum_t){.ok = 0};
        if (!id.ok) {
            missing_id = true;
            break;
        }
        ids[i] = id.u.s;
        for (int j = 0; j < i; j++)
            if (strcmp(ids[j], id.u.s) == 0) {
                dup = id.u.s;
                break;
            }
    }
    if (dup)
        snprintf(err, errlen, "point library '%s' declares point '%s' twice", path, dup);
    else if (missing_id)
        snprintf(err, errlen, "point library '%s': a point is missing its id", path);
    for (int i = 0; i < n; i++)
        free(ids[i]);
    free(ids);
    return (dup || missing_id || bad_key) ? NULL : points;
}

/* True when `key` is present in `tbl` under any TOML type. `toml_raw_in` only
 * sees scalars, so a key whose value is a table or an array would otherwise
 * look absent -- and `type = ["acme-meter-v2"]` would be silently dropped here
 * while the Rust loader rejects it. */
static bool key_present(toml_table_t *tbl, const char *key) {
    return toml_raw_in(tbl, key) || toml_table_in(tbl, key) ||
           toml_array_in(tbl, key);
}

/* True when `s` is empty or nothing but whitespace -- what neither a device type
 * nor a library type may be. The Rust loader rejects exactly the same values,
 * which is what keeps the two accepting the same files. */
static bool blank(const char *s) {
    for (; *s; s++)
        if (!isspace((unsigned char)*s))
            return false;
    return true;
}

/* Strip surrounding whitespace from `s` in place. A device type is rendered in
 * three places -- the parameter set names, the sample envelope and the link
 * status -- which must agree on its exact spelling, so it is normalised once
 * here, at load, exactly as the Rust loader does. */
static char *trim_in_place(char *s) {
    size_t end = strlen(s);
    while (end && isspace((unsigned char)s[end - 1]))
        s[--end] = '\0';
    size_t start = 0;
    while (s[start] && isspace((unsigned char)s[start]))
        start++;
    if (start)
        memmove(s, s + start, end - start + 1);
    return s;
}

/* The device type a library names ([library] type, §3.4), or NULL when it names
 * none -- the file name is deliberately not used instead, because this ends up
 * as a tenant-wide identifier in the cloud (§5.2). Caller frees. */
static int library_type(toml_table_t *root, const char *path, char **out,
                        char *err, size_t errlen) {
    *out = NULL;
    toml_table_t *library = toml_table_in(root, "library");
    if (!library || !key_present(library, "type"))
        return 0;
    toml_datum_t d = toml_string_in(library, "type");
    if (!d.ok || blank(d.u.s)) {
        if (d.ok)
            free(d.u.s);
        snprintf(err, errlen,
                 "point library '%s': [library] type must be a non-empty string", path);
        return -1;
    }
    *out = trim_in_place(d.u.s);
    return 0;
}

/* Parse a library once and keep it alive on the config: a point's `address` is
 * borrowed from the document it was declared in. Returns its point array, and
 * through `root_out` the document it came from (for [library] type). */
static toml_array_t *load_library(tdot_config_t *cfg, const char *path,
                                  const char *protocol, toml_table_t **root_out,
                                  char *err, size_t errlen) {
    for (size_t i = 0; i < cfg->nlibs; i++)
        if (strcmp(cfg->lib_paths[i], path) == 0) {
            *root_out = cfg->libs[i];
            return toml_array_in(cfg->libs[i], "point");
        }

    FILE *fp = fopen(path, "r");
    if (!fp) {
        snprintf(err, errlen, "cannot open point library %s", path);
        return NULL;
    }
    char tomlerr[200];
    toml_table_t *root = toml_parse_file(fp, tomlerr, sizeof tomlerr);
    fclose(fp);
    if (!root) {
        snprintf(err, errlen, "failed to parse point library '%s': %s", path, tomlerr);
        return NULL;
    }
    /* Owned from here on, so a validation failure below still frees the doc
     * when the config is freed. */
    cfg->libs = realloc(cfg->libs, (cfg->nlibs + 1) * sizeof *cfg->libs);
    cfg->lib_paths = realloc(cfg->lib_paths, (cfg->nlibs + 1) * sizeof *cfg->lib_paths);
    cfg->libs[cfg->nlibs] = root;
    cfg->lib_paths[cfg->nlibs] = strdup(path);
    cfg->nlibs++;

    *root_out = root;
    return library_points(root, path, protocol, err, errlen);
}

/* Add one point definition to a device's growing point list: a definition
 * whose id is already present patches it, a new id is appended. */
static int merge_point(tdot_device_t *dev, toml_table_t *pt,
                       tdot_mode_t device_mode, char *err, size_t errlen) {
    toml_datum_t id = toml_string_in(pt, "id");
    tdot_point_t *existing = NULL;
    if (id.ok) {
        existing = tdot_device_point(dev, id.u.s);
        free(id.u.s);
    }
    if (existing)
        return apply_point_table(pt, existing, err, errlen);

    tdot_point_t *grown =
        realloc(dev->points, (dev->npoints + 1) * sizeof *dev->points);
    if (!grown) {
        snprintf(err, errlen, "out of memory");
        return -1;
    }
    dev->points = grown;
    tdot_point_t *point = &dev->points[dev->npoints++];
    init_point(point, dev->poll_interval_s, device_mode);
    return apply_point_table(pt, point, err, errlen);
}

/* Resolve `points_from` for one device: every library in order, then the
 * device's own inline points, which therefore win. That ordering is what lets
 * a site extend a packaged list without editing the packaged file. */
static int resolve_device_points(tdot_config_t *cfg, tdot_device_t *dev,
                                 toml_table_t *dt, toml_table_t *connector,
                                 tdot_mode_t device_mode, const char *base_dir,
                                 char *err, size_t errlen) {
    toml_array_t *refs = toml_array_in(dt, "points_from");
    if (!refs) {
        /* A non-array points_from is a mistake worth naming rather than
         * silently ignoring. */
        toml_datum_t bad = toml_string_in(dt, "points_from");
        if (bad.ok) {
            free(bad.u.s);
            snprintf(err, errlen,
                     "device %s: points_from must be an array of point-library names or paths",
                     dev->name);
            return -1;
        }
    }
    int nrefs = refs ? toml_array_nelem(refs) : 0;
    if (nrefs > 0) {
        search_path_t sp = {0};
        if (library_search_path(connector, base_dir, &sp, err, errlen) != 0)
            return -1;
        dev->points_from = calloc((size_t)nrefs, sizeof *dev->points_from);
        for (int i = 0; i < nrefs; i++) {
            toml_datum_t ref = toml_string_at(refs, i);
            if (!ref.ok || !*ref.u.s) {
                if (ref.ok)
                    free(ref.u.s);
                snprintf(err, errlen,
                         "device %s: points_from entries must be non-empty strings "
                         "(point-library names or paths)",
                         dev->name);
                search_path_free(&sp);
                return -1;
            }
            dev->points_from[dev->npoints_from++] = ref.u.s;

            char *path = locate_library(ref.u.s, cfg->protocol, base_dir, &sp, err, errlen);
            if (!path) {
                search_path_free(&sp);
                return -1;
            }
            toml_table_t *lib_root = NULL;
            toml_array_t *points =
                load_library(cfg, path, cfg->protocol, &lib_root, err, errlen);
            if (points) {
                /* The device type comes from the *first* library that names
                 * one: later references extend a type rather than redefine it
                 * (["acme-meter-v2", "site-extras"]). A type on the device
                 * itself wins over both. */
                char *type = NULL;
                if (library_type(lib_root, path, &type, err, errlen) != 0)
                    points = NULL;
                else if (type && !dev->type)
                    dev->type = type;
                else
                    free(type);
            }
            free(path);
            if (!points) {
                search_path_free(&sp);
                return -1;
            }
            for (int j = 0; j < toml_array_nelem(points); j++) {
                toml_table_t *pt = toml_table_at(points, j);
                if (pt && merge_point(dev, pt, device_mode, err, errlen) != 0) {
                    search_path_free(&sp);
                    return -1;
                }
            }
        }
        search_path_free(&sp);
    }

    toml_array_t *inline_points = toml_array_in(dt, "point");
    if (!inline_points && (toml_raw_in(dt, "point") || toml_table_in(dt, "point"))) {
        snprintf(err, errlen,
                 "device %s: point must be an array of tables ([[device.point]])", dev->name);
        return -1;
    }
    int ninline = inline_points ? toml_array_nelem(inline_points) : 0;
    for (int j = 0; j < ninline; j++) {
        toml_table_t *pt = toml_table_at(inline_points, j);
        if (pt && merge_point(dev, pt, device_mode, err, errlen) != 0)
            return -1;
    }

    for (size_t j = 0; j < dev->npoints; j++)
        if (validate_point(&dev->points[j], err, errlen) != 0)
            return -1;

    /* Keep the pre-existing invariant that `points` is always allocated, so a
     * device with no points at all stays indistinguishable from before. */
    if (!dev->points)
        dev->points = calloc(1, sizeof *dev->points);

    return 0;
}

tdot_config_t *tdot_config_load(const char *path, char *err, size_t errlen) {
    FILE *fp = fopen(path, "r");
    if (!fp) {
        snprintf(err, errlen, "cannot open %s", path);
        return NULL;
    }
    char tomlerr[200];
    toml_table_t *root = toml_parse_file(fp, tomlerr, sizeof tomlerr);
    fclose(fp);
    if (!root) {
        snprintf(err, errlen, "%s: %s", path, tomlerr);
        return NULL;
    }

    tdot_config_t *cfg = calloc(1, sizeof *cfg);
    cfg->root = root;
    cfg->path = strdup(path);

    /* The known keys (§3.3), on the document as written and before anything is
     * read from it, in the order the Rust loader checks them. */
    char why[512];
    if (check_document_keys(root, why, sizeof why) != 0) {
        snprintf(err, errlen, "%s: %s", path, why);
        goto fail;
    }

    toml_table_t *conn = toml_table_in(root, "connector");
    if (!conn) {
        snprintf(err, errlen, "%s: missing [connector] section", path);
        goto fail;
    }
    toml_datum_t d = toml_string_in(conn, "protocol");
    if (!d.ok) {
        snprintf(err, errlen, "%s: [connector] missing protocol", path);
        goto fail;
    }
    cfg->protocol = d.u.s;

    d = toml_string_in(conn, "service_name");
    if (d.ok) {
        cfg->service_name = d.u.s;
    } else {
        /* tedge-dot-<protocol>, like the Rust SDK: connectors of different
         * protocols run from one directory by default and must not share a
         * service, and the name addresses their management commands (§6.3). */
        size_t n = strlen(cfg->protocol) + sizeof "tedge-dot-";
        cfg->service_name = malloc(n);
        snprintf(cfg->service_name, n, "tedge-dot-%s", cfg->protocol);
    }
    d = toml_string_in(conn, "log_level");
    cfg->log_level = d.ok ? d.u.s : strdup("info");

    cfg->poll_interval_s = 2.0;
    d = toml_string_in(conn, "poll_interval");
    if (d.ok) {
        cfg->poll_interval_s = tdot_duration_parse(d.u.s);
        if (cfg->poll_interval_s < 0) {
            snprintf(err, errlen, "%s: invalid connector.poll_interval '%s'",
                     path, d.u.s);
            free(d.u.s);
            goto fail;
        }
        free(d.u.s);
    }

    /* Liveness bounds (contract §8.1). Both are optional; an unparseable value
     * falls back to the default with a warning rather than failing the load,
     * matching the Rust runtime. */
    cfg->operation_timeout_s = 30.0;
    d = toml_string_in(conn, "operation_timeout");
    if (d.ok) {
        double v = tdot_duration_parse(d.u.s);
        if (v < 0 || v == 0) {
            fprintf(stderr,
                    "warn  invalid connector.operation_timeout '%s'; using 30s\n",
                    d.u.s);
        } else {
            cfg->operation_timeout_s = v;
        }
        free(d.u.s);
    }

    cfg->stall_timeout_s = 120.0;
    d = toml_string_in(conn, "stall_timeout");
    if (d.ok) {
        double v = tdot_duration_parse(d.u.s);
        if (v < 0) {
            fprintf(stderr,
                    "warn  invalid connector.stall_timeout '%s'; using 120s\n",
                    d.u.s);
        } else {
            cfg->stall_timeout_s = v;
        }
        free(d.u.s);
    }
    if (cfg->stall_timeout_s > 0) {
        /* Must outlast a single legitimate slow call, or a large batch on a
         * slow serial line would look like a hang and restart in a loop. */
        double floor_s = cfg->operation_timeout_s * 2;
        if (cfg->stall_timeout_s < floor_s) {
            fprintf(stderr,
                    "warn  connector.stall_timeout (%.0fs) is not longer than "
                    "operation_timeout (%.0fs); using %.0fs\n",
                    cfg->stall_timeout_s, cfg->operation_timeout_s, floor_s);
            cfg->stall_timeout_s = floor_s;
        }
    }

    toml_table_t *mqtt = toml_table_in(root, "mqtt");
    cfg->mqtt_host = dup_or(NULL, "127.0.0.1");
    cfg->mqtt_port = 1883;
    if (mqtt) {
        d = toml_string_in(mqtt, "host");
        if (d.ok) {
            free(cfg->mqtt_host);
            cfg->mqtt_host = d.u.s;
        }
        d = toml_int_in(mqtt, "port");
        if (d.ok)
            cfg->mqtt_port = (int)d.u.i;
    }

    cfg->connection = toml_table_in(root, "connection"); /* may be NULL */

    /* Relative point-library references resolve against the config file's own
     * directory, so a config and the lists next to it move together. */
    char base_dir[PATH_MAX];
    snprintf(base_dir, sizeof base_dir, "%s", path);
    char *slash = strrchr(base_dir, '/');
    if (slash)
        *slash = '\0';
    else
        snprintf(base_dir, sizeof base_dir, ".");

    /* Validate the library search path even when nothing references a library, so a
     * typo in the field is reported at load instead of waiting for the first device
     * that needs it. The Rust loader validates at the same point, which is what keeps
     * the two from accepting different files. */
    search_path_t probe = {0};
    if (library_search_path(conn, base_dir, &probe, err, errlen) != 0)
        goto fail;
    search_path_free(&probe);

    toml_array_t *devices = toml_array_in(root, "device");
    size_t ndeclared = devices ? (size_t)toml_array_nelem(devices) : 0;
    cfg->ndevices = 0; /* counts the enabled devices as they are loaded */
    cfg->devices = calloc(ndeclared ? ndeclared : 1, sizeof(tdot_device_t));
    for (size_t i = 0; i < ndeclared; i++) {
        toml_table_t *dt = toml_table_at(devices, (int)i);
        d = toml_string_in(dt, "name");
        if (!d.ok) {
            snprintf(err, errlen, "%s: device #%zu missing name", path, i + 1);
            goto fail;
        }
        /* §3.3: unique within a connector. Two same-named devices publish over
         * each other on one entity's topics, and they make "was this reference
         * already here" ambiguous for the management guard's before-lookup.
         * Checked against every declared device, disabled ones included:
         * switching one on must not produce a duplicate. */
        for (size_t k = 0; k < i; k++) {
            toml_datum_t other = toml_string_in(toml_table_at(devices, (int)k), "name");
            bool same = other.ok && strcmp(other.u.s, d.u.s) == 0;
            if (other.ok)
                free(other.u.s);
            if (same) {
                snprintf(err, errlen, "%s: device '%s' is defined more than once", path,
                         d.u.s);
                free(d.u.s);
                goto fail;
            }
        }
        /* `enabled = false` (§3.3) takes the device out of the configuration
         * before anything else about it is read -- its type, its address, its
         * point libraries -- so a config can carry a ready-made device switched
         * off, even one naming a library that is not installed yet. */
        if (key_present(dt, "enabled")) {
            toml_datum_t enabled = toml_bool_in(dt, "enabled");
            if (!enabled.ok) {
                snprintf(err, errlen, "%s: device %s: enabled must be true or false",
                         path, d.u.s);
                free(d.u.s);
                goto fail;
            }
            if (!enabled.u.b) {
                free(d.u.s);
                continue;
            }
        }
        tdot_device_t *dev = &cfg->devices[cfg->ndevices++];
        dev->name = d.u.s;
        /* The device type (§3.1). Parsed before the libraries are resolved, so a
         * device's own declaration wins over the one its library names. A
         * present-but-unusable value is an error rather than an absent type:
         * the Rust loader rejects the same files, and an empty string would
         * otherwise behave like no type at all. */
        if (key_present(dt, "type")) {
            d = toml_string_in(dt, "type");
            if (!d.ok || blank(d.u.s)) {
                if (d.ok)
                    free(d.u.s);
                snprintf(err, errlen, "%s: device %s: type must be a non-empty string",
                         path, dev->name);
                goto fail;
            }
            dev->type = trim_in_place(d.u.s);
        }
        dev->protocol_address = toml_table_in(dt, "protocol_address");
        if (!dev->protocol_address) {
            snprintf(err, errlen, "%s: device %s missing protocol_address",
                     path, dev->name);
            goto fail;
        }
        tdot_mode_t device_mode = TDOT_MODE_TYPED;
        if (parse_mode(dt, "default_mode", &device_mode) < 0) {
            snprintf(err, errlen, "%s: device %s: invalid default_mode", path,
                     dev->name);
            goto fail;
        }

        dev->poll_interval_s = cfg->poll_interval_s;
        d = toml_string_in(dt, "poll_interval");
        if (d.ok) {
            dev->poll_interval_s = tdot_duration_parse(d.u.s);
            free(d.u.s);
            if (dev->poll_interval_s < 0) {
                snprintf(err, errlen, "%s: device %s: invalid poll_interval",
                         path, dev->name);
                goto fail;
            }
        }

        /* Point libraries first (contract §3.4), then the device's own inline
         * points; a repeated id patches what came before. */
        if (resolve_device_points(cfg, dev, dt, conn, device_mode, base_dir, err,
                                  errlen) != 0)
            goto fail;
    }
    return cfg;

fail:
    tdot_config_free(cfg);
    return NULL;
}

static void free_contents(tdot_config_t *cfg, bool keep_path);

void tdot_config_release_protos(tdot_config_t *cfg) {
    for (size_t i = 0; i < cfg->ndevices; i++) {
        tdot_device_t *dev = &cfg->devices[i];
        for (size_t j = 0; j < dev->npoints; j++) {
            free(dev->points[j].proto);
            dev->points[j].proto = NULL;
            free(dev->points[j].addr_json);
            dev->points[j].addr_json = NULL;
        }
        free(dev->proto);
        dev->proto = NULL;
    }
}

void tdot_config_replace(tdot_config_t *dst, tdot_config_t *src) {
    char *path = dst->path;
    free_contents(dst, true);
    *dst = *src;
    dst->path = path;
    free(src->path);
    free(src);
}

char *tdot_config_root_json(const tdot_config_t *cfg) {
    if (!cfg->root)
        return strdup("{}");
    return toml_table_to_json_string(cfg->root);
}

char *tdot_config_fingerprint(const tdot_config_t *cfg) {
    cJSON *all = cJSON_CreateArray();
    cJSON_AddItemToArray(all, cfg->root ? toml_to_json_table(cfg->root)
                                        : cJSON_CreateObject());
    for (size_t i = 0; i < cfg->nlibs; i++)
        cJSON_AddItemToArray(all, toml_to_json_table(cfg->libs[i]));
    char *text = cJSON_PrintUnformatted(all);
    cJSON_Delete(all);
    return text;
}

void tdot_config_free(tdot_config_t *cfg) {
    if (!cfg)
        return;
    free_contents(cfg, false);
    free(cfg);
}

static void free_contents(tdot_config_t *cfg, bool keep_path) {
    for (size_t i = 0; i < cfg->ndevices; i++) {
        tdot_device_t *dev = &cfg->devices[i];
        for (size_t j = 0; j < dev->npoints; j++) {
            tdot_point_t *p = &dev->points[j];
            free(p->id);
            free(p->unit);
            free(p->name);
            free(p->description);
            free(p->meta_json);
            free(p->addr_json);
            free(p->proto);
        }
        free(dev->points);
        for (size_t j = 0; j < dev->npoints_from; j++)
            free(dev->points_from[j]);
        free(dev->points_from);
        free(dev->name);
        free(dev->type);
        free(dev->proto); /* connectors keep flat per-device state here and
                             release transports in disconnect_device() */
    }
    free(cfg->devices);
    if (!keep_path)
        free(cfg->path);
    free(cfg->protocol);
    free(cfg->service_name);
    free(cfg->log_level);
    free(cfg->mqtt_host);
    if (cfg->root)
        toml_free(cfg->root);
    /* The point tables borrowed by dev->points[].address live in these. */
    for (size_t i = 0; i < cfg->nlibs; i++) {
        toml_free(cfg->libs[i]);
        free(cfg->lib_paths[i]);
    }
    free(cfg->libs);
    free(cfg->lib_paths);
}

tdot_device_t *tdot_config_device(tdot_config_t *cfg, const char *name) {
    for (size_t i = 0; i < cfg->ndevices; i++)
        if (strcmp(cfg->devices[i].name, name) == 0)
            return &cfg->devices[i];
    return NULL;
}

tdot_point_t *tdot_device_point(tdot_device_t *dev, const char *id) {
    for (size_t j = 0; j < dev->npoints; j++)
        if (strcmp(dev->points[j].id, id) == 0)
            return &dev->points[j];
    return NULL;
}
