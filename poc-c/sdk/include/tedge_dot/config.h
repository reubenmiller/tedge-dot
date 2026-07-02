/* tedge-dot C SDK — TOML config model.
 *
 * Mirrors crates/sdk/src/config.rs: [connector], [mqtt], [connection] (opaque,
 * protocol-specific), [[device]] with opaque protocol_address, and
 * [[device.point]] with opaque address. Protocol-specific tables are kept as
 * borrowed toml_table_t pointers for the connector to interpret in
 * configure().
 */
#ifndef TDOT_CONFIG_H
#define TDOT_CONFIG_H

#include <stddef.h>
#include <stdint.h>

#include "model.h"
#include "toml.h"

#ifdef __cplusplus
extern "C" {
#endif

#define TDOT_ACCESS_READ 0x1
#define TDOT_ACCESS_WRITE 0x2

typedef struct tdot_point {
    char *id;
    tdot_datatype_t datatype;
    tdot_order_t endianness; /* default big */
    tdot_order_t word_order; /* default big */
    int access;              /* TDOT_ACCESS_* bits; default read */
    char *unit;              /* optional */
    tdot_transform_t transform;
    bool has_transform;
    char *meta_json; /* free-form [device.point.meta], serialized to JSON */
    bool subscribe;  /* default true (push delivery hint; PoC polls only) */
    double poll_interval_s; /* resolved: point ?? device ?? connector */
    toml_table_t *address;  /* protocol-specific, borrowed from the doc */

    /* Filled by the connector during configure(): */
    char *addr_json; /* address echo for the sample envelope ("addr") */
    void *proto;     /* connector's parsed address struct */

    /* Runtime state: */
    uint64_t seq;
    double next_due; /* monotonic seconds */
} tdot_point_t;

typedef enum {
    TDOT_LINK_UNKNOWN = 0,
    TDOT_LINK_CONNECTED,
    TDOT_LINK_DISCONNECTED,
    TDOT_LINK_DEGRADED,
} tdot_link_t;

typedef struct tdot_device {
    char *name;
    toml_table_t *protocol_address; /* protocol-specific, borrowed */
    double poll_interval_s;
    tdot_point_t *points;
    size_t npoints;

    void *proto; /* connector per-device state (e.g. modbus_t*, UA_Client*) */

    /* Runtime state: */
    tdot_link_t link;
    double backoff_s;     /* current reconnect backoff */
    double reconnect_at;  /* monotonic deadline for next reconnect attempt */
} tdot_device_t;

typedef struct tdot_config {
    char *path;
    char *protocol;
    char *service_name; /* default "tedge-dot" */
    char *log_level;    /* default "info" */
    double poll_interval_s; /* default 2.0 */

    char *mqtt_host; /* default "127.0.0.1" */
    int mqtt_port;   /* default 1883 */

    toml_table_t *connection; /* protocol-specific, borrowed; may be NULL */

    tdot_device_t *devices;
    size_t ndevices;

    toml_table_t *root; /* owns all borrowed tables above */
} tdot_config_t;

/* Load and validate one connector config. Returns NULL and fills err on
 * failure. */
tdot_config_t *tdot_config_load(const char *path, char *err, size_t errlen);
void tdot_config_free(tdot_config_t *cfg);

/* Parse durations like "500ms", "2s", "5m", "2h" (also bare seconds).
 * Returns seconds, or -1.0 on parse failure. */
double tdot_duration_parse(const char *s);

tdot_device_t *tdot_config_device(tdot_config_t *cfg, const char *name);
tdot_point_t *tdot_device_point(tdot_device_t *dev, const char *id);

#ifdef __cplusplus
}
#endif

#endif /* TDOT_CONFIG_H */
