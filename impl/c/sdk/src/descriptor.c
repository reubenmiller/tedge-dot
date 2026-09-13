#include "tedge_dot/descriptor.h"

#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Fold `src` into `dst` in place: every RUN of characters outside [A-Za-z0-9]
 * becomes a single '_', so a device type or group can be written the way it
 * reads ("acme-meter-v2") and still be a valid fragment key. A run rather than
 * a character because this folds bytes while the Rust implementation folds
 * chars: collapsing runs is what makes them agree on a name with a non-ASCII
 * character in it (one multi-byte character = one run either way). */
static void sanitize_in_place(char *s) {
    size_t o = 0;
    bool last_was_sep = false;
    for (size_t i = 0; s[i]; i++) {
        unsigned char c = (unsigned char)s[i];
        if (isalnum(c)) {
            s[o++] = (char)c;
            last_was_sep = false;
        } else if (!last_was_sep) {
            s[o++] = '_';
            last_was_sep = true;
        }
    }
    s[o] = '\0';
}

char *tdot_param_set_name(const char *qualifier, const char *group) {
    if (!group || !*group)
        group = TDOT_PARAM_DEFAULT_GROUP;
    if (!qualifier)
        qualifier = "";
    /* Assembled first and sanitized as a whole, so a qualifier that already
     * ends in a separator does not produce a doubled '_'. */
    size_t n = strlen(qualifier) + 1 + strlen(group) + sizeof("_parameters");
    char *out = malloc(n);
    snprintf(out, n, "%s_%s_parameters", qualifier, group);
    sanitize_in_place(out);
    return out;
}

tdot_set_naming_t tdot_param_naming(const tdot_device_t *dev,
                                    const char *protocol, const char *forced) {
    tdot_set_naming_t naming = {forced, protocol};
    if (dev && dev->type && *dev->type)
        naming.qualifier = dev->type;
    return naming;
}

bool tdot_param_key_valid(const char *key) {
    if (!key || !*key)
        return false;
    for (const char *p = key; *p; p++)
        if (!isalnum((unsigned char)*p) && *p != '_')
            return false;
    return true;
}

/* The point's `meta.parameter` normalized to an object, or NULL when the point
 * is not a parameter. Caller cJSON_Delete()s the result.
 * Mirrors descriptor.rs::parameter_of. */
static cJSON *parameter_options(const tdot_point_t *point) {
    cJSON *meta = point->meta_json ? cJSON_Parse(point->meta_json) : NULL;
    cJSON *param =
        meta ? cJSON_GetObjectItemCaseSensitive(meta, "parameter") : NULL;
    cJSON *options = NULL;

    if (param && cJSON_IsFalse(param)) { /* explicit opt-out */
        cJSON_Delete(meta);
        return NULL;
    }
    if (param) {
        if (cJSON_IsObject(param))
            options = cJSON_Duplicate(param, 1);
        else if (cJSON_IsString(param)) {
            options = cJSON_CreateObject(); /* a bare string names the set */
            cJSON_AddStringToObject(options, "set", param->valuestring);
        } else {
            options = cJSON_CreateObject(); /* `true`, or any other scalar */
        }
    } else if (point->access & TDOT_ACCESS_WRITE) {
        options = cJSON_CreateObject(); /* writable, no meta.parameter */
    }
    cJSON_Delete(meta);
    return options;
}

/* Append `name` to a growing string list unless it is empty or already there.
 * Takes ownership of `name` (frees it when it is a duplicate). */
static void push_name(char ***list, size_t *n, char *name) {
    if (!name || !*name) {
        free(name);
        return;
    }
    for (size_t i = 0; i < *n; i++)
        if (strcmp((*list)[i], name) == 0) {
            free(name);
            return;
        }
    *list = realloc(*list, (*n + 1) * sizeof **list);
    (*list)[(*n)++] = name;
}

/* The names a `set`/`group` option holds: one string, or an array of them.
 * Empty and non-string entries are ignored, so a mistyped entry degrades to the
 * default group rather than inventing a set name. Mirrors
 * descriptor.rs::names_of. */
