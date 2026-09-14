/* Device-parameter derivation and Cumulocity DTM rendering.
 *
 * Holds the C implementation to the same assertions (and the same fixture
 * config) as the Rust SDK's descriptor tests in impl/rust/crates/sdk/src/descriptor.rs,
 * so `tedge-dot describe` renders the same definitions in both builds.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "cjson/cJSON.h"
#include "tedge_dot/descriptor.h"

static const char *CONFIG =
    "[connector]\n"
    "protocol = \"modbus\"\n"
    "\n"
    "[[device]]\n"
    "name = \"plc1\"\n"
    "type = \"acme-boiler-v2\"\n"
    "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", port = 502, unit_id = 1 }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"temp_u16\"\n"
    "  datatype = \"uint16\"\n"
    "  access = \"read_write\"\n"
    "  unit = \"°C\"\n"
    "  name = \"Boiler temp\"\n"
    "  description = \"Outlet temperature after the heat exchanger\"\n"
    "  address = { table = \"holding\", address = 3, count = 1 }\n"
    "  meta = { parameter = { title = \"Temperature setpoint\", min = 0, max = 100, order = 7 } }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"coil_rw\"\n"
    "  datatype = \"bool\"\n"
    "  access = \"read_write\"\n"
    "  name = \"Pump enable\"\n"
    "  address = { table = \"coil\", address = 48, count = 1 }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"pump_speed\"\n"
    "  datatype = \"float32\"\n"
    "  access = \"write\"\n"
    "  address = { table = \"holding\", address = 10, count = 2 }\n"
    "  meta = { parameter = \"pump\" }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"level_f32\"\n"
    "  datatype = \"float32\"\n"
    "  description = \"Level in the buffer tank\"\n"
    "  address = { table = \"holding\", address = 6, count = 2 }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"status_word\"\n"
    "  datatype = \"uint16\"\n"
    "  address = { table = \"holding\", address = 20, count = 1 }\n"
    "  meta = { parameter = true }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"hidden_rw\"\n"
    "  datatype = \"uint16\"\n"
    "  access = \"read_write\"\n"
    "  address = { table = \"holding\", address = 21, count = 1 }\n"
    "  meta = { parameter = false }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"commission_code\"\n"
    "  datatype = \"uint16\"\n"
    "  access = \"read_write\"\n"
    "  address = { table = \"holding\", address = 22, count = 1 }\n"
    "  meta = { parameter = { group = \"commissioning\" } }\n"
    "\n"
    "  # In two groups at once: both fragments carry its value.\n"
    "  [[device.point]]\n"
    "  id = \"flow_limit\"\n"
    "  datatype = \"uint16\"\n"
    "  access = \"read_write\"\n"
    "  address = { table = \"holding\", address = 23, count = 1 }\n"
    "  meta = { parameter = { group = [\"control\", \"commissioning\"] } }\n"
    "\n"
    "# A device of an undeclared type: its sets fall back to the protocol.\n"
    "[[device]]\n"
    "name = \"plc2\"\n"
    "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", port = 503, unit_id = 1 }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"spare_rw\"\n"
    "  datatype = \"uint16\"\n"
    "  access = \"read_write\"\n"
    "  address = { table = \"holding\", address = 30, count = 1 }\n";

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

static char *write_temp_config(const char *body) {
    static char path[] = "/tmp/tdot-describe-XXXXXX.toml";
    char template[] = "/tmp/tdot-describe-XXXXXX";
    char *dir = mkdtemp(template);
    if (!dir) {
        perror("mkdtemp");
        exit(2);
    }
    snprintf(path, sizeof path, "%s/modbus.toml", dir);
    FILE *fp = fopen(path, "w");
    fputs(body, fp);
    fclose(fp);
    return path;
}

static const cJSON *prop(const cJSON *def, const char *key) {
    return cJSON_GetObjectItemCaseSensitive(
        cJSON_GetObjectItemCaseSensitive(
            cJSON_GetObjectItemCaseSensitive(def, "jsonSchema"), "properties"),
        key);
}

static const char *str_of(const cJSON *obj, const char *key) {
    const cJSON *v = cJSON_GetObjectItemCaseSensitive(obj, key);
    return cJSON_IsString(v) ? v->valuestring : "<missing>";
}

static double num_of(const cJSON *obj, const char *key) {
    const cJSON *v = cJSON_GetObjectItemCaseSensitive(obj, key);
    return cJSON_IsNumber(v) ? v->valuedouble : -1e300;
}

/* Device types that derive the SAME set names are warned about; a type that
 * merely contains a comma, or the same type on two devices, is not a collision.
 * Mirrors descriptor.rs::folded_device_types_are_warned_about. */
