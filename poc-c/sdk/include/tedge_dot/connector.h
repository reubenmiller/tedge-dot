/* tedge-dot C SDK — connector interface (C rendering of the Rust
 * `Connector` trait, crates/sdk/src/connector.rs).
 *
 * A connector is a vtable of function pointers plus opaque state. Protocol
 * modules provide a factory returning a heap-allocated tdot_connector_t;
 * tdot_connector_factory() selects one by protocol name (compile-time
 * feature-gated, like the Rust cargo features).
 */
#ifndef TDOT_CONNECTOR_H
#define TDOT_CONNECTOR_H

#include "config.h"
#include "model.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct tdot_connector tdot_connector_t;

struct tdot_connector {
    const char *protocol;
    /* JSON capability descriptor published retained on startup. */
    const char *capabilities_json;
    void *state;

    /* Parse protocol-specific config ([connection], device.protocol_address,
     * point.address). Must fill point->proto and point->addr_json.
     * Returns 0 on success, -1 with err filled on invalid config. */
    int (*configure)(tdot_connector_t *self, tdot_config_t *cfg, char *err,
                     size_t errlen);

    /* Establish transport for one device (fills device->proto).
     * Returns 0 on success, -1 with err filled. Also used for reconnect. */
    int (*connect_device)(tdot_connector_t *self, tdot_device_t *dev,
                          char *err, size_t errlen);

    /* Read one point. Always fills *out (bad samples carry an error reason).
     * Returns 0 when the transport is healthy, -1 when the failure indicates
     * the device link is down (triggers the runtime's reconnect backoff). */
    int (*read_point)(tdot_connector_t *self, tdot_device_t *dev,
                      tdot_point_t *pt, tdot_sample_t *out);

    /* Execute a typed "write" command. Returns 0 on success, -1 with err. */
    int (*write_point)(tdot_connector_t *self, tdot_device_t *dev,
                       tdot_point_t *pt, const tdot_value_t *value, char *err,
                       size_t errlen);

    /* Close one device's transport (frees device->proto). */
    void (*disconnect_device)(tdot_connector_t *self, tdot_device_t *dev);

    /* Free the connector itself (per-point proto state included). */
    void (*destroy)(tdot_connector_t *self);
};

/* Protocol module factories (feature-gated at compile time). */
#ifdef TDOT_FEATURE_MODBUS
tdot_connector_t *tdot_connector_modbus_new(void);
#endif
#ifdef TDOT_FEATURE_OPCUA
tdot_connector_t *tdot_connector_opcua_new(void);
#endif

/* Returns NULL when the protocol is unknown or compiled out. */
tdot_connector_t *tdot_connector_factory(const char *protocol);

#ifdef __cplusplus
}
#endif

#endif /* TDOT_CONNECTOR_H */