static char **names_of(const cJSON *options, const char *key, size_t *n) {
    char **names = NULL;
    *n = 0;
    const cJSON *value = cJSON_GetObjectItemCaseSensitive(options, key);
    if (cJSON_IsString(value)) {
        push_name(&names, n, strdup(value->valuestring));
    } else if (cJSON_IsArray(value)) {
        const cJSON *item;
        cJSON_ArrayForEach(item, value) {
            if (cJSON_IsString(item))
                push_name(&names, n, strdup(item->valuestring));
        }
    }
    return names;
}

/* Every set the options put the point in. `set` is absolute (used verbatim) and
 * wins over `group`; each accepts a string or a list.
 * Mirrors descriptor.rs::SetNaming::sets_of. */
static char **sets_of(const cJSON *options, const tdot_set_naming_t *naming,
                      size_t *n) {
    char **sets = names_of(options, "set", n);
    if (*n)
        return sets; /* absolute */
    if (naming->forced) {
        push_name(&sets, n, strdup(naming->forced));
        return sets;
    }
    size_t ngroups = 0;
    char **groups = names_of(options, "group", &ngroups);
    if (!ngroups) {
        push_name(&sets, n,
                  tdot_param_set_name(naming->qualifier,
                                      TDOT_PARAM_DEFAULT_GROUP));
    }
    /* Deduped on the resulting names, not the group names: two groups can fold
     * to one set ("a b" and "a-b") and a point must not appear twice in one
     * definition. */
    for (size_t i = 0; i < ngroups; i++)
        push_name(&sets, n, tdot_param_set_name(naming->qualifier, groups[i]));
    tdot_param_sets_free(groups, ngroups);
    return sets;
}

char **tdot_param_sets(const tdot_point_t *point,
                       const tdot_set_naming_t *naming, size_t *n) {
    *n = 0;
    cJSON *options = parameter_options(point);
    if (!options)
        return NULL;
    char **sets = sets_of(options, naming, n);
    cJSON_Delete(options);
    return sets;
}

void tdot_param_sets_free(char **sets, size_t n) {
    for (size_t i = 0; i < n; i++)
        free(sets[i]);
    free(sets);
}

bool tdot_param_is(const tdot_point_t *point, const tdot_set_naming_t *naming) {
    size_t n = 0;
    char **sets = tdot_param_sets(point, naming, &n);
    bool is = sets != NULL;
    tdot_param_sets_free(sets, n);
    return is;
}

/* Append "sep"-joined text to a growing heap string. */
static void append(char **buf, size_t *len, const char *sep, const char *text) {
    size_t add = strlen(text) + (*len ? strlen(sep) : 0);
    *buf = realloc(*buf, *len + add + 1);
    if (*len)
        memcpy(*buf + *len, sep, strlen(sep));
    strcpy(*buf + *len + (*len ? strlen(sep) : 0), text);
    *len += add;
}

char *tdot_param_invalid_keys(const tdot_config_t *cfg, const char *forced) {
    return tdot_param_invalid_keys_across(&cfg, 1, forced);
}

char *tdot_param_invalid_keys_across(const tdot_config_t *const *cfgs,
                                     size_t ncfgs, const char *forced) {
    char *buf = NULL;
    size_t len = 0;
    for (size_t c = 0; c < ncfgs; c++) {
        const tdot_config_t *cfg = cfgs[c];
        for (size_t i = 0; i < cfg->ndevices; i++) {
            const tdot_device_t *dev = &cfg->devices[i];
            tdot_set_naming_t naming =
                tdot_param_naming(dev, cfg->protocol, forced);
            for (size_t j = 0; j < dev->npoints; j++) {
                const tdot_point_t *pt = &dev->points[j];
                size_t nsets = 0;
                char **sets = tdot_param_sets(pt, &naming, &nsets);
                if (!sets)
                    continue;
                char item[256];
                if (!tdot_param_key_valid(pt->id)) {
                    snprintf(item, sizeof item, "point id '%s'", pt->id);
                    append(&buf, &len, ", ", item);
                }
                for (size_t k = 0; k < nsets; k++)
                    if (!tdot_param_key_valid(sets[k])) {
                        snprintf(item, sizeof item, "parameter set '%s'",
                                 sets[k]);
                        append(&buf, &len, ", ", item);
                    }
                tdot_param_sets_free(sets, nsets);
            }
        }
    }
    return buf;
}