static void check_type_collisions(void) {
    static const struct {
        const char *a;
        const char *b;
        int want;
        const char *needle;
    } cases[] = {
        {"acme-boiler-v2", "acme boiler v2", 1, "acme_boiler_v2_control_parameters"},
        {"acme-boiler-v2", "acme-boiler-v2", 0, NULL}, /* one type, two devices */
        {"acme-boiler-v2", "acme-boiler-v2-", 1, NULL}, /* trailing separator folds in */
        {"Acme, Inc. Meter", NULL, 0, NULL},           /* a comma is not two types */
        {"acme-boiler-v2", "other-type", 0, NULL},
        /* Nothing usable in the type: it folds away entirely. */
        {"日本語", NULL, 1, "no [A-Za-z0-9] character"},
        {"---", NULL, 1, "no [A-Za-z0-9] character"},
    };
    for (size_t i = 0; i < sizeof cases / sizeof *cases; i++) {
        char body[2048];
        char second[512] = "";
        if (cases[i].b)
            snprintf(second, sizeof second,
                     "[[device]]\nname = \"d2\"\ntype = \"%s\"\n"
                     "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
                     "port = 502, unit_id = 2 }\n"
                     "  [[device.point]]\n  id = \"p2\"\n  datatype = \"uint16\"\n"
                     "  access = \"read_write\"\n"
                     "  address = { table = \"holding\", address = 2, count = 1 }\n",
                     cases[i].b);
        snprintf(body, sizeof body,
                 "[connector]\nprotocol = \"modbus\"\n"
                 "[[device]]\nname = \"d1\"\ntype = \"%s\"\n"
                 "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", "
                 "port = 502, unit_id = 1 }\n"
                 "  [[device.point]]\n  id = \"p1\"\n  datatype = \"uint16\"\n"
                 "  access = \"read_write\"\n"
                 "  address = { table = \"holding\", address = 1, count = 1 }\n%s",
                 cases[i].a, second);
        char *path = write_temp_config(body);
        char err[256];
        tdot_config_t *cfg = tdot_config_load(path, err, sizeof err);
        if (!cfg) {
            CHECK(false, "case %zu did not load: %s", i, err);
            continue;
        }
        char *warning = tdot_param_type_warnings(cfg);
        CHECK((warning != NULL) == (cases[i].want != 0),
              "case %zu ('%s' vs '%s'): warning=%s", i, cases[i].a,
              cases[i].b ? cases[i].b : "<none>", warning ? warning : "<none>");
        if (warning && cases[i].needle)
            CHECK(strstr(warning, cases[i].needle) != NULL,
                  "case %zu must name the derived set, got: %s", i, warning);
        free(warning);
        tdot_config_free(cfg);
        unlink(path);
    }
}

/* Several configurations at once (`describe -c <dir>`): a set declared in two
 * files is ONE definition, device types folding together collide across files,
 * and the untyped-device warning is per protocol. Mirrors
 * descriptor.rs::definitions_and_warnings_span_every_config. */
