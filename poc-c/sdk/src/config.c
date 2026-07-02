#include "tedge_dot/config.h"

#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cjson/cJSON.h"

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

static tdot_order_t parse_order(toml_table_t *t, const char *key) {
    toml_datum_t d = toml_string_in(t, key);
    tdot_order_t o = TDOT_ORDER_BIG;
    if (d.ok) {
        if (strcmp(d.u.s, "little") == 0)
            o = TDOT_ORDER_LITTLE;
        free(d.u.s);
    }
    return o;
}

static int parse_point(toml_table_t *pt, tdot_point_t *point,
                       double device_interval, char *err, size_t errlen) {
    memset(point, 0, sizeof *point);
    toml_datum_t d = toml_string_in(pt, "id");
    if (!d.ok) {
        snprintf(err, errlen, "point missing required field: id");
        return -1;
    }
    point->id = d.u.s;

    d = toml_string_in(pt, "datatype");
    if (d.ok) {
        point->datatype = tdot_datatype_parse(d.u.s);
        if (point->datatype == TDOT_DT_NONE) {
            snprintf(err, errlen, "point %s: unknown datatype '%s'", point->id,
                     d.u.s);
            free(d.u.s);
            return -1;
        }
        free(d.u.s);
    }

    point->endianness = parse_order(pt, "endianness");
    point->word_order = parse_order(pt, "word_order");

    point->access = TDOT_ACCESS_READ;
    d = toml_string_in(pt, "access");
    if (d.ok) {
        if (strcmp(d.u.s, "read") == 0)
            point->access = TDOT_ACCESS_READ;
        else if (strcmp(d.u.s, "write") == 0)
            point->access = TDOT_ACCESS_WRITE;
        else if (strcmp(d.u.s, "read_write") == 0)
            point->access = TDOT_ACCESS_READ | TDOT_ACCESS_WRITE;
        else {
            snprintf(err, errlen, "point %s: invalid access '%s'", point->id,
                     d.u.s);
            free(d.u.s);
            return -1;
        }
        free(d.u.s);
    }

    d = toml_string_in(pt, "unit");
    if (d.ok)
        point->unit = d.u.s;

    tdot_transform_init(&point->transform);
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
        point->meta_json = toml_table_to_json_string(meta);

    point->subscribe = true;
    d = toml_bool_in(pt, "subscribe");
    if (d.ok)
        point->subscribe = d.u.b;

    point->poll_interval_s = device_interval;
    d = toml_string_in(pt, "poll_interval");
    if (d.ok) {
        point->poll_interval_s = tdot_duration_parse(d.u.s);
        if (point->poll_interval_s < 0) {
            snprintf(err, errlen, "point %s: invalid poll_interval '%s'",
                     point->id, d.u.s);
            free(d.u.s);
            return -1;
        }
        free(d.u.s);
    }

    point->address = toml_table_in(pt, "address");
    if (!point->address) {
        snprintf(err, errlen, "point %s: missing required field: address",
                 point->id);
        return -1;
    }
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
    cfg->service_name = d.ok ? d.u.s : strdup("tedge-dot");
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

    toml_array_t *devices = toml_array_in(root, "device");
    cfg->ndevices = devices ? (size_t)toml_array_nelem(devices) : 0;
    cfg->devices = calloc(cfg->ndevices ? cfg->ndevices : 1,
                          sizeof(tdot_device_t));
    for (size_t i = 0; i < cfg->ndevices; i++) {
        toml_table_t *dt = toml_table_at(devices, (int)i);
        tdot_device_t *dev = &cfg->devices[i];
        d = toml_string_in(dt, "name");
        if (!d.ok) {
            snprintf(err, errlen, "%s: device #%zu missing name", path, i + 1);
            goto fail;
        }
        dev->name = d.u.s;
        dev->protocol_address = toml_table_in(dt, "protocol_address");
        if (!dev->protocol_address) {
            snprintf(err, errlen, "%s: device %s missing protocol_address",
                     path, dev->name);
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

        toml_array_t *points = toml_array_in(dt, "point");
        dev->npoints = points ? (size_t)toml_array_nelem(points) : 0;
        dev->points =
            calloc(dev->npoints ? dev->npoints : 1, sizeof(tdot_point_t));
        for (size_t j = 0; j < dev->npoints; j++) {
            toml_table_t *ptt = toml_table_at(points, (int)j);
            if (parse_point(ptt, &dev->points[j], dev->poll_interval_s, err,
                            errlen) != 0)
                goto fail;
        }
    }
    return cfg;

fail:
    tdot_config_free(cfg);
    return NULL;
}

void tdot_config_free(tdot_config_t *cfg) {
    if (!cfg)
        return;
    for (size_t i = 0; i < cfg->ndevices; i++) {
        tdot_device_t *dev = &cfg->devices[i];
        for (size_t j = 0; j < dev->npoints; j++) {
            tdot_point_t *p = &dev->points[j];
            free(p->id);
            free(p->unit);
            free(p->meta_json);
            free(p->addr_json);
            free(p->proto);
        }
        free(dev->points);
        free(dev->name);
        free(dev->proto); /* connectors keep flat per-device state here and
                             release transports in disconnect_device() */
    }
    free(cfg->devices);
    free(cfg->path);
    free(cfg->protocol);
    free(cfg->service_name);
    free(cfg->log_level);
    free(cfg->mqtt_host);
    if (cfg->root)
        toml_free(cfg->root);
    free(cfg);
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