char *tdot_param_type_warnings(const tdot_config_t *cfg) {
    return tdot_param_type_warnings_across(&cfg, 1);
}

char *tdot_param_type_warnings_across(const tdot_config_t *const *cfgs,
                                      size_t ncfgs) {
    /* Grouped by the set name the types actually derive -- the same string the
     * Rust build groups and displays by, so there is no second notion of
     * "qualifier" for the two to disagree about. Everything here is heap-built:
     * a device type is an arbitrary configured string, and a truncated warning
     * would name a type that is not in the configuration. */
    struct group {
        char *key;    /* representative set name */
        char **types; /* distinct raw types deriving it */
        size_t ntypes;
    } *groups = NULL;
    size_t ngroups = 0;

    char *buf = NULL;
    size_t len = 0;

    for (size_t c = 0; c < ncfgs; c++) {
        const tdot_config_t *cfg = cfgs[c];
        for (size_t i = 0; i < cfg->ndevices; i++) {
            const char *declared = cfg->devices[i].type;
            if (!declared || !*declared)
                continue;
            /* A type with nothing usable in it ("日本語", "---") folds away
             * entirely and the sets are named "_control_parameters" -- which
             * every such type shares, silently. */
            bool usable = false;
            for (const char *p = declared; *p; p++)
                if (isalnum((unsigned char)*p)) {
                    usable = true;
                    break;
                }
            if (!usable) {
                char *derived =
                    tdot_param_set_name(declared, TDOT_PARAM_DEFAULT_GROUP);
                static const char *UNUSABLE_FMT =
                    "warning: device type '%s' has no [A-Za-z0-9] character, so "
                    "its parameter sets are named '%s' with nothing to tell them "
                    "apart from another such type's; name the type in ASCII";
                size_t ulen = strlen(UNUSABLE_FMT) + strlen(declared) +
                              strlen(derived) + 1;
                char *msg = malloc(ulen);
                if (msg) {
                    snprintf(msg, ulen, UNUSABLE_FMT, declared, derived);
                    append(&buf, &len, "\n", msg);
                    free(msg);
                }
                free(derived);
            }
            /* A device with no parameters derives no set, so it cannot
             * collide. */
            tdot_set_naming_t naming =
                tdot_param_naming(&cfg->devices[i], cfg->protocol, NULL);
            bool has_parameters = false;
            for (size_t j = 0; j < cfg->devices[i].npoints; j++)
                if (tdot_param_is(&cfg->devices[i].points[j], &naming)) {
                    has_parameters = true;
                    break;
                }
            if (!has_parameters)
                continue;
            char *key = tdot_param_set_name(declared, TDOT_PARAM_DEFAULT_GROUP);
            struct group *g = NULL;
            for (size_t k = 0; k < ngroups; k++)
                if (strcmp(groups[k].key, key) == 0) {
                    g = &groups[k];
                    break;
                }
            if (!g) {
                groups = realloc(groups, (ngroups + 1) * sizeof *groups);
                g = &groups[ngroups++];
                g->key = key;
                g->types = NULL;
                g->ntypes = 0;
            } else {
                free(key);
            }
            bool seen = false;
            for (size_t t = 0; t < g->ntypes; t++)
                if (strcmp(g->types[t], declared) == 0) {
                    seen = true; /* one type on two devices is one device type */
                    break;
                }
            if (!seen) {
                g->types =
                    realloc(g->types, (g->ntypes + 1) * sizeof *g->types);
                g->types[g->ntypes++] = strdup(declared);
            }
        }
    }

    static const char *FMT =
        "warning: device types %s derive the same parameter set names (e.g. "
        "'%s'), so they share one tenant-wide definition and the first one "
        "rendered wins; give them names that differ by more than punctuation";
    for (size_t k = 0; k < ngroups; k++) {
        /* More than one DISTINCT type is the collision -- counted, never
         * inferred from the rendered text (a type may contain a comma). */
        if (groups[k].ntypes > 1) {
            char *names = NULL;
            size_t nlen = 0;
            for (size_t t = 0; t < groups[k].ntypes; t++) {
                size_t qlen = strlen(groups[k].types[t]) + 3;
                char *quoted = malloc(qlen);
                if (quoted) {
                    snprintf(quoted, qlen, "'%s'", groups[k].types[t]);
                    append(&names, &nlen, ", ", quoted);
                    free(quoted);
                }
            }
            size_t mlen = strlen(FMT) + nlen + strlen(groups[k].key) + 1;
            char *msg = malloc(mlen);
            if (msg) {
                snprintf(msg, mlen, FMT, names ? names : "", groups[k].key);
                append(&buf, &len, "\n", msg);
                free(msg);
            }
            free(names);
        }
        for (size_t t = 0; t < groups[k].ntypes; t++)
            free(groups[k].types[t]);
        free(groups[k].types);
        free(groups[k].key);
    }
    free(groups);
    return buf;
}