static void check_across_configs(void) {
    char *modbus = strdup(write_temp_config(
        "[connector]\nprotocol = \"modbus\"\n"
        "[[device]]\nname = \"plc1\"\ntype = \"acme-boiler-v2\"\n"
        "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", port = 502, unit_id = 1 }\n"
        "  [[device.point]]\n  id = \"temp_u16\"\n  datatype = \"uint16\"\n"
        "  access = \"read_write\"\n  name = \"Boiler temp\"\n"
        "  address = { table = \"holding\", address = 3, count = 1 }\n"
        "[[device]]\nname = \"plc2\"\n"
        "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", port = 503, unit_id = 1 }\n"
        "  [[device.point]]\n  id = \"spare_rw\"\n  datatype = \"uint16\"\n"
        "  access = \"read_write\"\n"
        "  address = { table = \"holding\", address = 30, count = 1 }\n"));
    char *opcua = strdup(write_temp_config(
        "[connector]\nprotocol = \"opcua\"\n"
        "[[device]]\nname = \"boiler2\"\ntype = \"acme boiler v2\"\n"
        "protocol_address = { endpoint = \"opc.tcp://127.0.0.1:4840/\" }\n"
        "  [[device.point]]\n  id = \"temp_u16\"\n  datatype = \"uint16\"\n"
        "  access = \"read_write\"\n  name = \"Second title\"\n"
        "  address = { node_id = \"ns=2;s=Temp\" }\n"
        "  [[device.point]]\n  id = \"Tank.Level\"\n  datatype = \"float32\"\n"
        "  access = \"read_write\"\n"
        "  address = { node_id = \"ns=2;s=Level\" }\n"
        "[[device]]\nname = \"opc1\"\n"
        "protocol_address = { endpoint = \"opc.tcp://127.0.0.1:4841/\" }\n"
        "  [[device.point]]\n  id = \"spare_rw\"\n  datatype = \"uint16\"\n"
        "  access = \"read_write\"\n"
        "  address = { node_id = \"ns=2;s=Spare\" }\n"));
    char err[256];
    tdot_config_t *a = tdot_config_load(modbus, err, sizeof err);
    CHECK(a != NULL, "modbus config did not load: %s", err);
    tdot_config_t *b = tdot_config_load(opcua, err, sizeof err);
    CHECK(b != NULL, "opcua config did not load: %s", err);
    if (!a || !b)
        goto out;
    const tdot_config_t *both[] = {a, b};

    /* The second file's "acme boiler v2" folds to the same set name as the
     * first file's "acme-boiler-v2", so there is ONE merged definition for
     * both (a key they share keeps the first file's schema), plus each
     * protocol's own fallback set for its untyped device. */
    cJSON *docs = tdot_c8y_dtm_definitions_across(both, 2, NULL);
    CHECK(cJSON_GetArraySize(docs) == 3, "expected 3 definitions, got %d",
          cJSON_GetArraySize(docs));
    const cJSON *shared = cJSON_GetArrayItem(docs, 0);
    CHECK(strcmp(str_of(shared, "identifier"),
                 "acme_boiler_v2_control_parameters") == 0,
          "shared identifier = %s", str_of(shared, "identifier"));
    CHECK(strcmp(str_of(prop(shared, "temp_u16"), "title"), "Boiler temp") == 0,
          "the first file's definition of a key must win, got title %s",
          str_of(prop(shared, "temp_u16"), "title"));
    CHECK(prop(shared, "Tank_Level") == NULL && prop(shared, "Tank.Level") != NULL,
          "the second file's extra key must be merged into the shared set");
    CHECK(strcmp(cJSON_GetArrayItem(cJSON_GetObjectItemCaseSensitive(shared, "tags"), 1)
                     ->valuestring,
                 "modbus") == 0,
          "a shared set is tagged with the protocol that declared it first");
    const cJSON *opc = cJSON_GetArrayItem(docs, 2);
    CHECK(strcmp(str_of(opc, "identifier"), "opcua_control_parameters") == 0,
          "third identifier = %s", str_of(opc, "identifier"));
    CHECK(strcmp(cJSON_GetArrayItem(cJSON_GetObjectItemCaseSensitive(opc, "tags"), 1)
                     ->valuestring,
                 "opcua") == 0,
          "a set is tagged with its own protocol");
    cJSON_Delete(docs);

    /* Neither file collides on its own; together they do. */
    char *warning = tdot_param_type_warnings(a);
    CHECK(warning == NULL, "one file alone has no collision, got: %s", warning);
    free(warning);
    warning = tdot_param_type_warnings_across(both, 2);
    CHECK(warning && strstr(warning, "'acme-boiler-v2', 'acme boiler v2'"),
          "types folding together across files must be warned about, got: %s",
          warning ? warning : "<none>");
    free(warning);

    char *untyped = tdot_param_untyped_devices_across(both, 2, "modbus");
    CHECK(untyped && strcmp(untyped, "plc2") == 0, "modbus untyped = %s",
          untyped ? untyped : "<none>");
    free(untyped);
    untyped = tdot_param_untyped_devices_across(both, 2, "opcua");
    CHECK(untyped && strcmp(untyped, "opc1") == 0, "opcua untyped = %s",
          untyped ? untyped : "<none>");
    free(untyped);

    char *bad = tdot_param_invalid_keys_across(both, 2, NULL);
    CHECK(bad && strcmp(bad, "point id 'Tank.Level'") == 0,
          "an invalid key in the second file must be reported, got: %s",
          bad ? bad : "<none>");
    free(bad);

