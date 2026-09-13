/* tedge-dot C SDK — TOML config model.
 *
 * Mirrors impl/rust/crates/sdk/src/config.rs: [connector], [mqtt], [connection] (opaque,
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

/* Per-point output mode (contract §3.1): typed decodes a primitive, raw
 * publishes the wire bytes only. */
typedef enum {
    TDOT_MODE_TYPED = 0,
    TDOT_MODE_RAW,
} tdot_mode_t;

typedef struct tdot_point {
    char *id;
    tdot_datatype_t datatype;
    tdot_order_t endianness; /* default big */
    tdot_order_t word_order; /* default big */
    int access;              /* TDOT_ACCESS_* bits; default read */
    tdot_mode_t mode;        /* point.mode ?? device.default_mode ?? typed */
    char *unit;              /* optional */
    /* Human-readable labels (contract §3.1). `name` is a short display label,
     * `description` a longer explanation; the `id` stays a plain identifier
     * because it is a topic segment and a fragment key. They feed a
     * parameter's DTM title/description and the capability descriptor's
     * `point_labels` (§7) -- never a sample, since they are static. */
    char *name;              /* optional */
    char *description;       /* optional */
    tdot_transform_t transform;
    bool has_transform;
    char *meta_json; /* free-form [device.point.meta], serialized to JSON */
    bool subscribe;  /* default true: deliver by push when the module supports it */
    /* Resolved: point ?? device ?? connector. Drives the polling schedule AND,
     * for a subscribe-capable module, the per-point sampling-interval hint (an
     * OPC UA monitored item's samplingInterval). The Rust runtime resolves
     * PointRef::interval the same way, so the same config samples at the same
     * rate in both builds. */
    double poll_interval_s;
    toml_table_t *address;  /* protocol-specific, borrowed from the doc */

    /* Filled by the connector during configure(): */
    char *addr_json; /* address echo for the sample envelope ("addr") */
    void *proto;     /* connector's parsed address struct */

    /* Runtime state: */
    uint64_t seq;
    double next_due; /* monotonic seconds */
    /* Set by the runtime when the module accepted this point for push delivery
     * (contract §4.2). A subscribed point is off the polling schedule; it is
     * cleared whenever the device link drops, so the point falls back to
     * polling until the subscription is re-established on reconnect. */
    bool subscribed;
} tdot_point_t;

typedef enum {
    TDOT_LINK_UNKNOWN = 0,
    TDOT_LINK_CONNECTED,
    TDOT_LINK_DISCONNECTED,
    TDOT_LINK_DEGRADED,
} tdot_link_t;

typedef struct tdot_device {
    char *name;
    /* The device *type* this instance is one of (contract §3.1): what its point
     * list describes, not where it is. Declared on the device, or inherited
     * from the first point library it references (§3.4) -- a library is the
     * point list of one device type, so it is the natural place to name it.
     *
     * It qualifies the device's parameter set names (§5.2), which are
     * tenant-wide identifiers in the cloud, and is echoed in samples and link
     * status so the registration flow can use it as the entity type. NULL when
     * nothing declares one. */
    char *type;
    toml_table_t *protocol_address; /* protocol-specific, borrowed */
    double poll_interval_s;
    tdot_point_t *points;
    size_t npoints;
    /* Point libraries this device inherited its points from, in order
     * (contract §3.4, `points_from`). Resolved by the loader, so `points`
     * already holds the fully-merged list; kept only as a record of where it
     * came from. */
    char **points_from;
    size_t npoints_from;

    void *proto; /* connector per-device state (e.g. modbus_t*, UA_Client*) */

    /* Runtime state: */
    tdot_link_t link;
    double backoff_s;     /* current reconnect backoff */
    double reconnect_at;  /* monotonic deadline for next reconnect attempt */
} tdot_device_t;