char *tdot_param_untyped_devices(const tdot_config_t *cfg) {
    return tdot_param_untyped_devices_across(&cfg, 1, cfg->protocol);
}

char *tdot_param_untyped_devices_across(const tdot_config_t *const *cfgs,
                                        size_t ncfgs, const char *protocol) {
    char *buf = NULL;
    size_t len = 0;
    for (size_t c = 0; c < ncfgs; c++) {
        const tdot_config_t *cfg = cfgs[c];
        if (strcmp(cfg->protocol, protocol) != 0)
            continue;
        for (size_t i = 0; i < cfg->ndevices; i++) {
            const tdot_device_t *dev = &cfg->devices[i];
            if (dev->type && *dev->type)
                continue;
            tdot_set_naming_t naming =
                tdot_param_naming(dev, cfg->protocol, NULL);
            for (size_t j = 0; j < dev->npoints; j++) {
                if (tdot_param_is(&dev->points[j], &naming)) {
                    append(&buf, &len, ", ", dev->name);
                    break;
                }
            }
        }
    }
    return buf;
}

/* JSON-schema type and datatype range for a point's datatype. 64-bit limits
 * exceed the JS safe range, so they are left unbounded (as in Rust). */
static const char *schema_type(tdot_datatype_t dt, bool *has_range, double *min,
                               double *max) {
    *has_range = true;
    switch (dt) {
    case TDOT_DT_BOOL:
        *has_range = false;
        return "boolean";
    case TDOT_DT_INT8:
        *min = -128.0, *max = 127.0;
        return "integer";
    case TDOT_DT_UINT8:
        *min = 0.0, *max = 255.0;
        return "integer";
    case TDOT_DT_INT16:
        *min = -32768.0, *max = 32767.0;
        return "integer";
    case TDOT_DT_UINT16:
        *min = 0.0, *max = 65535.0;
        return "integer";
    case TDOT_DT_INT32:
        *min = -2147483648.0, *max = 2147483647.0;
        return "integer";
    case TDOT_DT_UINT32:
        *min = 0.0, *max = 4294967295.0;
        return "integer";
    case TDOT_DT_INT64:
    case TDOT_DT_UINT64:
        *has_range = false;
        return "integer";
    case TDOT_DT_FLOAT32:
    case TDOT_DT_FLOAT64:
        *has_range = false;
        return "number";
    default: /* string / raw / unset */
        *has_range = false;
        return "string";
    }
}

static const char *opt_string(const cJSON *options, const char *key) {
    const cJSON *v = cJSON_GetObjectItemCaseSensitive(options, key);
    return cJSON_IsString(v) ? v->valuestring : NULL;
}

/* JSON-schema property for one parameter: type and limits from the datatype,
 * everything else from `meta.parameter`. */