out:
    tdot_config_free(a);
    tdot_config_free(b);
    unlink(modbus);
    unlink(opcua);
    free(modbus);
    free(opcua);
}

/* meta.parameter.key: the key a point has inside its sets, so a point keeps an
 * id that is unique on the device and still carries a conventional key. The
 * definition is keyed by it (a point without a label is titled by it), a usable
 * key frees the id from the key rule, and two points of a device sharing a key
 * in a set, or a key on a write-only point, are refused. Mirrors
 * descriptor.rs::a_point_can_name_its_own_key. */
static const char *KEYED =
    "[connector]\n"
    "protocol = \"modbus\"\n"
    "\n"
    "[[device]]\n"
    "name = \"plc1\"\n"
    "type = \"zephyr\"\n"
    "protocol_address = { transport = \"tcp\", host = \"127.0.0.1\", port = 502, unit_id = 1 }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"firmwareName\"\n"
    "  datatype = \"uint16\"\n"
    "  name = \"Firmware name\"\n"
    "  address = { table = \"holding\", address = 1, count = 1 }\n"
    "  meta = { parameter = { set = \"firmware\", key = \"name\" } }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"firmwareVersion\"\n"
    "  datatype = \"uint16\"\n"
    "  address = { table = \"holding\", address = 2, count = 1 }\n"
    "  meta = { parameter = { set = \"firmware\", key = \"version\" } }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"Tank.Level\"\n"
    "  datatype = \"uint16\"\n"
    "  address = { table = \"holding\", address = 5, count = 1 }\n"
    "  meta = { parameter = { key = \"tank_level\" } }\n";

/* Appended to KEYED: a second point with the key `name` in `firmware`, a key on
 * a write-only point, and an unusable key. */
static const char *KEYED_CONFLICTS =
    "\n"
    "  [[device.point]]\n"
    "  id = \"bootName\"\n"
    "  datatype = \"uint16\"\n"
    "  address = { table = \"holding\", address = 3, count = 1 }\n"
    "  meta = { parameter = { set = \"firmware\", key = \"name\" } }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"update_cmd\"\n"
    "  datatype = \"uint16\"\n"
    "  access = \"write\"\n"
    "  address = { table = \"holding\", address = 4, count = 1 }\n"
    "  meta = { parameter = { set = \"firmware\", key = \"update\" } }\n"
    "\n"
    "  [[device.point]]\n"
    "  id = \"level\"\n"
    "  datatype = \"uint16\"\n"
    "  address = { table = \"holding\", address = 6, count = 1 }\n"
    "  meta = { parameter = { key = \"lev.el\" } }\n";

