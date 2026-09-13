/* tedge-dot C SDK — runtime: poll scheduler, envelopes, MQTT/stdout output.
 * Mirrors impl/rust/crates/sdk/src/runtime.rs.
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
    /* Lists the config paths again on a reload (SIGHUP), so configs added to or
     * removed from a directory start and stop their connectors. Returns 0 with
     * a heap array of heap strings (possibly empty), which the runtime frees,
     * or -1 to keep the running set unchanged. NULL: the paths never change. */
    int (*discover)(void *ctx, char ***paths, size_t *npaths);
    void *discover_ctx;
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
 * SIGHUP re-reads the config file and applies it in place; a change that
 * needs the connector restarted (another service name, protocol, broker or
 * stall timeout) is reported and not applied, since this function cannot
 * rebuild the connector it was given -- tdot_runtime_run_configs can.
 * Returns 0 on clean stop, -1 on fatal error. */
int tdot_runtime_run(tdot_connector_t *conn, tdot_config_t *cfg,
                     const tdot_run_opts_t *opts);

/* Run several connector configs concurrently in one process (one thread per
 * config), mirroring the single-service model of the packaged systemd unit.
 * Each config is loaded and its connector built here; invalid configs are
 * logged and skipped.
 *
 * SIGHUP reloads, as the Rust build does: every running connector re-reads
 * its file and applies what changed in place (an unchanged file is left
 * alone, an unusable one reported and the running configuration kept), a
 * connector whose change needs a restart is restarted, `opts->discover` (when
 * set) lists the paths again so new files start a connector and removed ones
 * stop theirs, and a config that failed to start is tried again.
 *
 * Returns once every connector has stopped: 0 if all stopped cleanly, -1
 * otherwise (including when no config was valid). */
int tdot_runtime_run_configs(const char *const *paths, size_t npaths,
                             const tdot_run_opts_t *opts);

/* ---- stall watchdog, internals exposed for tests --------------------------
 *
 * The watchdog thread itself cannot be driven from a test without a protocol
 * library that ignores its own timeout, so its two decisions are factored out
 * here and tested directly (impl/c/tests/config.c). What remains uncovered is
 * the thread plumbing and the _exit() call.
 */

/* Seconds a connector's loop has been stuck, or -1 when it has NOT stalled --
 * which includes a disabled slot (limit_s <= 0) and one that has not started
 * ticking yet (beat_ms == 0). Both must stay silent: a disabled watchdog must
 * never fire, and a loop that has not begun has not stalled. */
double tdot_runtime_stall_idle(long long beat_ms, long long now_ms,
                               double limit_s);

/* How often the watchdog should check, given every slot's limit: a quarter of
 * the tightest enabled limit, clamped to [0.5s, 10s]. Returns 0 when no slot
 * is enabled, i.e. no watchdog is needed. */
double tdot_runtime_watchdog_period(const double *limits, size_t n);

#ifdef __cplusplus
}
#endif

#endif /* TDOT_RUNTIME_H */
