/* tedge-dot C SDK — runtime: poll scheduler, envelopes, MQTT/stdout output.
 * Mirrors crates/sdk/src/runtime.rs.
 */
#ifndef TDOT_RUNTIME_H
#define TDOT_RUNTIME_H

#include "config.h"
#include "connector.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef enum {
    TDOT_OUTPUT_MQTT = 0,
    TDOT_OUTPUT_STDOUT,
} tdot_output_t;

typedef struct {
    tdot_output_t output;
    double duration_s; /* 0 = run forever */
} tdot_run_opts_t;

/* Build the sample envelope JSON for one read result. Caller frees. */
char *tdot_envelope_sample(const tdot_config_t *cfg, const tdot_device_t *dev,
                           const tdot_point_t *pt, const tdot_sample_t *s);

/* Monotonic clock (seconds) and wall-clock helpers. */
double tdot_mono(void);
/* RFC 3339 ms-precision UTC, e.g. "2026-07-02T10:00:00.000Z". */
void tdot_now_rfc3339(char *dst, size_t dstlen);
double tdot_now_ms(void);

/* Run one connector until the duration elapses or SIGINT/SIGTERM.
 * configure() must not have been called yet; the runtime drives the full
 * lifecycle (configure -> connect -> poll/commands -> disconnect).
 * Returns 0 on clean stop, -1 on fatal error. */
int tdot_runtime_run(tdot_connector_t *conn, tdot_config_t *cfg,
                     const tdot_run_opts_t *opts);

#ifdef __cplusplus
}
#endif

#endif /* TDOT_RUNTIME_H */