static void check_parameter_keys(void) {
    char err[256];
    char *path = write_temp_config(KEYED);
    tdot_config_t *cfg = tdot_config_load(path, err, sizeof err);
    CHECK(cfg != NULL, "keyed config did not load: %s", err);
    if (cfg) {
        char *bad = tdot_param_invalid_keys(cfg, NULL);
        CHECK(bad == NULL, "a usable key frees the id from the key rule, got: %s",
              bad ? bad : "");
        free(bad);
        char *conflicts = tdot_param_key_conflicts(cfg, NULL);
        CHECK(conflicts == NULL, "keyed fixture reported conflicts: %s",
              conflicts ? conflicts : "");
        free(conflicts);

        cJSON *docs = tdot_c8y_dtm_definitions(cfg, NULL);
        const cJSON *fw = cJSON_GetArrayItem(docs, 0);
        const cJSON *name = fw ? prop(fw, "name") : NULL;
        const cJSON *version = fw ? prop(fw, "version") : NULL;
        CHECK(fw && strcmp(str_of(fw, "identifier"), "firmware") == 0,
              "the first definition must be the firmware set");
        CHECK(name && version && !prop(fw, "firmwareName"),
              "the firmware set must be keyed by name and version, not the ids");
        CHECK(name && strcmp(str_of(name, "title"), "Firmware name") == 0,
              "name title = %s", name ? str_of(name, "title") : "<none>");
        CHECK(version && strcmp(str_of(version, "title"), "version") == 0,
              "a point without a label must be titled by its key, got %s",
              version ? str_of(version, "title") : "<none>");
        const cJSON *control = cJSON_GetArrayItem(docs, 1);
        CHECK(control && prop(control, "tank_level") && !prop(control, "Tank.Level"),
              "Tank.Level must be rendered under its key");
        cJSON_Delete(docs);
        tdot_config_free(cfg);
    }
    unlink(path);

    size_t n = strlen(KEYED) + strlen(KEYED_CONFLICTS) + 1;
    char *body = malloc(n);
    snprintf(body, n, "%s%s", KEYED, KEYED_CONFLICTS);
    path = write_temp_config(body);
    free(body);
    cfg = tdot_config_load(path, err, sizeof err);
    CHECK(cfg != NULL, "conflicting keyed config did not load: %s", err);
    if (cfg) {
        char *bad = tdot_param_invalid_keys(cfg, NULL);
        CHECK(bad && strcmp(bad, "parameter key 'lev.el' of point 'level'") == 0,
              "an unusable key must be reported, got: %s", bad ? bad : "<none>");
        free(bad);
        char *conflicts = tdot_param_key_conflicts(cfg, NULL);
        CHECK(conflicts &&
                  strcmp(conflicts,
                         "key 'name' of points 'firmwareName' and 'bootName' in set "
                         "'firmware' on device 'plc1', key 'update' of write-only "
                         "point 'update_cmd' on device 'plc1'") == 0,
              "conflicts = %s", conflicts ? conflicts : "<none>");
        free(conflicts);
        tdot_config_free(cfg);
    }
    unlink(path);
}