static cJSON *property_schema(const tdot_point_t *point, const cJSON *options) {
    cJSON *schema = cJSON_CreateObject();
    bool has_range = false;
    double min = 0, max = 0;
    cJSON_AddStringToObject(schema, "type",
                            schema_type(point->datatype, &has_range, &min, &max));

    /* meta.parameter.title wins, then the point's own `name`, then the id: a
     * point can carry a general-purpose label and still say something
     * different in the parameter UI (mirrors descriptor.rs property_schema). */
    const char *title = opt_string(options, "title");
    if (!title)
        title = point->name;
    cJSON_AddStringToObject(schema, "title", title ? title : point->id);

    /* Heap-built, because `description` and `unit` are arbitrary configured
     * strings: a fixed buffer would truncate where the Rust SDK does not, and
     * the two builds must render the same definition. */
    const char *d = opt_string(options, "description");
    if (!d)
        d = point->description;
    static const char *WRITE_ONLY = "(write-only: shows the last value written)";
    size_t len = (d ? strlen(d) : 0) +
                 (point->unit ? strlen(point->unit) + 4 : 0) +
                 (point->access == TDOT_ACCESS_WRITE ? strlen(WRITE_ONLY) + 1 : 0) + 1;
    char *description = calloc(1, len);
    if (description) {
        size_t n = 0;
        if (d)
            n += (size_t)snprintf(description + n, len - n, "%s", d);
        if (point->unit)
            n += (size_t)snprintf(description + n, len - n, "%s[%s]",
                                  n ? " " : "", point->unit);
        if (point->access == TDOT_ACCESS_WRITE)
            n += (size_t)snprintf(description + n, len - n, "%s%s", n ? " " : "",
                                  WRITE_ONLY);
        if (*description)
            cJSON_AddStringToObject(schema, "description", description);
        free(description);
    }

    const cJSON *opt_min = cJSON_GetObjectItemCaseSensitive(options, "min");
    const cJSON *opt_max = cJSON_GetObjectItemCaseSensitive(options, "max");
    if (cJSON_IsNumber(opt_min))
        cJSON_AddNumberToObject(schema, "minimum", opt_min->valuedouble);
    else if (has_range)
        cJSON_AddNumberToObject(schema, "minimum", min);
    if (cJSON_IsNumber(opt_max))
        cJSON_AddNumberToObject(schema, "maximum", opt_max->valuedouble);
    else if (has_range)
        cJSON_AddNumberToObject(schema, "maximum", max);

    static const char *passthrough[] = {"enum", "default", "order"};
    for (size_t i = 0; i < sizeof passthrough / sizeof *passthrough; i++) {
        const cJSON *v = cJSON_GetObjectItemCaseSensitive(options, passthrough[i]);
        if (v)
            cJSON_AddItemToObject(schema, passthrough[i], cJSON_Duplicate(v, 1));
    }
    if (!(point->access & TDOT_ACCESS_WRITE))
        cJSON_AddTrueToObject(schema, "readOnly");
    return schema;
}

/* "modbus_control_parameters" -> "Modbus control parameters" (only the first word is
 * capitalized, as in Rust's title_from_key). Caller frees. */
static char *title_from_key(const char *key) {
    /* Heap, not a fixed buffer: the key contains the device type (§3.1), which
     * is an arbitrary configured string. The old fixed buffer both truncated
     * where Rust does not AND could write its terminator one byte past the end,
     * because the word-start branch emits two characters in one iteration.
     * Every input character yields at most one output character ('_' becomes a
     * single space or nothing), so strlen(key) + 1 always fits. */
    size_t o = 0;
    char *out = malloc(strlen(key) + 1);
    if (!out)
        return NULL;
    bool first_word = true, word_start = true;
    for (const char *p = key; *p; p++) {
        if (*p == '_') {
            if (!word_start) {
                word_start = true;
                first_word = false;
            }
            continue;
        }
        if (word_start) {
            if (!first_word)
                out[o++] = ' ';
            out[o++] = first_word ? (char)toupper((unsigned char)*p) : *p;
            word_start = false;
            continue;
        }
        out[o++] = *p;
    }
    out[o] = '\0';
    return out;
}

cJSON *tdot_c8y_dtm_definitions(const tdot_config_t *cfg, const char *forced) {
    return tdot_c8y_dtm_definitions_across(&cfg, 1, forced);
}