typedef struct tdot_config {
    char *path;
    char *protocol;
    char *service_name; /* default "tedge-dot-<protocol>" */
    char *log_level;    /* default "info" */
    double poll_interval_s; /* default 2.0 */

    /* Liveness bounds (contract §8.1), mirroring the Rust runtime's
     * [connector] operation_timeout / stall_timeout.
     *
     * operation_timeout is the upper bound on ONE protocol-module call. The C
     * runtime cannot cancel a call in flight (a blocking libmodbus/open62541
     * call owns the thread), so instead of wrapping the call it pushes this
     * value down into the protocol library's own response timeout during
     * configure() — which is what actually makes a hung peer return.
     *
     * stall_timeout arms a watchdog over the poll loop's progress heartbeat:
     * if a loop stops making progress for this long, a protocol call is stuck
     * somewhere the response timeout does not cover and the process exits so
     * the service manager restarts it (see tdot_runtime_run_configs).
     * 0 disables the watchdog. Raised to 2x operation_timeout when set lower,
     * so one slow-but-legitimate call cannot cause a restart loop. */
    double operation_timeout_s; /* default 30.0 */
    double stall_timeout_s;     /* default 120.0; 0 disables */

    char *mqtt_host; /* default "127.0.0.1" */
    int mqtt_port;   /* default 1883 */

    toml_table_t *connection; /* protocol-specific, borrowed; may be NULL */

    tdot_device_t *devices;
    size_t ndevices;

    toml_table_t *root; /* owns all borrowed tables above */

    /* Parsed point-library documents (contract §3.4). A point's `address` is
     * borrowed from the document it was declared in, so every library a device
     * referenced must stay alive for as long as the config does. Libraries are
     * parsed once each and shared between the devices that reference them. */
    toml_table_t **libs;
    char **lib_paths; /* resolved path of libs[i], the cache key */
    size_t nlibs;
} tdot_config_t;

/* Load and validate one connector config, resolving the point libraries its
 * devices reference (contract §3.4, `points_from`). Returns NULL and fills err
 * on failure.
 *
 * Only the in-memory device/point lists are expanded: `cfg->root` keeps the
 * document exactly as it was written, references and all, so the management
 * verbs (§6.3) patch and persist a reference rather than baking a library's
 * points into the user's file. */
tdot_config_t *tdot_config_load(const char *path, char *err, size_t errlen);
void tdot_config_free(tdot_config_t *cfg);

/* Parse durations like "500ms", "2s", "5m", "2h" (also bare seconds).
 * Returns seconds, or -1.0 on parse failure. */
double tdot_duration_parse(const char *s);

/* True when a `points_from` entry names a path rather than a point library
 * (contract §3.4). The runtime uses it to refuse path references arriving
 * through a management command, which is a different trust boundary from a
 * config file: see tdot_runtime's handling of set-config/define-device. */
bool tdot_is_path_reference(const char *ref);

tdot_device_t *tdot_config_device(tdot_config_t *cfg, const char *name);
tdot_point_t *tdot_device_point(tdot_device_t *dev, const char *id);

/* The whole configuration document as a JSON string (tables -> objects,
 * arrays-of-tables -> arrays). Used by the management verbs, which patch the
 * document and write it back as TOML. Caller frees. */
char *tdot_config_root_json(const tdot_config_t *cfg);

/* What a configuration was loaded from, as one JSON string: its document and
 * every point library it resolved, in load order. Two configurations with the
 * same fingerprint resolve to the same devices and points, which is how a
 * reload (SIGHUP) tells an unchanged file -- a comment edit, say -- from one
 * that has to be applied. Caller frees. */
char *tdot_config_fingerprint(const tdot_config_t *cfg);

/* Free the connector-owned per-device/per-point state (dev->proto, pt->proto)
 * so the connector can be re-configured against the same config. Transports
 * must have been released with disconnect_device() first. */
void tdot_config_release_protos(tdot_config_t *cfg);

/* Move the contents of `src` into `dst` (freeing dst's previous contents and
 * the src struct), so every pointer to dst stays valid across a live reload.
 * dst keeps its own `path`. */
void tdot_config_replace(tdot_config_t *dst, tdot_config_t *src);

#ifdef __cplusplus
}
#endif

#endif /* TDOT_CONFIG_H */