int main(void) {
    char *path = write_temp_config(CONFIG);
    char err[256];
    tdot_config_t *cfg = tdot_config_load(path, err, sizeof err);
    if (!cfg) {
        printf("FAIL: cannot load fixture config: %s\n", err);
        return 1;
    }

    /* ---- set naming + key validation ------------------------------------- */
    /* A set name is qualified by the device *type* -- what decides which points
     * exist -- and only falls back to the protocol when none is declared. */
    char *opcua_set =
        tdot_param_set_name("opc-ua", TDOT_PARAM_DEFAULT_GROUP);
    CHECK(strcmp(opcua_set, "opc_ua_control_parameters") == 0, "opc-ua set = %s",
          opcua_set);
    free(opcua_set);
    char *pump_set = tdot_param_set_name("ACME meter v2", "pump");
    CHECK(strcmp(pump_set, "ACME_meter_v2_pump_parameters") == 0,
          "grouped set = %s", pump_set);
    free(pump_set);
    /* A run of separators (or one multi-byte character) folds to a single '_',
     * which is what keeps this identical to the Rust implementation's char-wise
     * fold. Mirrors descriptor.rs::key_validation_and_set_names. */
    char *run_set = tdot_param_set_name("acme -- v2", "control");
    CHECK(strcmp(run_set, "acme_v2_control_parameters") == 0, "run set = %s",
          run_set);
    free(run_set);
    char *utf8_set = tdot_param_set_name("wärmezähler", "control");
    CHECK(strcmp(utf8_set, "w_rmez_hler_control_parameters") == 0,
          "utf-8 set = %s", utf8_set);
    free(utf8_set);
    CHECK(cfg->devices[0].type && strcmp(cfg->devices[0].type, "acme-boiler-v2") == 0,
          "plc1 type = %s", cfg->devices[0].type ? cfg->devices[0].type : "<none>");
    CHECK(cfg->devices[1].type == NULL, "plc2 must have no type");
    CHECK(tdot_param_key_valid("ok_id_1"), "ok_id_1 should be a valid key");
    CHECK(!tdot_param_key_valid("Environment.Temperature"),
          "a dotted id must be rejected");
    CHECK(!tdot_param_key_valid(""), "an empty id must be rejected");

    /* ---- which points are parameters, and in which set ------------------- */
    struct {
        const char *point;
        const char *set;
    } want[] = {
        {"temp_u16", "acme_boiler_v2_control_parameters"},
        {"coil_rw", "acme_boiler_v2_control_parameters"},
        {"pump_speed", "pump"}, /* meta.parameter = "<name>" is absolute */
        {"status_word", "acme_boiler_v2_control_parameters"},
        {"commission_code", "acme_boiler_v2_commissioning_parameters"},
        /* One point, two groups -> one entry per set, in the declared order. */
        {"flow_limit", "acme_boiler_v2_control_parameters"},
        {"flow_limit", "acme_boiler_v2_commissioning_parameters"},
        /* plc2 declares no type: back to the protocol, which is what collides
         * across device types and is the reason `describe` warns about it. */
        {"spare_rw", "modbus_control_parameters"},
    };
    size_t nwant = sizeof want / sizeof *want, found = 0;
    for (size_t i = 0; i < cfg->ndevices; i++) {
        tdot_set_naming_t naming =
            tdot_param_naming(&cfg->devices[i], cfg->protocol, NULL);
        for (size_t j = 0; j < cfg->devices[i].npoints; j++) {
            const tdot_point_t *pt = &cfg->devices[i].points[j];
            size_t nsets = 0;
            char **sets = tdot_param_sets(pt, &naming, &nsets);
            if (!sets)
                continue;
            for (size_t k = 0; k < nsets; k++, found++) {
                if (found >= nwant)
                    continue;
                CHECK(strcmp(pt->id, want[found].point) == 0,
                      "parameter #%zu is %s, wanted %s", found, pt->id,
                      want[found].point);
                CHECK(strcmp(sets[k], want[found].set) == 0,
                      "%s set is %s, wanted %s", pt->id, sets[k],
                      want[found].set);
            }
            tdot_param_sets_free(sets, nsets);
        }
    }
    CHECK(found == nwant, "%zu parameters derived, wanted %zu", found, nwant);

    /* The untyped device is the one `describe` warns about. */
    char *untyped = tdot_param_untyped_devices(cfg);
    CHECK(untyped && strcmp(untyped, "plc2") == 0, "untyped devices = %s",
          untyped ? untyped : "<none>");
    free(untyped);

    /* Invalid keys are reported, valid ones are not. */
    char *bad = tdot_param_invalid_keys(cfg, NULL);
    CHECK(bad == NULL, "fixture reported invalid keys: %s", bad ? bad : "");
    free(bad);
    bad = tdot_param_invalid_keys(cfg, "plant.floor");
    CHECK(bad && strstr(bad, "parameter set 'plant.floor'"),
          "a dotted default set must be reported, got '%s'", bad ? bad : "");
    free(bad);
    free(cfg->devices[0].points[0].id);
    cfg->devices[0].points[0].id = strdup("Boiler.Temp");
    bad = tdot_param_invalid_keys(cfg, NULL);
    CHECK(bad && strcmp(bad, "point id 'Boiler.Temp'") == 0,
          "a dotted point id must be reported, got '%s'", bad ? bad : "");
    free(bad);
    free(cfg->devices[0].points[0].id);
    cfg->devices[0].points[0].id = strdup("temp_u16");

    /* ---- DTM rendering --------------------------------------------------- */
    cJSON *docs = tdot_c8y_dtm_definitions(cfg, NULL);
    CHECK(cJSON_GetArraySize(docs) == 4, "%d definitions, wanted 4",
          cJSON_GetArraySize(docs));
    const cJSON *main_def = cJSON_GetArrayItem(docs, 0);
    CHECK(strcmp(str_of(main_def, "identifier"),
                 "acme_boiler_v2_control_parameters") == 0,
          "identifier = %s", str_of(main_def, "identifier"));
    const cJSON *contexts = cJSON_GetObjectItemCaseSensitive(main_def, "contexts");
    CHECK(cJSON_GetArraySize(contexts) == 3 &&
              strcmp(cJSON_GetArrayItem(contexts, 0)->valuestring, "asset") == 0 &&
              strcmp(cJSON_GetArrayItem(contexts, 1)->valuestring, "event") == 0 &&
              strcmp(cJSON_GetArrayItem(contexts, 2)->valuestring,
                     "operation") == 0,
          "contexts must be [asset, event, operation]");

    const cJSON *temp = prop(main_def, "temp_u16");
    CHECK(strcmp(str_of(temp, "type"), "integer") == 0, "temp_u16 type = %s",
          str_of(temp, "type"));
    CHECK(strcmp(str_of(temp, "title"), "Temperature setpoint") == 0,
          "temp_u16 title = %s", str_of(temp, "title"));
    CHECK(num_of(temp, "minimum") == 0.0, "temp_u16 minimum = %g",
          num_of(temp, "minimum"));
    CHECK(num_of(temp, "maximum") == 100.0, "temp_u16 maximum = %g",
          num_of(temp, "maximum"));
    CHECK(num_of(temp, "order") == 7, "temp_u16 order = %g",
          num_of(temp, "order"));
    /* meta.parameter.title wins over the point's `name` (asserted above), while
     * the point's own `description` is used -- there is no
     * meta.parameter.description here -- and still composes with the unit. */
    CHECK(strcmp(str_of(temp, "description"),
                 "Outlet temperature after the heat exchanger [°C]") == 0,
          "temp_u16 description = %s", str_of(temp, "description"));

    const cJSON *coil = prop(main_def, "coil_rw");
    /* With no meta at all, the point's `name` becomes the title. */
    CHECK(strcmp(str_of(coil, "title"), "Pump enable") == 0, "coil_rw title = %s",
          str_of(coil, "title"));
    CHECK(strcmp(str_of(coil, "type"), "boolean") == 0, "coil_rw type = %s",
          str_of(coil, "type"));
    CHECK(num_of(coil, "order") == 2, "coil_rw order = %g",
          num_of(coil, "order"));
    CHECK(!cJSON_GetObjectItemCaseSensitive(coil, "minimum"),
          "a boolean must carry no range");

    const cJSON *status = prop(main_def, "status_word");
    CHECK(cJSON_IsTrue(cJSON_GetObjectItemCaseSensitive(status, "readOnly")),
          "an opted-in read-only point must be readOnly");
    CHECK(num_of(status, "maximum") == 65535.0, "status_word maximum = %g",
          num_of(status, "maximum"));

    /* The two-group point is a property of BOTH its sets, so either screen edits it. */
    CHECK(prop(main_def, "flow_limit") != NULL,
          "a two-group point must be in the control set");
    CHECK(!prop(main_def, "hidden_rw"), "meta.parameter = false must opt out");
    CHECK(!prop(main_def, "level_f32"), "a read-only point must not appear");

    const cJSON *pump = cJSON_GetArrayItem(docs, 1);
    CHECK(strcmp(str_of(pump, "identifier"), "pump") == 0, "identifier = %s",
          str_of(pump, "identifier"));
    CHECK(strcmp(str_of(cJSON_GetObjectItemCaseSensitive(pump, "jsonSchema"),
                        "title"),
                 "Pump") == 0,
          "pump title = %s",
          str_of(cJSON_GetObjectItemCaseSensitive(pump, "jsonSchema"), "title"));
    const cJSON *speed = prop(pump, "pump_speed");
    CHECK(strcmp(str_of(speed, "type"), "number") == 0, "pump_speed type = %s",
          str_of(speed, "type"));
    CHECK(strstr(str_of(speed, "description"), "write-only") != NULL,
          "a write-only point must say so: %s", str_of(speed, "description"));

    /* One device type, two sets (the group), and one untyped device on the
     * protocol name. */
    CHECK(strcmp(str_of(cJSON_GetArrayItem(docs, 2), "identifier"),
                 "acme_boiler_v2_commissioning_parameters") == 0,
          "grouped identifier = %s",
          str_of(cJSON_GetArrayItem(docs, 2), "identifier"));
    CHECK(prop(cJSON_GetArrayItem(docs, 2), "flow_limit") != NULL,
          "a two-group point must be in the commissioning set too");
    CHECK(strcmp(str_of(cJSON_GetArrayItem(docs, 3), "identifier"),
                 "modbus_control_parameters") == 0,
          "untyped identifier = %s",
          str_of(cJSON_GetArrayItem(docs, 3), "identifier"));
    CHECK(prop(cJSON_GetArrayItem(docs, 3), "spare_rw") != NULL,
          "the untyped device's parameter must be rendered");
    cJSON_Delete(docs);

    /* The title of a multi-word set capitalizes only the first word, and an
     * explicit --set overrides every derived name. */
    docs = tdot_c8y_dtm_definitions(cfg, "plant_settings");
    CHECK(strcmp(str_of(cJSON_GetArrayItem(docs, 0), "identifier"),
                 "plant_settings") == 0,
          "overridden identifier = %s",
          str_of(cJSON_GetArrayItem(docs, 0), "identifier"));
    CHECK(strcmp(str_of(cJSON_GetObjectItemCaseSensitive(
                            cJSON_GetArrayItem(docs, 0), "jsonSchema"),
                        "title"),
                 "Plant settings") == 0,
          "overridden title = %s",
          str_of(cJSON_GetObjectItemCaseSensitive(cJSON_GetArrayItem(docs, 0),
                                                  "jsonSchema"),
                 "title"));
    cJSON_Delete(docs);

    tdot_config_free(cfg);
    unlink(path);
    check_type_collisions();
    check_across_configs();
    check_parameter_keys();

    if (failures) {
        printf("describe: %d check(s) failed\n", failures);
        return 1;
    }
    puts("describe: all checks passed");
    return 0;
}