cJSON *tdot_c8y_dtm_definitions_across(const tdot_config_t *const *cfgs,
                                       size_t ncfgs, const char *forced) {
    /* One entry per set, in configuration order; `properties` keeps the order
     * the points are configured in so the default `order` matches Rust. */
    cJSON *sets = cJSON_CreateObject(); /* set name -> properties object */
    /* set name -> protocol of the config that declared it first */
    cJSON *protocols = cJSON_CreateObject();
    for (size_t c = 0; c < ncfgs; c++) {
        const tdot_config_t *cfg = cfgs[c];
        for (size_t i = 0; i < cfg->ndevices; i++) {
            const tdot_device_t *dev = &cfg->devices[i];
            tdot_set_naming_t naming =
                tdot_param_naming(dev, cfg->protocol, forced);
            for (size_t j = 0; j < dev->npoints; j++) {
                const tdot_point_t *pt = &dev->points[j];
                cJSON *options = parameter_options(pt);
                if (!options)
                    continue;
                size_t nsets = 0;
                char **point_sets = sets_of(options, &naming, &nsets);
                for (size_t k = 0; k < nsets; k++) {
                    cJSON *props =
                        cJSON_GetObjectItemCaseSensitive(sets, point_sets[k]);
                    if (!props) {
                        props = cJSON_AddObjectToObject(sets, point_sets[k]);
                        cJSON_AddStringToObject(protocols, point_sets[k],
                                                cfg->protocol);
                    }
                    if (!cJSON_GetObjectItemCaseSensitive(props, pt->id)) /* first definition wins */
                        cJSON_AddItemToObject(props, pt->id,
                                              property_schema(pt, options));
                }
                tdot_param_sets_free(point_sets, nsets);
                cJSON_Delete(options);
            }
        }
    }

    cJSON *docs = cJSON_CreateArray();
    cJSON *props;
    cJSON_ArrayForEach(props, sets) {
        const char *protocol =
            cJSON_GetObjectItemCaseSensitive(protocols, props->string)->valuestring;
        cJSON *doc = cJSON_CreateObject();
        cJSON_AddStringToObject(doc, "identifier", props->string);

        cJSON *schema = cJSON_AddObjectToObject(doc, "jsonSchema");
        cJSON_AddStringToObject(schema, "$schema",
                                "http://json-schema.org/draft-07/schema#");
        char *title = title_from_key(props->string);
        cJSON_AddStringToObject(schema, "title", title ? title : props->string);
        free(title);
        static const char *DESC_FMT =
            "Writable %s points exposed by tedge-dot (generated from the "
            "connector configuration)";
        size_t dlen = strlen(DESC_FMT) + strlen(protocol) + 1;
        char *description = malloc(dlen);
        if (description) {
            snprintf(description, dlen, DESC_FMT, protocol);
            cJSON_AddStringToObject(schema, "description", description);
            free(description);
        }
        cJSON_AddStringToObject(schema, "type", "object");

        /* Properties without an explicit `order` get their 1-based position. */
        int position = 0;
        cJSON *prop;
        cJSON_ArrayForEach(prop, props) {
            position++;
            if (!cJSON_GetObjectItemCaseSensitive(prop, "order"))
                cJSON_AddNumberToObject(prop, "order", position);
        }
        cJSON_AddItemToObject(schema, "properties", cJSON_Duplicate(props, 1));

        cJSON *contexts = cJSON_AddArrayToObject(doc, "contexts");
        cJSON_AddItemToArray(contexts, cJSON_CreateString("asset"));
        cJSON_AddItemToArray(contexts, cJSON_CreateString("event"));
        cJSON_AddItemToArray(contexts, cJSON_CreateString("operation"));

        cJSON *tags = cJSON_AddArrayToObject(doc, "tags");
        cJSON_AddItemToArray(tags, cJSON_CreateString("tedge-dot"));
        cJSON_AddItemToArray(tags, cJSON_CreateString(protocol));

        cJSON_AddItemToArray(docs, doc);
    }
    cJSON_Delete(sets);
    return docs;
}
