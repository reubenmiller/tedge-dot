#include "tedge_dot/runtime.h"

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "cjson/cJSON.h"
#include "mosquitto.h"

#define TICK_MS 200
#define BACKOFF_INITIAL_S 1.0
#define BACKOFF_MAX_S 60.0
/* Retry interval while the MQTT broker is unreachable (the Rust runtime's too). */
#define MQTT_RETRY_S 1.0

/* ---- liveness ------------------------------------------------------------
 *
 * The runtime bounds protocol calls by pushing connector.operation_timeout down
 * into the protocol library's own response timeout (see each module's
 * configure()). That covers a peer that stops answering, which is the common
 * case, but not a call that wedges INSIDE a library: this runtime is one thread
 * per config and cannot cancel a call in flight the way the Rust runtime's
 * tokio::time::timeout can.
 *
 * connector.stall_timeout closes that gap from the outside. Every poll loop
 * stamps a heartbeat each tick; a watchdog thread notices when one stops
 * advancing and exits the process, so the service manager restarts it
 * (packaging/tedge-dot.service sets Restart=always). That is coarser than the
 * Rust behaviour -- which cancels and restarts the one wedged connector while
 * the others keep running -- because a pthread stuck in a blocking library call
 * cannot be safely cancelled. Documented in impl/c/README.md's parity table. */
typedef struct {
    /* tdot_mono() in milliseconds at the last loop tick; 0 before the loop
     * starts, which the watchdog treats as "not running yet". */
    _Atomic long long beat_ms;
    double limit_s; /* connector.stall_timeout; 0 disables this slot */
    const char *name; /* config path, for the log line */
} progress_t;

/* Two flags for two readers, because they have different requirements.
 *
 * g_stop is written by the signal handler and read by the poll loops; sig_atomic_t is exactly
 * what a handler may touch, and each loop runs on the thread whose work it is stopping.
 *
 * g_stop_threads is the same signal for the WATCHDOG thread, which is a genuine cross-thread
 * read and also has a second writer: the supervisor sets it after the workers have joined, to
 * wake the watchdog out of its sleep so shutdown does not wait a whole check period. Both are
 * set together by the handler, so they never disagree about whether a stop was requested. */
static volatile sig_atomic_t g_stop = 0;
static _Atomic int g_stop_threads = 0;

static void on_signal(int sig) {
    (void)sig;
    g_stop = 1;
    atomic_store(&g_stop_threads, 1);
}

/* Bumped by SIGHUP, the conventional "reload your configuration" signal (what
 * `systemctl reload` sends through the unit's ExecReload). A generation rather
 * than a flag, because one signal has several readers that each act on it
 * once: every connector thread re-reads its own file when the generation has
 * moved past the one it last saw, and the supervisor lists the config paths
 * again. A lock-free atomic, so the handler may update it. */
static _Atomic unsigned g_reload_gen = 0;

static void on_hangup(int sig) {
    (void)sig;
    atomic_fetch_add(&g_reload_gen, 1);
}

typedef struct {
    tdot_connector_t *conn;
    tdot_config_t *cfg;
    struct mosquitto *mosq; /* NULL in stdout mode */
    tdot_output_t output;
    progress_t *progress; /* NULL when no watchdog is armed */
    bool mqtt_up;         /* false from a failed loop until the next CONNACK */
    bool mqtt_resume;     /* the next CONNACK is a reconnect: restore the session */
    double mqtt_retry_at; /* tdot_mono() of the next reconnect attempt */
} rt_t;

static void logmsg(const char *level, const char *fmt, ...) {
    char ts[40];
    tdot_now_rfc3339(ts, sizeof ts);
    fprintf(stderr, "%s %-5s ", ts, level);
    va_list ap;
    va_start(ap, fmt);
    vfprintf(stderr, fmt, ap);
    va_end(ap);
    fputc('\n', stderr);
}

static void publish(rt_t *rt, const char *topic, const char *payload,
                    bool retained) {
    if (rt->output == TDOT_OUTPUT_STDOUT) {
        /* samples go to stdout; everything else is log-only */
        return;
    }
    mosquitto_publish(rt->mosq, NULL, topic, (int)strlen(payload), payload, 0,
                      retained);
}

static void emit_sample(rt_t *rt, tdot_device_t *dev, tdot_point_t *pt,
                        const tdot_sample_t *s) {
    pt->seq++;
    char *json = tdot_envelope_sample(rt->cfg, dev, pt, s);
    if (!json)
        return;
    if (rt->output == TDOT_OUTPUT_STDOUT) {
        puts(json);
        fflush(stdout);
    } else {
        char topic[256];
        snprintf(topic, sizeof topic, "te/device/%s/ot/%s/sample/%s",
                 dev->name, rt->cfg->protocol, pt->id);
        mosquitto_publish(rt->mosq, NULL, topic, (int)strlen(json), json, 0,
                          false);
    }
    free(json);
}

/* Publish the device's current link status (retained), whether or not it
 * changed: also used to restore it on a broker that lost its retained messages. */
static void publish_link_status(rt_t *rt, tdot_device_t *dev) {
    const char *name = dev->link == TDOT_LINK_CONNECTED   ? "connected"
                       : dev->link == TDOT_LINK_DEGRADED  ? "degraded"
                                                          : "disconnected";
    char ts[40];
    tdot_now_rfc3339(ts, sizeof ts);
    char topic[256];
    snprintf(topic, sizeof topic, "te/device/%s/ot/%s/status/link", dev->name,
             rt->cfg->protocol);

    cJSON *obj = cJSON_CreateObject();
    cJSON_AddStringToObject(obj, "status", name);
    /* The device type (§3.1), so the registration flow can use it as the
     * thin-edge entity type without reading the connector config. */
    if (dev->type)
        cJSON_AddStringToObject(obj, "type", dev->type);
    /* Every point configured on the device, so a consumer keeping per-point
     * state (the parameter twin) can drop the ones a reload removed: a
     * write-only point never samples, so missing samples cannot tell it. */
    cJSON *points = cJSON_AddArrayToObject(obj, "points");
    for (size_t j = 0; points && j < dev->npoints; j++)
        cJSON_AddItemToArray(points, cJSON_CreateString(dev->points[j].id));
    cJSON_AddStringToObject(obj, "since", ts);
    /* Optional device descriptor from the module (contract status schema
     * `info`); the registration flow forwards it into a twin fragment. */
    if (rt->conn->device_info) {
        char *info = rt->conn->device_info(rt->conn, dev);
        if (info) {
            cJSON *parsed = cJSON_Parse(info);
            if (parsed && cJSON_IsObject(parsed))
                cJSON_AddItemToObject(obj, "info", parsed);
            else
                cJSON_Delete(parsed);
            free(info);
        }
    }
    char *payload = cJSON_PrintUnformatted(obj);
    publish(rt, topic, payload, true);
    free(payload);
    cJSON_Delete(obj);
}

static void publish_link(rt_t *rt, tdot_device_t *dev, tdot_link_t status) {
    if (dev->link == status)
        return;
    dev->link = status;
    logmsg("info", "device %s: link %s", dev->name,
           status == TDOT_LINK_CONNECTED  ? "connected"
           : status == TDOT_LINK_DEGRADED ? "degraded"
                                          : "disconnected");
    publish_link_status(rt, dev);
}

static void publish_health(rt_t *rt, const char *status) {
    char ts[40];
    tdot_now_rfc3339(ts, sizeof ts);
    char topic[256], payload[128];
    snprintf(topic, sizeof topic, "te/device/main/service/%s/status/health",
             rt->cfg->service_name);
    snprintf(payload, sizeof payload, "{\"status\":\"%s\",\"time\":\"%s\"}",
             status, ts);
    publish(rt, topic, payload, true);
}

/* Drop every push subscription flag, putting the device's points back on the
 * polling schedule. Called whenever the link goes down: the module's
 * subscription died with the transport, and subscribe_device() will re-arm on
 * the next successful connect. */
static void clear_subscriptions(tdot_device_t *dev) {
    for (size_t j = 0; j < dev->npoints; j++)
        dev->points[j].subscribed = false;
}

/* Ask the module for push delivery on a freshly connected device. Failure is
 * not fatal: the points simply stay on the polling schedule, which is the
 * contract's own fallback (§4.2) and keeps a subscription-hostile server
 * working. */
static void arm_subscriptions(rt_t *rt, tdot_device_t *dev) {
    clear_subscriptions(dev);
    if (!rt->conn->subscribe_device || !rt->conn->drain_subscriptions)
        return;
    char err[TDOT_ERR_MAX];
    if (rt->conn->subscribe_device(rt->conn, dev, err, sizeof err) != 0) {
        logmsg("warn",
               "device %s: push delivery unavailable (%s); polling every point",
               dev->name, err);
        clear_subscriptions(dev);
        return;
    }
    size_t pushed = 0;
    for (size_t j = 0; j < dev->npoints; j++)
        if (dev->points[j].subscribed)
            pushed++;
    if (pushed > 0)
        logmsg("info", "device %s: %zu point(s) delivered by subscription",
               dev->name, pushed);
}

static void connect_device(rt_t *rt, tdot_device_t *dev) {
    char err[TDOT_ERR_MAX];
    if (rt->conn->connect_device(rt->conn, dev, err, sizeof err) == 0) {
        dev->backoff_s = 0;
        arm_subscriptions(rt, dev);
        publish_link(rt, dev, TDOT_LINK_CONNECTED);
    } else {
        clear_subscriptions(dev);
        logmsg("warn", "device %s: connect failed: %s", dev->name, err);
        publish_link(rt, dev, TDOT_LINK_DISCONNECTED);
        dev->backoff_s = dev->backoff_s > 0
                             ? (dev->backoff_s * 2 > BACKOFF_MAX_S
                                    ? BACKOFF_MAX_S
                                    : dev->backoff_s * 2)
                             : BACKOFF_INITIAL_S;
        dev->reconnect_at = tdot_mono() + dev->backoff_s;
        logmsg("info", "device %s: retrying in %.0fs", dev->name,
               dev->backoff_s);
    }
}

static void mark_transport_down(rt_t *rt, tdot_device_t *dev) {
    clear_subscriptions(dev);
    rt->conn->disconnect_device(rt->conn, dev);
    publish_link(rt, dev, TDOT_LINK_DISCONNECTED);
    dev->backoff_s = BACKOFF_INITIAL_S;
    dev->reconnect_at = tdot_mono() + dev->backoff_s;
    logmsg("info", "device %s: reconnecting in %.0fs", dev->name,
           dev->backoff_s);
}

/* ---- command handling ----------------------------------------------------
 * Inbound: te/device/<dev>/ot/<protocol>/cmd/<verb>/<id>
 * payload {"status":"init","point":...,"value":...}
 * Result published retained on the same topic.
 */

static int json_to_value(const cJSON *jv, tdot_value_t *out) {
    memset(out, 0, sizeof *out);
    if (cJSON_IsBool(jv)) {
        out->kind = TDOT_VAL_BOOL;
        out->b = cJSON_IsTrue(jv);
    } else if (cJSON_IsNumber(jv)) {
        out->kind = TDOT_VAL_NUM;
        out->num = jv->valuedouble;
    } else if (cJSON_IsString(jv)) {
        out->kind = TDOT_VAL_STR;
        snprintf(out->str, sizeof out->str, "%s", jv->valuestring);
    } else {
        return -1;
    }
    return 0;
}

static bool is_management_verb(const char *verb);
static void handle_management(rt_t *rt, const char *topic, const char *verb,
                              const cJSON *req);
static void publish_status(rt_t *rt, const char *topic, const char *status,
                           const char *reason, const cJSON *req);
static char *augmented_capabilities(const char *json, const tdot_config_t *cfg);

static void publish_retained(rt_t *rt, const char *topic, cJSON *obj) {
    char *payload = cJSON_PrintUnformatted(obj);
    mosquitto_publish(rt->mosq, NULL, topic, (int)strlen(payload), payload, 0,
                      true);
    free(payload);
}

/* Execute one point write; returns 0 on success, else fills `reason`. */
static int do_write(rt_t *rt, tdot_device_t *dev, const char *dev_name,
                    const char *point_id, const cJSON *jvalue, char *reason,
                    size_t reason_len) {
    tdot_point_t *pt = (dev && point_id) ? tdot_device_point(dev, point_id) : NULL;
    tdot_value_t value;
    if (!dev) {
        snprintf(reason, reason_len, "unknown device: %s", dev_name);
    } else if (!pt) {
        snprintf(reason, reason_len, "unknown point: %s",
                 point_id ? point_id : "(missing)");
    } else if (!(pt->access & TDOT_ACCESS_WRITE)) {
        snprintf(reason, reason_len, "point %s is not writable", pt->id);
    } else if (json_to_value(jvalue, &value) != 0) {
        snprintf(reason, reason_len, "missing or invalid value");
    } else if (rt->conn->write_point(rt->conn, dev, pt, &value, reason,
                                     reason_len) == 0) {
        return 0;
    }
    return -1;
}

/* Echo the request's `origin` (contract §6.4) into a transition published for
 * that command.
 *
 * The command topic is retained and holds exactly ONE message, so `executing`
 * and then the result overwrite the request that carried `origin`. A consumer
 * that starts (or restarts) afterwards replays only the terminal state --
 * carrying the correlation data forward is what lets it still tell which
 * parameter set an acknowledged write belongs to (§5.2), instead of guessing
 * the default one and retaining a fragment no definition matches. */
static void add_origin(cJSON *out, const cJSON *req) {
    /* Case-sensitive, like the Rust runtime's `json.get("origin")`: a request
     * carrying "Origin" must be ignored by both, not echoed by one. */
    const cJSON *origin = cJSON_GetObjectItemCaseSensitive(req, "origin");
    if (origin)
        cJSON_AddItemToObject(out, "origin", cJSON_Duplicate(origin, 1));
}

/* `write`: {"status":"init","point":...,"value":...} -> executing -> successful|failed */
static void handle_write(rt_t *rt, const char *topic, const char *dev_name,
                         tdot_device_t *dev, const cJSON *req) {
    const cJSON *jpoint = cJSON_GetObjectItem(req, "point");
    const char *point_id = cJSON_IsString(jpoint) ? jpoint->valuestring : NULL;
    const cJSON *jv = cJSON_GetObjectItem(req, "value");

    cJSON *exec = cJSON_CreateObject();
    cJSON_AddStringToObject(exec, "status", "executing");
    if (point_id)
        cJSON_AddStringToObject(exec, "point", point_id);
    add_origin(exec, req);
    publish_retained(rt, topic, exec);
    cJSON_Delete(exec);

    char reason[TDOT_ERR_MAX] = "";
    bool ok = do_write(rt, dev, dev_name, point_id, jv, reason, sizeof reason) == 0;

    cJSON *res = cJSON_CreateObject();
    cJSON_AddStringToObject(res, "status", ok ? "successful" : "failed");
    if (point_id)
        cJSON_AddStringToObject(res, "point", point_id);
    if (ok) {
        if (jv)
            cJSON_AddItemToObject(res, "value", cJSON_Duplicate(jv, 1));
        logmsg("info", "cmd write %s/%s: ok", dev_name,
               point_id ? point_id : "?");
    } else {
        cJSON_AddStringToObject(res, "reason", reason);
        logmsg("warn", "cmd write %s/%s: %s", dev_name,
               point_id ? point_id : "?", reason);
    }
    add_origin(res, req);
    publish_retained(rt, topic, res);
    cJSON_Delete(res);
}

/* `write-batch` (contract §6.4): {"status":"init","writes":[{point,value},...]}.
 * Writes run sequentially in request order and stop at the first failure; the
 * terminal message lists one result per attempted write. Implemented once
 * here on top of the connector's write_point, like the Rust SDK runtime. */
static void handle_write_batch(rt_t *rt, const char *topic,
                               const char *dev_name, tdot_device_t *dev,
                               const cJSON *req) {
    const cJSON *writes = cJSON_GetObjectItem(req, "writes");
    cJSON *results = cJSON_CreateArray();
    char reason[TDOT_ERR_MAX] = "";
    bool failed = false;

    if (!cJSON_IsArray(writes)) {
        snprintf(reason, sizeof reason,
                 "write-batch request needs a `writes` array");
        failed = true;
    } else if (cJSON_GetArraySize(writes) == 0) {
        snprintf(reason, sizeof reason, "write-batch request has no writes");
        failed = true;
    }

    if (!failed) {
        cJSON *exec = cJSON_CreateObject();
        cJSON_AddStringToObject(exec, "status", "executing");
        add_origin(exec, req);
        cJSON *points = cJSON_AddArrayToObject(exec, "points");
        const cJSON *w;
        cJSON_ArrayForEach(w, writes) {
            const cJSON *jp = cJSON_GetObjectItem(w, "point");
            cJSON_AddItemToArray(points, cJSON_CreateString(
                                             cJSON_IsString(jp) ? jp->valuestring
                                                                : ""));
        }
        publish_retained(rt, topic, exec);
        cJSON_Delete(exec);

        cJSON_ArrayForEach(w, writes) {
            const cJSON *jp = cJSON_GetObjectItem(w, "point");
            const char *point_id = cJSON_IsString(jp) ? jp->valuestring : NULL;
            const cJSON *jv = cJSON_GetObjectItem(w, "value");
            char why[TDOT_ERR_MAX] = "";
            cJSON *r = cJSON_CreateObject();
            cJSON_AddStringToObject(r, "point", point_id ? point_id : "");
            if (do_write(rt, dev, dev_name, point_id, jv, why, sizeof why) == 0) {
                cJSON_AddStringToObject(r, "status", "successful");
                if (jv)
                    cJSON_AddItemToObject(r, "value", cJSON_Duplicate(jv, 1));
                cJSON_AddItemToArray(results, r);
            } else {
                snprintf(reason, sizeof reason, "write to %s failed: %s",
                         point_id ? point_id : "(missing)", why);
                cJSON_AddStringToObject(r, "status", "failed");
                cJSON_AddStringToObject(r, "reason", reason);
                cJSON_AddItemToArray(results, r);
                failed = true;
                break;
            }
        }
    }

    cJSON *res = cJSON_CreateObject();
    cJSON_AddStringToObject(res, "status", failed ? "failed" : "successful");
    if (failed)
        cJSON_AddStringToObject(res, "reason", reason);
    cJSON_AddItemToObject(res, "results", results);
    add_origin(res, req);
    logmsg(failed ? "warn" : "info", "cmd write-batch %s: %s", dev_name,
           failed ? reason : "ok");
    publish_retained(rt, topic, res);
    cJSON_Delete(res);
}

static void on_message(struct mosquitto *mosq, void *ud,
                       const struct mosquitto_message *msg) {
    (void)mosq;
    rt_t *rt = ud;
    if (!msg->payload || msg->payloadlen == 0)
        return;

    /* Parse topic segments. One more than a command topic has, so a longer
     * topic is told apart from a valid one instead of being truncated into it. */
    char topic[256];
    snprintf(topic, sizeof topic, "%s", msg->topic);
    char *seg[10] = {0};
    int nseg = 0;
    for (char *p = strtok(topic, "/"); p && nseg < 10; p = strtok(NULL, "/"))
        seg[nseg++] = p;

    /* Every instance of the protocol on the broker receives every device
     * command, so each message is routed here (mirrors `route_command` in the
     * Rust runtime):
     *   te/device/<dev>/ot/<proto>/cmd/<verb>/<id>        a device THIS config
     *                                                     defines, else ignored
     *   te/device/main/service/<svc>/ot/cmd/<verb>/<id>   this service's own
     *                                                     management commands
     * A command for a device this instance does not own is left unanswered:
     * another instance (or process) may own it, and an "unknown device" failure
     * from here would race that owner's real result. */
    const char *dev_name = NULL, *verb = NULL;
    bool service_cmd = false;
    if (nseg == 8 && !strcmp(seg[0], "te") && !strcmp(seg[1], "device") &&
        !strcmp(seg[3], "ot") && !strcmp(seg[4], rt->cfg->protocol) &&
        !strcmp(seg[5], "cmd")) {
        dev_name = seg[2];
        verb = seg[6];
    } else if (nseg == 9 && !strcmp(seg[0], "te") && !strcmp(seg[1], "device") &&
               !strcmp(seg[2], "main") && !strcmp(seg[3], "service") &&
               !strcmp(seg[4], rt->cfg->service_name) && !strcmp(seg[5], "ot") &&
               !strcmp(seg[6], "cmd")) {
        verb = seg[7];
        service_cmd = true;
    } else {
        return;
    }
    tdot_device_t *dev = dev_name ? tdot_config_device(rt->cfg, dev_name) : NULL;
    if (!service_cmd && !dev)
        return;

    cJSON *req = cJSON_ParseWithLength(msg->payload, (size_t)msg->payloadlen);
    if (!req)
        return;
    const cJSON *status = cJSON_GetObjectItem(req, "status");
    if (!cJSON_IsString(status) || strcmp(status->valuestring, "init") != 0) {
        cJSON_Delete(req); /* our own result echo, or already-processed */
        return;
    }

    if (service_cmd || is_management_verb(verb)) {
        if (service_cmd && is_management_verb(verb)) {
            handle_management(rt, msg->topic, verb, req);
        } else {
            /* A verb on the wrong kind of topic. Refused rather than ignored:
             * the topic already names this instance as the only addressee. */
            char reason[TDOT_ERR_MAX];
            if (service_cmd)
                snprintf(reason, sizeof reason,
                         "'%s' is not a service command: only set-config, "
                         "define-device and remove-device are; device commands "
                         "go to te/device/<device>/ot/%s/cmd/%s/<id>",
                         verb, rt->cfg->protocol, verb);
            else
                snprintf(reason, sizeof reason,
                         "management verb '%s' is addressed to the connector "
                         "service: te/device/main/service/%s/ot/cmd/%s/<id>",
                         verb, rt->cfg->service_name, verb);
            publish_status(rt, msg->topic, "failed", reason, req);
            logmsg("warn", "cmd %s: %s", verb, reason);
        }
        cJSON_Delete(req);
        return;
    }

    if (strcmp(verb, "write") == 0 || strcmp(verb, "write-coil") == 0) {
        /* write-coil is c8y_SetCoil's alias for write (see the Rust module) */
        handle_write(rt, msg->topic, dev_name, dev, req);
    } else if (strcmp(verb, "write-batch") == 0) {
        handle_write_batch(rt, msg->topic, dev_name, dev, req);
    } else if (is_management_verb(verb)) {
        handle_management(rt, msg->topic, verb, req);
    } else {
        cJSON *res = cJSON_CreateObject();
        cJSON_AddStringToObject(res, "status", "failed");
        char reason[TDOT_ERR_MAX];
        snprintf(reason, sizeof reason, "unsupported verb: %s", verb);
        cJSON_AddStringToObject(res, "reason", reason);
        /* The origin echo applies to this refusal too: the requester's
         * correlation data must come back even when the verb was not
         * recognised, so a consumer can still tell which request failed. */
        add_origin(res, req);
        logmsg("warn", "cmd %s %s: unsupported verb", verb, dev_name);
        publish_retained(rt, msg->topic, res);
        cJSON_Delete(res);
    }
    cJSON_Delete(req);
}

/* ---- capability augmentation ---------------------------------------------
 * Like the Rust SDK runtime, the verbs implemented here (management + batch)
 * are added to every module's descriptor at publish time. */
static void add_unique(cJSON *arr, const char *item) {
    const cJSON *x;
    cJSON_ArrayForEach(x, arr) {
        if (cJSON_IsString(x) && strcmp(x->valuestring, item) == 0)
            return;
    }
    cJSON_AddItemToArray(arr, cJSON_CreateString(item));
}

/* `point_labels` of the capability descriptor (contract §7): every configured
 * point that declares a `name` or a `description`, so a consumer can show
 * something friendlier than the point id.
 *
 * Points with neither are omitted -- their id IS their label -- so a
 * configuration using none of this adds nothing to the descriptor. Published
 * once, retained, rather than echoed in every sample: the labels are static and
 * a sample is a time series. Mirrors descriptor.rs `point_labels`. */
static void add_point_labels(cJSON *caps, const tdot_config_t *cfg) {
    cJSON *labels = NULL;
    for (size_t i = 0; i < cfg->ndevices; i++) {
        const tdot_device_t *dev = &cfg->devices[i];
        for (size_t j = 0; j < dev->npoints; j++) {
            const tdot_point_t *pt = &dev->points[j];
            if (!pt->name && !pt->description)
                continue;
            if (!labels)
                labels = cJSON_AddArrayToObject(caps, "point_labels");
            cJSON *entry = cJSON_CreateObject();
            cJSON_AddStringToObject(entry, "device", dev->name);
            cJSON_AddStringToObject(entry, "point", pt->id);
            if (pt->name)
                cJSON_AddStringToObject(entry, "name", pt->name);
            if (pt->description)
                cJSON_AddStringToObject(entry, "description", pt->description);
            cJSON_AddItemToArray(labels, entry);
        }
    }
}

/* `parameter_keys` of the capability descriptor (contract §7): every configured
 * point that names its own key inside its parameter sets (`meta.parameter.key`),
 * with the `set` / `group` that name those sets, exactly as configured.
 *
 * A consumer (the ot-parameter-state flow) learns from it which point a key
 * belongs to before the point samples -- right after a restart (samples are not
 * retained), and at all for a write-only point, which never samples. Points
 * without a key are omitted: their key is their id. Mirrors descriptor.rs
 * `parameter_keys`. */
static void add_parameter_keys(cJSON *caps, const tdot_config_t *cfg) {
    cJSON *keys = NULL;
    for (size_t i = 0; i < cfg->ndevices; i++) {
        const tdot_device_t *dev = &cfg->devices[i];
        for (size_t j = 0; j < dev->npoints; j++) {
            const tdot_point_t *pt = &dev->points[j];
            cJSON *meta = pt->meta_json ? cJSON_Parse(pt->meta_json) : NULL;
            const cJSON *param =
                meta ? cJSON_GetObjectItemCaseSensitive(meta, "parameter") : NULL;
            const cJSON *key = cJSON_IsObject(param)
                                   ? cJSON_GetObjectItemCaseSensitive(param, "key")
                                   : NULL;
            if (cJSON_IsString(key)) {
                if (!keys)
                    keys = cJSON_AddArrayToObject(caps, "parameter_keys");
                cJSON *entry = cJSON_CreateObject();
                cJSON_AddStringToObject(entry, "device", dev->name);
                cJSON_AddStringToObject(entry, "point", pt->id);
                cJSON_AddStringToObject(entry, "key", key->valuestring);
                static const char *naming[] = {"set", "group"};
                for (size_t k = 0; k < sizeof naming / sizeof *naming; k++) {
                    const cJSON *v = cJSON_GetObjectItemCaseSensitive(param, naming[k]);
                    if (v)
                        cJSON_AddItemToObject(entry, naming[k], cJSON_Duplicate(v, 1));
                }
                cJSON_AddItemToArray(keys, entry);
            }
            cJSON_Delete(meta);
        }
    }
}

static char *augmented_capabilities(const char *json, const tdot_config_t *cfg) {
    cJSON *caps = cJSON_Parse(json);
    if (!caps)
        return NULL;
    cJSON *verbs = cJSON_GetObjectItem(caps, "command_verbs");
    if (!cJSON_IsArray(verbs))
        verbs = cJSON_AddArrayToObject(caps, "command_verbs");
    bool has_write = false;
    const cJSON *v;
    cJSON_ArrayForEach(v, verbs) {
        if (cJSON_IsString(v) && strcmp(v->valuestring, "write") == 0)
            has_write = true;
    }
    if (has_write)
        add_unique(verbs, "write-batch");
    add_unique(verbs, "set-config");
    add_unique(verbs, "define-device");
    add_unique(verbs, "remove-device");
    cJSON *features = cJSON_GetObjectItem(caps, "features");
    if (!cJSON_IsArray(features))
        features = cJSON_AddArrayToObject(caps, "features");
    add_unique(features, "management");
    add_point_labels(caps, cfg);
    char *out = cJSON_PrintUnformatted(caps);
    cJSON_Delete(caps);
    return out;
}

/* Publish the retained capability descriptor (contract §7).
 *
 * Called at startup AND after a management command: its `point_labels` come
 * from the CONFIGURATION, unlike everything else in it, and set-config /
 * define-device / remove-device change the configuration. Left unpublished,
 * the retained message would keep describing the configuration as it was at
 * startup -- labels for points that are gone, none for a device just defined. */
static void publish_capabilities(rt_t *rt) {
    if (!rt->mosq || !rt->conn->capabilities_json)
        return;
    char cap_topic[256];
    snprintf(cap_topic, sizeof cap_topic,
             "te/device/main/service/%s/ot/capabilities", rt->cfg->service_name);
    char *caps = augmented_capabilities(rt->conn->capabilities_json, rt->cfg);
    publish(rt, cap_topic, caps ? caps : rt->conn->capabilities_json, true);
    free(caps);
}

/* ---- MQTT session ----------------------------------------------------------
 * The client uses a clean session, so a broker that drops the connection
 * forgets the command subscriptions, and one restarted without persistence
 * forgets every retained message too. Both have to be restored on reconnect:
 * without that, a connector that lost the broker kept polling its devices but
 * never received another command (a cloud parameter update stayed pending for
 * good) and its service health stayed "down". */

static void subscribe_commands(rt_t *rt) {
    /* Device commands for the whole protocol (on_message keeps the ones for
     * devices this config defines), and management commands for this service
     * (contract §6.3, §6.5). */
    char topic[256];
    snprintf(topic, sizeof topic, "te/device/+/ot/%s/cmd/+/+",
             rt->cfg->protocol);
    mosquitto_subscribe(rt->mosq, NULL, topic, 0);
    snprintf(topic, sizeof topic, "te/device/main/service/%s/ot/cmd/+/+",
             rt->cfg->service_name);
    mosquitto_subscribe(rt->mosq, NULL, topic, 0);
}

static void on_connect(struct mosquitto *mosq, void *ud, int rc) {
    (void)mosq;
    rt_t *rt = ud;
    if (rc != 0)
        return;
    rt->mqtt_up = true;
    /* The first session is set up by run_connector right after connecting,
     * so commands do not wait for every device's initial connect. */
    if (!rt->mqtt_resume)
        return;
    rt->mqtt_resume = false;
    logmsg("info", "reconnected to MQTT broker %s:%d", rt->cfg->mqtt_host,
           rt->cfg->mqtt_port);
    subscribe_commands(rt);
    publish_health(rt, "up");
    publish_capabilities(rt);
    for (size_t i = 0; i < rt->cfg->ndevices; i++)
        if (rt->cfg->devices[i].link != TDOT_LINK_UNKNOWN)
            publish_link_status(rt, &rt->cfg->devices[i]);
}

/* Drive the MQTT client for one tick, reconnecting while the broker is gone. */
static void mqtt_service(rt_t *rt) {
    int rc = mosquitto_loop(rt->mosq, TICK_MS, 1);
    if (rc == MOSQ_ERR_SUCCESS)
        return;
    if (rt->mqtt_up) {
        rt->mqtt_up = false;
        logmsg("warn", "lost connection to MQTT broker %s:%d (%s); reconnecting",
               rt->cfg->mqtt_host, rt->cfg->mqtt_port, mosquitto_strerror(rc));
    }
    double now = tdot_mono();
    if (now >= rt->mqtt_retry_at) {
        rt->mqtt_retry_at = now + MQTT_RETRY_S;
        rt->mqtt_resume = true;
        /* Asynchronous, so an unreachable broker address cannot hold the poll
         * loop for the OS TCP timeout: the following loops finish the
         * handshake and on_connect restores the session. */
        mosquitto_reconnect_async(rt->mosq);
    }
    /* Without a connection mosquitto_loop returns at once instead of waiting
     * out its timeout, so sleep the tick here: otherwise the poll loop spins a
     * core for as long as the broker is away. */
    struct timespec ts = {.tv_sec = 0, .tv_nsec = TICK_MS * 1000000L};
    nanosleep(&ts, NULL);
}

/* ---- TOML emitter (cJSON document -> TOML text) ---------------------------
 * The management verbs patch the configuration as JSON and write it back as
 * TOML. Layout: top-level objects become [sections] (nested objects become
 * [dotted.sections]); arrays of objects become [[array]] entries whose nested
 * objects are inline tables and whose nested arrays of objects become
 * [[array.sub]] entries — i.e. the layout of the shipped config files
 * ([connector], [connection.serial], [[device]] with inline protocol_address,
 * [[device.point]] with inline address/transform/meta). Comments are not
 * preserved (tomlc99 is read-only), unlike the Rust runtime's toml_edit.
 */
typedef struct {
    char *buf;
    size_t len, cap;
} sb_t;

static void sb_put(sb_t *sb, const char *s) {
    size_t n = strlen(s);
    if (sb->len + n + 1 > sb->cap) {
        size_t cap = sb->cap ? sb->cap * 2 : 1024;
        while (cap < sb->len + n + 1)
            cap *= 2;
        sb->buf = realloc(sb->buf, cap);
        sb->cap = cap;
    }
    memcpy(sb->buf + sb->len, s, n + 1);
    sb->len += n;
}

static void sb_putf(sb_t *sb, const char *fmt, ...) {
    char tmp[512];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(tmp, sizeof tmp, fmt, ap);
    va_end(ap);
    sb_put(sb, tmp);
}

static bool bare_key(const char *k) {
    if (!*k)
        return false;
    for (const char *p = k; *p; p++) {
        if (!((*p >= 'A' && *p <= 'Z') || (*p >= 'a' && *p <= 'z') ||
              (*p >= '0' && *p <= '9') || *p == '_' || *p == '-'))
            return false;
    }
    return true;
}

static void emit_string(sb_t *sb, const char *str) {
    sb_put(sb, "\"");
    for (const char *p = str; *p; p++) {
        switch (*p) {
        case '"': sb_put(sb, "\\\""); break;
        case '\\': sb_put(sb, "\\\\"); break;
        case '\n': sb_put(sb, "\\n"); break;
        case '\r': sb_put(sb, "\\r"); break;
        case '\t': sb_put(sb, "\\t"); break;
        default:
            if ((unsigned char)*p < 0x20)
                sb_putf(sb, "\\u%04x", (unsigned)(unsigned char)*p);
            else {
                char c[2] = {*p, 0};
                sb_put(sb, c);
            }
        }
    }
    sb_put(sb, "\"");
}

static void emit_key(sb_t *sb, const char *k) {
    if (bare_key(k))
        sb_put(sb, k);
    else
        emit_string(sb, k);
}

static bool is_object_array(const cJSON *v) {
    if (!cJSON_IsArray(v) || cJSON_GetArraySize(v) == 0)
        return false;
    const cJSON *x;
    cJSON_ArrayForEach(x, v) {
        if (!cJSON_IsObject(x))
            return false;
    }
    return true;
}

static void emit_inline(sb_t *sb, const cJSON *v) {
    if (cJSON_IsString(v)) {
        emit_string(sb, v->valuestring);
    } else if (cJSON_IsBool(v)) {
        sb_put(sb, cJSON_IsTrue(v) ? "true" : "false");
    } else if (cJSON_IsNumber(v)) {
        double d = v->valuedouble;
        if (d == (double)(long long)d && d < 9.2e18 && d > -9.2e18)
            sb_putf(sb, "%lld", (long long)d);
        else
            sb_putf(sb, "%.17g", d);
    } else if (cJSON_IsArray(v)) {
        sb_put(sb, "[");
        const cJSON *x;
        bool first = true;
        cJSON_ArrayForEach(x, v) {
            if (!first)
                sb_put(sb, ", ");
            first = false;
            emit_inline(sb, x);
        }
        sb_put(sb, "]");
    } else if (cJSON_IsObject(v)) {
        sb_put(sb, "{ ");
        const cJSON *x;
        bool first = true;
        cJSON_ArrayForEach(x, v) {
            if (cJSON_IsNull(x))
                continue;
            if (!first)
                sb_put(sb, ", ");
            first = false;
            emit_key(sb, x->string);
            sb_put(sb, " = ");
            emit_inline(sb, x);
        }
        sb_put(sb, " }");
    } else {
        sb_put(sb, "\"\""); /* null: should have been skipped */
    }
}

static void emit_table(sb_t *sb, const cJSON *obj, const char *path,
                       bool in_array_item);

/* Scalars and inline values first, then the deferred sub-tables. */
static void emit_table_body(sb_t *sb, const cJSON *obj, const char *path,
                            bool in_array_item) {
    const cJSON *x;
    cJSON_ArrayForEach(x, obj) {
        if (cJSON_IsNull(x))
            continue;
        bool deferred = is_object_array(x) || (cJSON_IsObject(x) && !in_array_item);
        if (deferred)
            continue;
        emit_key(sb, x->string);
        sb_put(sb, " = ");
        emit_inline(sb, x);
        sb_put(sb, "\n");
    }
    cJSON_ArrayForEach(x, obj) {
        if (cJSON_IsNull(x))
            continue;
        char sub[512];
        if (*path)
            snprintf(sub, sizeof sub, "%s.%s", path, x->string);
        else
            snprintf(sub, sizeof sub, "%s", x->string);
        if (is_object_array(x)) {
            const cJSON *item;
            cJSON_ArrayForEach(item, x) {
                sb_putf(sb, "\n[[%s]]\n", sub);
                emit_table_body(sb, item, sub, true);
            }
        } else if (cJSON_IsObject(x) && !in_array_item) {
            emit_table(sb, x, sub, false);
        }
    }
}

static void emit_table(sb_t *sb, const cJSON *obj, const char *path,
                       bool in_array_item) {
    if (*path)
        sb_putf(sb, "\n[%s]\n", path);
    emit_table_body(sb, obj, path, in_array_item);
}

static char *toml_emit(const cJSON *doc) {
    sb_t sb = {0};
    sb_put(&sb, "# Written by tedge-dot (management command); comments are not preserved.\n");
    emit_table_body(&sb, doc, "", false);
    return sb.buf;
}

/* ---- management verbs (contract §6.3) -------------------------------------
 * set-config / define-device / remove-device patch the configuration document,
 * validate the result (config loader + connector configure), persist it to
 * the config file, and live-reload the connector — the same behaviour as the
 * Rust SDK runtime. */
static bool is_management_verb(const char *verb) {
    return strcmp(verb, "set-config") == 0 ||
           strcmp(verb, "define-device") == 0 ||
           strcmp(verb, "remove-device") == 0;
}

/* Deep-merge `patch` into `target`: objects merge recursively, everything
 * else replaces. */
static void deep_merge(cJSON *target, const cJSON *patch) {
    const cJSON *x;
    cJSON_ArrayForEach(x, patch) {
        cJSON *existing = cJSON_GetObjectItemCaseSensitive(target, x->string);
        if (existing && cJSON_IsObject(existing) && cJSON_IsObject(x)) {
            deep_merge(existing, x);
        } else if (existing) {
            cJSON_ReplaceItemInObjectCaseSensitive(target, x->string,
                                                   cJSON_Duplicate(x, 1));
        } else {
            cJSON_AddItemToObject(target, x->string, cJSON_Duplicate(x, 1));
        }
    }
}

/* True when `doc` already gives `device` this exact points_from reference. */
static bool has_path_reference(const cJSON *doc, const char *device,
                               const char *reference) {
    const cJSON *devices = cJSON_GetObjectItem(doc, "device");
    if (!cJSON_IsArray(devices))
        return false;
    const cJSON *dev;
    cJSON_ArrayForEach(dev, devices) {
        const cJSON *name = cJSON_GetObjectItem(dev, "name");
        if (!cJSON_IsString(name) || strcmp(name->valuestring, device) != 0)
            continue;
        const cJSON *refs = cJSON_GetObjectItem(dev, "points_from");
        if (!cJSON_IsArray(refs))
            return false;
        const cJSON *ref;
        cJSON_ArrayForEach(ref, refs)
            if (cJSON_IsString(ref) && strcmp(ref->valuestring, reference) == 0)
                return true;
        return false;
    }
    return false;
}

/* A management command may name a point library, never a path (contract §3.4).
 * A config file is edited by root or tedge; a command is a different trust
 * boundary -- anything that can publish on the broker could otherwise name an
 * arbitrary path and read the loader's verdict (does it exist, and through a
 * parse error a line of its contents) out of the retained command result.
 * Names are all a discovery mechanism needs, so only names are accepted.
 *
 * Only what the command CHANGED is judged, against `before`: the path
 * references a device already had stay legal, so an unrelated set-config (or a
 * remove-device) on a configuration that uses the path form still works.
 * Checked before the candidate is written or loaded. */
static int reject_path_references(const cJSON *before, const cJSON *after,
                                  char *reason, size_t rlen) {
    const cJSON *devices = cJSON_GetObjectItem(after, "device");
    if (!cJSON_IsArray(devices))
        return 0;
    const cJSON *dev;
    cJSON_ArrayForEach(dev, devices) {
        const cJSON *name = cJSON_GetObjectItem(dev, "name");
        const cJSON *refs = cJSON_GetObjectItem(dev, "points_from");
        if (!cJSON_IsArray(refs))
            continue;
        const char *device = cJSON_IsString(name) ? name->valuestring : "<unnamed>";
        const cJSON *ref;
        cJSON_ArrayForEach(ref, refs) {
            if (!cJSON_IsString(ref) || !tdot_is_path_reference(ref->valuestring))
                continue;
            if (has_path_reference(before, device, ref->valuestring))
                continue; /* already in the configuration; not this command's doing */
            snprintf(reason, rlen,
                     "device '%s': points_from '%s' is a path; a management command may only "
                     "name a point library, not a path",
                     device, ref->valuestring);
            return -1;
        }
    }
    return 0;
}

static cJSON *find_device(cJSON *devices, const char *name, int *index) {
    int i = 0;
    cJSON *d;
    cJSON_ArrayForEach(d, devices) {
        const cJSON *n = cJSON_GetObjectItem(d, "name");
        if (cJSON_IsString(n) && strcmp(n->valuestring, name) == 0) {
            if (index)
                *index = i;
            return d;
        }
        i++;
    }
    return NULL;
}

/* Apply one management verb to the JSON form of the config document.
 * Returns 0, or -1 with `reason` filled. */
static int apply_management(cJSON *doc, const char *verb, const cJSON *req,
                            char *reason, size_t rlen) {
    cJSON *devices = cJSON_GetObjectItem(doc, "device");
    if (!cJSON_IsArray(devices))
        devices = cJSON_AddArrayToObject(doc, "device");

    if (strcmp(verb, "set-config") == 0) {
        const cJSON *target = cJSON_GetObjectItem(req, "target");
        const cJSON *config = cJSON_GetObjectItem(req, "config");
        if (!cJSON_IsString(target) || !cJSON_IsObject(config)) {
            snprintf(reason, rlen, "set-config needs `target` (string) and `config` (object)");
            return -1;
        }
        const char *t = target->valuestring;
        /* The service name addresses this connector's management commands
         * (§6.3) and the protocol selects its module: a running instance cannot
         * take either from a command. Mirrors apply_set_config (Rust). */
        if (strcmp(t, "connector") == 0) {
            const char *key =
                cJSON_GetObjectItemCaseSensitive(config, "service_name") ? "service_name"
                : cJSON_GetObjectItemCaseSensitive(config, "protocol")   ? "protocol"
                                                                          : NULL;
            if (key) {
                snprintf(reason, rlen,
                         "set-config cannot change connector.%s: edit the "
                         "configuration file and restart the connector",
                         key);
                return -1;
            }
        }
        cJSON *section = NULL;
        if (strcmp(t, "connector") == 0 || strcmp(t, "mqtt") == 0 ||
            strcmp(t, "connection") == 0) {
            section = cJSON_GetObjectItem(doc, t);
            if (!cJSON_IsObject(section))
                section = cJSON_AddObjectToObject(doc, t);
        } else if (strncmp(t, "device:", 7) == 0) {
            section = find_device(devices, t + 7, NULL);
            if (!section) {
                snprintf(reason, rlen, "unknown device '%s'", t + 7);
                return -1;
            }
        } else {
            snprintf(reason, rlen,
                     "unknown target '%s' (expected connector|mqtt|connection|device:<name>)", t);
            return -1;
        }
        deep_merge(section, config);
        return 0;
    }
    if (strcmp(verb, "define-device") == 0) {
        const cJSON *device = cJSON_GetObjectItem(req, "device");
        const cJSON *name = cJSON_IsObject(device) ? cJSON_GetObjectItem(device, "name") : NULL;
        if (!cJSON_IsString(name)) {
            snprintf(reason, rlen, "define-device needs a `device` object with a `name`");
            return -1;
        }
        int idx = -1;
        if (find_device(devices, name->valuestring, &idx))
            cJSON_ReplaceItemInArray(devices, idx, cJSON_Duplicate(device, 1));
        else
            cJSON_AddItemToArray(devices, cJSON_Duplicate(device, 1));
        return 0;
    }
    /* remove-device */
    const cJSON *name = cJSON_GetObjectItem(req, "device");
    if (!cJSON_IsString(name)) {
        snprintf(reason, rlen, "remove-device needs `device` (the device name)");
        return -1;
    }
    int idx = -1;
    if (!find_device(devices, name->valuestring, &idx)) {
        snprintf(reason, rlen, "unknown device '%s'", name->valuestring);
        return -1;
    }
    cJSON_DeleteItemFromArray(devices, idx);
    return 0;
}

/* One transition of a command handled by the runtime itself (management verbs,
 * refusals). `req` is the request, for its `origin` echo (§6.4): a requester
 * routing the result back — a bridge flow completing the command on the entity
 * it was issued for — replays only this retained message. */
static void publish_status(rt_t *rt, const char *topic, const char *status,
                           const char *reason, const cJSON *req) {
    cJSON *res = cJSON_CreateObject();
    cJSON_AddStringToObject(res, "status", status);
    if (reason)
        cJSON_AddStringToObject(res, "reason", reason);
    add_origin(res, req);
    publish_retained(rt, topic, res);
    cJSON_Delete(res);
}

/* Try `candidate` on the protocol module in place of the running
 * configuration: release the running transports and connector state, then
 * configure. On failure the candidate is freed, the running configuration is
 * configured and connected again, and `reason` says why. Shared by management
 * commands and reloads. */
static int try_configure(rt_t *rt, tdot_config_t *candidate, char *reason,
                         size_t rlen) {
    tdot_config_t *cfg = rt->cfg;
    for (size_t i = 0; i < cfg->ndevices; i++)
        rt->conn->disconnect_device(rt->conn, &cfg->devices[i]);
    tdot_config_release_protos(cfg);
    char err[256];
    if (rt->conn->configure(rt->conn, candidate, err, sizeof err) == 0)
        return 0;
    tdot_config_free(candidate);
    snprintf(reason, rlen, "configure failed: %s", err);
    char err2[256];
    if (rt->conn->configure(rt->conn, cfg, err2, sizeof err2) != 0)
        logmsg("error", "re-configure of the previous config failed: %s", err2);
    for (size_t i = 0; i < cfg->ndevices; i++) {
        cfg->devices[i].link = TDOT_LINK_UNKNOWN;
        connect_device(rt, &cfg->devices[i]);
    }
    return -1;
}

/* Install a candidate the protocol module accepted (try_configure): replace
 * the running configuration in place (the cfg pointer stays valid), republish
 * the capability descriptor -- its point_labels follow the configuration (§7)
 * -- and connect every device with it. */
static void commit_config(rt_t *rt, tdot_config_t *candidate) {
    tdot_config_replace(rt->cfg, candidate);
    publish_capabilities(rt);
    for (size_t i = 0; i < rt->cfg->ndevices; i++)
        connect_device(rt, &rt->cfg->devices[i]);
}

/* Validate the candidate config end to end, persist it, and live-reload:
 * disconnect -> configure(new) -> replace in place -> reconnect. On any
 * failure the running configuration is kept (and re-configured). */
static void handle_management(rt_t *rt, const char *topic, const char *verb,
                              const cJSON *req) {
    publish_status(rt, topic, "executing", NULL, req);
    char reason[TDOT_ERR_MAX] = "";
    tdot_config_t *cfg = rt->cfg;

    if (!cfg->path) {
        publish_status(rt, topic, "failed", "configuration has no file path to persist to", req);
        return;
    }
    char *json = tdot_config_root_json(cfg);
    cJSON *doc = json ? cJSON_Parse(json) : NULL;
    free(json);
    if (!doc) {
        publish_status(rt, topic, "failed", "cannot read the running configuration document", req);
        return;
    }
    cJSON *before = cJSON_Duplicate(doc, 1); /* apply_management mutates `doc` in place */
    int rc = apply_management(doc, verb, req, reason, sizeof reason);
    if (rc == 0)
        rc = reject_path_references(before, doc, reason, sizeof reason);
    cJSON_Delete(before);
    if (rc != 0) {
        cJSON_Delete(doc);
        publish_status(rt, topic, "failed", reason, req);
        logmsg("warn", "cmd %s: %s", verb, reason);
        return;
    }
    char *text = toml_emit(doc);
    cJSON_Delete(doc);

    /* Write the candidate next to the config and validate it with the real
     * loader before it replaces anything. */
    char tmp_path[1024];
    snprintf(tmp_path, sizeof tmp_path, "%s.tmp", cfg->path);
    FILE *f = fopen(tmp_path, "w");
    if (!f || fputs(text, f) == EOF || fclose(f) != 0) {
        free(text);
        snprintf(reason, sizeof reason, "cannot write %s: %s", tmp_path, strerror(errno));
        publish_status(rt, topic, "failed", reason, req);
        return;
    }
    free(text);
    char err[256];
    tdot_config_t *candidate = tdot_config_load(tmp_path, err, sizeof err);
    if (!candidate) {
        unlink(tmp_path);
        snprintf(reason, sizeof reason, "resulting config is invalid: %s", err);
        publish_status(rt, topic, "failed", reason, req);
        logmsg("warn", "cmd %s: %s", verb, reason);
        return;
    }

    if (try_configure(rt, candidate, reason, sizeof reason) != 0) {
        unlink(tmp_path);
        publish_status(rt, topic, "failed", reason, req);
        logmsg("warn", "cmd %s: %s", verb, reason);
        return;
    }
    if (rename(tmp_path, cfg->path) != 0)
        logmsg("warn", "failed to persist config to %s: %s", cfg->path, strerror(errno));
    commit_config(rt, candidate);
    publish_status(rt, topic, "successful", NULL, req);
    logmsg("info", "cmd %s: applied and persisted to %s", verb, cfg->path);
}

/* ---- reload (SIGHUP) ------------------------------------------------------ */

/* What a reload did (mirrors `Reloaded` in the Rust runtime). */
typedef enum {
    RELOAD_UNCHANGED, /* the file is the configuration already running */
    RELOAD_KEPT,      /* the file could not be used; the running one is kept */
    RELOAD_APPLIED,   /* the new configuration was applied in place */
    RELOAD_RESTART,   /* the change needs the connector restarted with it */
} reload_t;

/* A change the running connector cannot adopt in place (mirrors
 * `needs_restart` in the Rust runtime): its MQTT client id, last will and
 * command subscriptions are named after the service and the protocol, the
 * protocol selects the module, the client is connected to one broker, and the
 * stall watchdog takes its limit when the connector starts. */
static bool needs_restart(const tdot_config_t *running,
                          const tdot_config_t *candidate) {
    return strcmp(running->protocol, candidate->protocol) != 0 ||
           strcmp(running->service_name, candidate->service_name) != 0 ||
           strcmp(running->mqtt_host, candidate->mqtt_host) != 0 ||
           running->mqtt_port != candidate->mqtt_port ||
           running->stall_timeout_s != candidate->stall_timeout_s;
}

/* Re-read the connector's config file and apply what changed, keeping the
 * running configuration when the file cannot be used. An unchanged file --
 * same documents, point libraries included -- is left alone entirely, so a
 * reload for another connector's file does not reconnect this one's devices. */
static reload_t reload_from_file(rt_t *rt, bool can_restart) {
    tdot_config_t *cfg = rt->cfg;
    if (!cfg->path)
        return RELOAD_UNCHANGED;
    char err[256];
    tdot_config_t *candidate = tdot_config_load(cfg->path, err, sizeof err);
    if (!candidate) {
        logmsg("error", "reload: %s; keeping the running configuration", err);
        return RELOAD_KEPT;
    }
    if (needs_restart(cfg, candidate)) {
        tdot_config_free(candidate);
        if (!can_restart) {
            logmsg("error",
                   "reload: %s changes the service name, protocol, broker or "
                   "stall timeout, which needs a restart; keeping the running "
                   "configuration",
                   cfg->path);
            return RELOAD_KEPT;
        }
        logmsg("info",
               "reload: %s changes the service name, protocol, broker or stall "
               "timeout; restarting the connector",
               cfg->path);
        return RELOAD_RESTART;
    }
    char *before = tdot_config_fingerprint(cfg);
    char *after = tdot_config_fingerprint(candidate);
    bool unchanged = before && after && strcmp(before, after) == 0;
    free(before);
    free(after);
    if (unchanged) {
        logmsg("info", "reload: %s is unchanged", cfg->path);
        tdot_config_free(candidate);
        return RELOAD_UNCHANGED;
    }
    char reason[TDOT_ERR_MAX];
    if (try_configure(rt, candidate, reason, sizeof reason) != 0) {
        logmsg("error", "reload: %s: %s; keeping the running configuration",
               cfg->path, reason);
        return RELOAD_KEPT;
    }
    commit_config(rt, candidate);
    logmsg("info", "reload: applied %s", cfg->path);
    return RELOAD_APPLIED;
}

/* ---- main loop ------------------------------------------------------------ */

/* Context handed to the module's drain_subscriptions() so pushed samples reach
 * the same publishing path as polled ones. */
typedef struct {
    rt_t *rt;
} sink_ctx_t;

static void push_sink(void *ctx, tdot_device_t *dev, tdot_point_t *pt,
                      const tdot_sample_t *s) {
    sink_ctx_t *c = ctx;
    /* Same raw-mode rule as the poll path: a raw point publishes the wire bytes
     * only, never a decoded value. tdot_sample_t is a flat POD, so the copy is
     * cheap and leaves the module's own sample untouched. */
    tdot_sample_t local = *s;
    if (pt->mode == TDOT_MODE_RAW)
        local.value.kind = TDOT_VAL_NONE;
    emit_sample(c->rt, dev, pt, &local);
}

/* How run_connector ended. */
#define RUN_STOPPED 0  /* asked to stop: a signal, the duration, the supervisor */
#define RUN_FAILED -1  /* could not start: configure or the broker failed */
#define RUN_RESTART 1  /* a reload needs the connector restarted from its file */

/* The supervisor's controls over one running connector. */
typedef struct {
    _Atomic int stop; /* stop this connector: its config file is gone */
    bool can_restart; /* whoever runs it restarts it on RUN_RESTART */
    /* The reload generation current before its config file was read: a SIGHUP
     * that arrives while the connector starts (the broker and device connects
     * can take a while) is newer, so it is still acted on. */
    unsigned start_gen;
} run_ctl_t;

static bool stop_requested(const run_ctl_t *ctl) {
    return g_stop || (ctl && atomic_load(&ctl->stop));
}

/* Run one connector to completion. Assumes the mosquitto library is already
 * initialised and the signal handlers are installed by the caller, so it is
 * safe to call from one of several worker threads (each owns its own
 * connector, config and mosquitto client). A SIGHUP makes it re-read its
 * config file and apply what changed (reload_from_file). */
static int run_connector(tdot_connector_t *conn, tdot_config_t *cfg,
                         const tdot_run_opts_t *opts, progress_t *progress,
                         run_ctl_t *ctl) {
    rt_t rt = {.conn = conn,
               .cfg = cfg,
               .output = opts->output,
               .progress = progress};
    char err[256];

    if (conn->configure(conn, cfg, err, sizeof err) != 0) {
        logmsg("error", "configure failed: %s", err);
        return -1;
    }

    /* Say where a device's points came from when it references point libraries
     * (contract §3.4): with the list in another file, "which points did I
     * actually get" is the first question a misconfiguration raises. */
    for (size_t i = 0; i < cfg->ndevices; i++) {
        const tdot_device_t *dev = &cfg->devices[i];
        if (!dev->npoints_from)
            continue;
        char refs[512] = "";
        size_t used = 0;
        for (size_t j = 0; j < dev->npoints_from && used < sizeof refs - 1; j++)
            used += (size_t)snprintf(refs + used, sizeof refs - used, "%s%s",
                                     used ? ", " : "", dev->points_from[j]);
        logmsg("info", "device %s: %zu point(s) resolved from point librar%s %s",
               dev->name, dev->npoints, dev->npoints_from == 1 ? "y" : "ies", refs);
    }

    if (rt.output == TDOT_OUTPUT_MQTT) {
        char client_id[128];
        snprintf(client_id, sizeof client_id, "%s-%s", cfg->service_name,
                 cfg->protocol);
        rt.mosq = mosquitto_new(client_id, true, &rt);
        mosquitto_message_callback_set(rt.mosq, on_message);
        mosquitto_connect_callback_set(rt.mosq, on_connect);
        /* last will: health "down" */
        char will_topic[256];
        snprintf(will_topic, sizeof will_topic,
                 "te/device/main/service/%s/status/health", cfg->service_name);
        mosquitto_will_set(rt.mosq, will_topic, 17, "{\"status\":\"down\"}",
                           0, true);
        if (mosquitto_connect(rt.mosq, cfg->mqtt_host, cfg->mqtt_port, 60) !=
            MOSQ_ERR_SUCCESS) {
            logmsg("error", "cannot connect to MQTT broker %s:%d",
                   cfg->mqtt_host, cfg->mqtt_port);
            mosquitto_destroy(rt.mosq);
            return -1;
        }
        rt.mqtt_up = true;
        subscribe_commands(&rt);
        logmsg("info", "connected to MQTT broker %s:%d", cfg->mqtt_host,
               cfg->mqtt_port);

        publish_health(&rt, "up");
        publish_capabilities(&rt);
    }

    /* Arm the watchdog before the first protocol call, not at the top of the
     * loop: the initial connect is itself a protocol call that can wedge (a
     * route that blackholes SYN blocks for the OS TCP timeout, far longer than
     * any response timeout), and a heartbeat left at 0 reads as "not running
     * yet" and would never fire. */
    if (rt.progress)
        atomic_store(&rt.progress->beat_ms, (long long)(tdot_mono() * 1000.0));

    /* Initial connect for all devices. */
    for (size_t i = 0; i < cfg->ndevices; i++)
        connect_device(&rt, &cfg->devices[i]);

    double deadline =
        opts->duration_s > 0 ? tdot_mono() + opts->duration_s : 0;
    /* The reload generation this connector has acted on: the one current
     * before its file was read, so a reload requested since is applied now. */
    unsigned seen_gen = ctl ? ctl->start_gen : atomic_load(&g_reload_gen);
    int result = RUN_STOPPED;

    while (!stop_requested(ctl)) {
        double now = tdot_mono();
        if (deadline > 0 && now >= deadline)
            break;

        unsigned gen = atomic_load(&g_reload_gen);
        if (gen != seen_gen) {
            seen_gen = gen;
            if (reload_from_file(&rt, ctl && ctl->can_restart) == RELOAD_RESTART) {
                result = RUN_RESTART;
                break;
            }
            now = tdot_mono(); /* applying a reload reconnects the devices */
        }

        /* Heartbeat for the stall watchdog: stamped at the top of every tick,
         * so it stops advancing exactly when a protocol call below does not
         * return. */
        if (rt.progress)
            atomic_store(&rt.progress->beat_ms, (long long)(now * 1000.0));

        for (size_t i = 0; i < cfg->ndevices && !stop_requested(ctl); i++) {
            tdot_device_t *dev = &cfg->devices[i];
            if (dev->link == TDOT_LINK_DISCONNECTED) {
                if (now < dev->reconnect_at)
                    continue;
                connect_device(&rt, dev);
                if (dev->link != TDOT_LINK_CONNECTED)
                    continue;
            }
            bool transport_down = false;
            size_t bad = 0, polled = 0;
            for (size_t j = 0; j < dev->npoints && !transport_down; j++) {
                tdot_point_t *pt = &dev->points[j];
                /* A subscribed point is delivered by the module's drain below;
                 * polling it too would double-publish. */
                if (pt->subscribed)
                    continue;
                if (!(pt->access & TDOT_ACCESS_READ) || now < pt->next_due)
                    continue;
                tdot_sample_t s;
                tdot_sample_init(&s);
                int rc = conn->read_point(conn, dev, pt, &s);
                if (pt->mode == TDOT_MODE_RAW)
                    s.value.kind = TDOT_VAL_NONE; /* raw: bytes only */
                emit_sample(&rt, dev, pt, &s);
                pt->next_due = now + pt->poll_interval_s;
                polled++;
                if (s.quality == TDOT_Q_BAD)
                    bad++;
                if (rc != 0)
                    transport_down = true;
            }
            if (transport_down) {
                mark_transport_down(&rt, dev);
                continue;
            }
            if (polled > 0) {
                /* whole batch failing degrades the link; any success is
                 * connected */
                publish_link(&rt, dev,
                             bad == polled ? TDOT_LINK_DEGRADED
                                           : TDOT_LINK_CONNECTED);
            }

            /* Hand over whatever the module received by push since the last
             * tick. Only the drain's transport verdict feeds the link state:
             * pushed samples are value changes, so their absence says nothing
             * about the link (a quiet signal is normal), which is why they do
             * not drive the degraded/connected transitions above. */
            if (rt.conn->drain_subscriptions) {
                bool any = false;
                for (size_t j = 0; j < dev->npoints && !any; j++)
                    any = dev->points[j].subscribed;
                if (any) {
                    sink_ctx_t sink = {.rt = &rt};
                    if (rt.conn->drain_subscriptions(rt.conn, dev, push_sink,
                                                     &sink) != 0)
                        mark_transport_down(&rt, dev);
                }
            }
        }

        if (rt.output == TDOT_OUTPUT_MQTT)
            mqtt_service(&rt);
        else {
            struct timespec ts = {.tv_sec = 0, .tv_nsec = TICK_MS * 1000000L};
            nanosleep(&ts, NULL);
        }
    }

    logmsg("info", "shutting down");
    for (size_t i = 0; i < cfg->ndevices; i++)
        conn->disconnect_device(conn, &cfg->devices[i]);
    if (rt.output == TDOT_OUTPUT_MQTT) {
        publish_health(&rt, "down");
        mosquitto_loop(rt.mosq, 100, 1); /* flush */
        mosquitto_disconnect(rt.mosq);
        mosquitto_destroy(rt.mosq);
    }
    return result;
}

static void install_signal_handlers(void) {
    struct sigaction sa = {.sa_handler = on_signal};
    sigaction(SIGINT, &sa, NULL);
    sigaction(SIGTERM, &sa, NULL);
    struct sigaction hup = {.sa_handler = on_hangup};
    sigaction(SIGHUP, &hup, NULL);
}

/* ---- stall watchdog ------------------------------------------------------- */

/* The watchdog's view of the running connectors: the heartbeat slot of each.
 * Connectors come and go on a reload, so the supervisor adds and removes slots
 * while the watchdog thread reads them, under `lock`. */
typedef struct {
    pthread_mutex_t lock;
    progress_t **slots;
    size_t n, cap;
} watchdog_t;

/* Exit code used when the watchdog fires, so an operator reading `systemctl
 * status` can tell a wedged connector from a config error (which exits 1). */
#define TDOT_EXIT_STALLED 70

static void watchdog_add(watchdog_t *wd, progress_t *slot) {
    pthread_mutex_lock(&wd->lock);
    if (wd->n == wd->cap) {
        wd->cap = wd->cap ? wd->cap * 2 : 8;
        wd->slots = realloc(wd->slots, wd->cap * sizeof *wd->slots);
    }
    wd->slots[wd->n++] = slot;
    pthread_mutex_unlock(&wd->lock);
}

static void watchdog_remove(watchdog_t *wd, progress_t *slot) {
    pthread_mutex_lock(&wd->lock);
    for (size_t i = 0; i < wd->n; i++)
        if (wd->slots[i] == slot) {
            wd->slots[i] = wd->slots[--wd->n];
            break;
        }
    pthread_mutex_unlock(&wd->lock);
}

/* Start a thread with SIGINT, SIGTERM and SIGHUP blocked in it, so those always
 * land on the supervising thread: a signal handled on a connector thread would
 * interrupt the protocol library's I/O there (a reload must not turn into a
 * failed read). */
static int start_thread(pthread_t *thread, void *(*fn)(void *), void *arg) {
    sigset_t block, old;
    sigemptyset(&block);
    sigaddset(&block, SIGINT);
    sigaddset(&block, SIGTERM);
    sigaddset(&block, SIGHUP);
    pthread_sigmask(SIG_BLOCK, &block, &old);
    int rc = pthread_create(thread, NULL, fn, arg);
    pthread_sigmask(SIG_SETMASK, &old, NULL);
    return rc;
}

double tdot_runtime_stall_idle(long long beat_ms, long long now_ms,
                               double limit_s) {
    if (limit_s <= 0)
        return -1.0; /* watchdog disabled for this connector */
    if (beat_ms == 0)
        return -1.0; /* loop has not started ticking yet */
    double idle_s = (double)(now_ms - beat_ms) / 1000.0;
    return idle_s >= limit_s ? idle_s : -1.0;
}

double tdot_runtime_watchdog_period(const double *limits, size_t n) {
    /* Check often enough to react within a quarter of the tightest limit, but
     * never busier than twice a second nor lazier than every 10s -- the same
     * shape as the Rust watchdog's period. */
    double tightest = 0;
    for (size_t i = 0; i < n; i++)
        if (limits[i] > 0 && (tightest == 0 || limits[i] < tightest))
            tightest = limits[i];
    if (tightest == 0)
        return 0;
    double period = tightest / 4;
    if (period < 0.5)
        period = 0.5;
    if (period > 10.0)
        period = 10.0;
    return period;
}

static void *watchdog_main(void *arg) {
    watchdog_t *wd = arg;
    double slept = 0;
    while (!atomic_load(&g_stop_threads)) {
        /* Sleep in short slices rather than one long nap: the check only needs
         * to happen every `period`, but shutdown must not wait for it. A single
         * nanosleep(period) would hold the process open for up to 10s after the
         * workers have finished. */
        struct timespec ts = {.tv_sec = 0, .tv_nsec = TICK_MS * 1000000L};
        nanosleep(&ts, NULL);
        slept += TICK_MS / 1000.0;

        pthread_mutex_lock(&wd->lock);
        /* The period follows the connectors running now: a reload starts and
         * stops them. */
        double *limits = calloc(wd->n ? wd->n : 1, sizeof *limits);
        double period = 0;
        if (limits) {
            for (size_t i = 0; i < wd->n; i++)
                limits[i] = wd->slots[i]->limit_s;
            period = tdot_runtime_watchdog_period(limits, wd->n);
            free(limits);
        }
        if (period > 0 && slept >= period) {
            slept = 0;
            long long now_ms = (long long)(tdot_mono() * 1000.0);
            for (size_t i = 0; i < wd->n; i++) {
                progress_t *p = wd->slots[i];
                double idle_s = tdot_runtime_stall_idle(
                    atomic_load(&p->beat_ms), now_ms, p->limit_s);
                if (idle_s < 0)
                    continue;
                logmsg("error",
                       "%s: no progress for %.0fs (connector.stall_timeout "
                       "%.0fs): a protocol call is not returning, restarting "
                       "the process",
                       p->name, idle_s, p->limit_s);
                /* The loop cannot rescue itself -- the hang is inside it -- and
                 * a pthread blocked in a protocol library cannot be safely
                 * cancelled, so the whole process goes down and the service
                 * manager brings it back (Restart=always). Exiting also drops
                 * the MQTT connection, which makes the broker publish the
                 * retained last-will health "down" -- the same observable
                 * outcome as the Rust runtime cancelling the connector. _exit()
                 * rather than exit(): no atexit handler should run while
                 * another thread is wedged. */
                fflush(NULL);
                _exit(TDOT_EXIT_STALLED);
            }
        }
        pthread_mutex_unlock(&wd->lock);
    }
    return NULL;
}

/* Whether the stall watchdog guards the connectors: only the long-running
 * service. A `run --duration` or stdout invocation is a foreground one-shot
 * whose caller is watching it. */
static bool watchdog_wanted(const tdot_run_opts_t *opts) {
    return opts->output == TDOT_OUTPUT_MQTT && opts->duration_s <= 0;
}

int tdot_runtime_run(tdot_connector_t *conn, tdot_config_t *cfg,
                     const tdot_run_opts_t *opts) {
    if (opts->output == TDOT_OUTPUT_MQTT)
        mosquitto_lib_init();
    install_signal_handlers();

    progress_t slot = {.beat_ms = 0,
                       .limit_s = watchdog_wanted(opts) ? cfg->stall_timeout_s : 0,
                       .name = cfg->path ? cfg->path : cfg->protocol};
    watchdog_t wd = {.lock = PTHREAD_MUTEX_INITIALIZER};
    pthread_t wd_thread;
    bool watching = false;
    if (slot.limit_s > 0) {
        watchdog_add(&wd, &slot);
        watching = start_thread(&wd_thread, watchdog_main, &wd) == 0;
    } else {
        logmsg("info", "stall watchdog disabled (connector.stall_timeout = 0)");
    }

    /* The connector runs on this caller's thread, with the connector it was
     * given, so a reload that needs a restart is reported, not applied. */
    run_ctl_t ctl = {.can_restart = false, .start_gen = atomic_load(&g_reload_gen)};
    int rc = run_connector(conn, cfg, opts, &slot, &ctl);

    if (watching) {
        atomic_store(&g_stop_threads, 1); /* wake the watchdog out of its sleep loop */
        pthread_join(wd_thread, NULL);
    }
    free(wd.slots);
    if (opts->output == TDOT_OUTPUT_MQTT)
        mosquitto_lib_cleanup();
    return rc == RUN_FAILED ? -1 : 0;
}

/* ---- multi-connector supervisor ------------------------------------------- */

/* One process runs every connector config found in a directory, each in its
 * own thread (mirrors the Rust runtime's single-service model). The calling
 * thread supervises them: it restarts a connector whose reload needs it, and
 * on a reload stops the connectors of files that are gone, starts one for each
 * new file and tries the configs that could not start again. */

typedef struct {
    char *path;
    const tdot_run_opts_t *opts;
    tdot_connector_t *conn; /* NULL while not running */
    tdot_config_t *cfg;
    /* One watchdog slot per connector, each with its own stall_timeout, so a
     * slow serial bus and a fast TCP one can be bounded differently. */
    progress_t progress;
    run_ctl_t ctl;
    pthread_t thread;
    bool running;     /* a thread was started and has not been joined */
    _Atomic int done; /* set by the thread once run_connector returned */
    int rc;           /* run_connector's result, valid once done */
    double retry_at;  /* when a connector that is not running is tried again; 0: never */
} worker_t;

static void *worker_main(void *arg) {
    worker_t *w = arg;
    w->rc = run_connector(w->conn, w->cfg, w->opts, &w->progress, &w->ctl);
    atomic_store(&w->done, 1);
    return NULL;
}

static worker_t *worker_new(const char *path, const tdot_run_opts_t *opts) {
    worker_t *w = calloc(1, sizeof *w);
    w->path = strdup(path);
    w->opts = opts;
    return w;
}

/* TEDGE_DOT_RESTART_DELAY (whole seconds, default 5): how long a connector
 * that could not start, or failed, waits before it is tried again; a reload
 * tries it at once. Mirrors restart_delay_from_env in the Rust binary. */
static double restart_delay_s(void) {
    const char *v = getenv("TEDGE_DOT_RESTART_DELAY");
    if (v) {
        const char *p = *v == '+' ? v + 1 : v;
        bool digits = *p != '\0';
        for (const char *c = p; *c && digits; c++)
            digits = *c >= '0' && *c <= '9';
        if (digits)
            return strtod(p, NULL);
    }
    return 5;
}

/* Remember that the worker's connector is not running and when to try it
 * again: after the restart delay, or on the next reload. */
static void worker_retry_later(worker_t *w) {
    double delay = restart_delay_s();
    w->retry_at = tdot_mono() + delay;
    logmsg("info", "%s: trying its connector again in %.0fs, or on reload", w->path, delay);
}

/* Load the worker's config and start its connector thread. A config that
 * cannot be used is logged and the worker left stopped, to be tried again
 * (worker_retry_later). */
static bool worker_start(worker_t *w, watchdog_t *wd) {
    /* Before the file is read: see run_ctl_t.start_gen. */
    w->ctl.start_gen = atomic_load(&g_reload_gen);
    char err[256];
    tdot_config_t *cfg = tdot_config_load(w->path, err, sizeof err);
    if (!cfg) {
        logmsg("error", "%s", err);
        worker_retry_later(w);
        return false;
    }
    tdot_connector_t *conn = tdot_connector_factory(cfg->protocol);
    if (!conn) {
        logmsg("error", "%s: unknown protocol '%s'", w->path, cfg->protocol);
        tdot_config_free(cfg);
        worker_retry_later(w);
        return false;
    }
    w->cfg = cfg;
    w->conn = conn;
    w->progress.beat_ms = 0;
    w->progress.limit_s = watchdog_wanted(w->opts) ? cfg->stall_timeout_s : 0;
    w->progress.name = w->path;
    w->ctl.stop = 0;
    w->ctl.can_restart = true;
    w->done = 0;
    if (start_thread(&w->thread, worker_main, w) != 0) {
        logmsg("error", "%s: cannot start a connector thread", w->path);
        conn->destroy(conn);
        tdot_config_free(cfg);
        w->conn = NULL;
        w->cfg = NULL;
        worker_retry_later(w);
        return false;
    }
    w->running = true;
    w->retry_at = 0;
    watchdog_add(wd, &w->progress);
    logmsg("info", "loaded %s (%s)", w->path, cfg->protocol);
    return true;
}

/* Wait for the worker's thread (it has returned, or been asked to stop) and
 * release its connector. */
static void worker_join(worker_t *w, watchdog_t *wd) {
    if (!w->running)
        return;
    pthread_join(w->thread, NULL);
    w->running = false;
    watchdog_remove(wd, &w->progress);
    w->conn->destroy(w->conn);
    tdot_config_free(w->cfg);
    w->conn = NULL;
    w->cfg = NULL;
}

static void worker_free(worker_t *w) {
    free(w->path);
    free(w);
}

typedef struct {
    worker_t **items;
    size_t n, cap;
} workers_t;

static void workers_push(workers_t *ws, worker_t *w) {
    if (ws->n == ws->cap) {
        ws->cap = ws->cap ? ws->cap * 2 : 8;
        ws->items = realloc(ws->items, ws->cap * sizeof *ws->items);
    }
    ws->items[ws->n++] = w;
}

/* Mirrors the Rust supervisor's warnings: instances sharing a service_name
 * take over each other's MQTT session, and two of one protocol defining the
 * same device both own it -- both act on its commands, racing each other's
 * results. Checked over the running connectors, at start and after a reload. */
static void warn_duplicates(const workers_t *ws) {
    for (size_t i = 0; i < ws->n; i++) {
        const worker_t *wi = ws->items[i];
        if (!wi->running)
            continue;
        for (size_t j = i + 1; j < ws->n; j++) {
            const worker_t *wj = ws->items[j];
            if (!wj->running)
                continue;
            tdot_config_t *a = wi->cfg, *b = wj->cfg;
            if (strcmp(a->service_name, b->service_name) == 0)
                logmsg("warn",
                       "configs %s and %s share service_name '%s'; give each "
                       "connector a unique service_name or they will steal "
                       "each other's MQTT session",
                       wi->path, wj->path, a->service_name);
            if (strcmp(a->protocol, b->protocol) != 0)
                continue;
            for (size_t x = 0; x < a->ndevices; x++)
                if (tdot_config_device(b, a->devices[x].name))
                    logmsg("warn",
                           "configs %s and %s both define %s device '%s'; "
                           "define each device in one config only or both "
                           "will answer its commands",
                           wi->path, wj->path, a->protocol, a->devices[x].name);
        }
    }
}

/* A reload (SIGHUP), from the supervisor's side: list the config paths again
 * (when the caller can), stop the connectors whose file is gone, try the
 * configs that could not start again, and start a connector for every new
 * file. The running connectors re-read their own files -- run_connector
 * watches the same generation. */
static void supervise_reload(workers_t *ws, const tdot_run_opts_t *opts,
                             watchdog_t *wd) {
    char **paths = NULL;
    size_t npaths = 0;
    bool listed = false;
    if (opts->discover) {
        if (opts->discover(opts->discover_ctx, &paths, &npaths) == 0)
            listed = true;
        else
            logmsg("error", "reload: cannot list the config paths; the "
                            "running connectors are unchanged");
    }

    if (listed) {
        for (size_t i = 0; i < ws->n;) {
            worker_t *w = ws->items[i];
            bool wanted = false;
            for (size_t j = 0; j < npaths && !wanted; j++)
                wanted = strcmp(paths[j], w->path) == 0;
            if (wanted) {
                i++;
                continue;
            }
            logmsg("info", "%s is gone; stopping its connector", w->path);
            atomic_store(&w->ctl.stop, 1);
            worker_join(w, wd);
            worker_free(w);
            memmove(&ws->items[i], &ws->items[i + 1],
                    (ws->n - i - 1) * sizeof *ws->items);
            ws->n--;
        }
    }

    for (size_t i = 0; i < ws->n; i++)
        if (!ws->items[i]->running)
            worker_start(ws->items[i], wd);

    if (listed) {
        for (size_t j = 0; j < npaths; j++) {
            bool known = false;
            for (size_t i = 0; i < ws->n && !known; i++)
                known = strcmp(ws->items[i]->path, paths[j]) == 0;
            if (!known) {
                logmsg("info", "new config %s; starting its connector", paths[j]);
                worker_t *w = worker_new(paths[j], opts);
                workers_push(ws, w);
                worker_start(w, wd);
            }
        }
        for (size_t j = 0; j < npaths; j++)
            free(paths[j]);
        free(paths);
    }
    warn_duplicates(ws);
}

int tdot_runtime_run_configs(const char *const *paths, size_t npaths,
                             const tdot_run_opts_t *opts) {
    if (npaths == 0) {
        logmsg("error", "no connector configs to run");
        return -1;
    }
    /* First of all, so a SIGHUP while starting up is a reload request rather
     * than the signal's default action, which terminates the process. */
    install_signal_handlers();
    /* The reload generation the supervisor has acted on, taken before any
     * config is read, so a reload requested while the connectors start is
     * still acted on once they have. */
    unsigned seen_gen = atomic_load(&g_reload_gen);
    if (opts->output == TDOT_OUTPUT_MQTT)
        mosquitto_lib_init();

    /* With `discover` this is the service, and it runs until it is stopped or
     * --duration elapses, as the Rust build does: a connector that cannot start
     * or fails -- at start-up, or restarting after a reload -- is tried again
     * after the restart delay and on every reload, rather than ending the
     * process when it was the last one running. */
    bool service = opts->discover != NULL;
    double deadline = opts->duration_s > 0 ? tdot_mono() + opts->duration_s : 0;

    watchdog_t wd = {.lock = PTHREAD_MUTEX_INITIALIZER};
    workers_t ws = {0};
    size_t started = 0;
    for (size_t i = 0; i < npaths; i++) {
        worker_t *w = worker_new(paths[i], opts);
        workers_push(&ws, w);
        if (worker_start(w, &wd))
            started++;
    }
    warn_duplicates(&ws);

    int rc = 0;
    pthread_t wd_thread;
    bool watching = false;
    if (started == 0) {
        logmsg("error", "no valid connector configs");
        rc = -1;
        if (!service)
            goto out;
    }
    if (watchdog_wanted(opts))
        watching = start_thread(&wd_thread, watchdog_main, &wd) == 0;

    while (!g_stop) {
        struct timespec ts = {.tv_sec = 0, .tv_nsec = TICK_MS * 1000000L};
        nanosleep(&ts, NULL);
        double now = tdot_mono();
        if (service && deadline > 0 && now >= deadline)
            break;

        /* Reap the connectors that returned -- restart one whose reload needs
         * it, remember a failure -- and try again the ones whose retry is due. */
        size_t alive = 0;
        for (size_t i = 0; i < ws.n && !g_stop; i++) {
            worker_t *w = ws.items[i];
            if (w->running && atomic_load(&w->done)) {
                int wrc = w->rc;
                worker_join(w, &wd);
                if (wrc == RUN_RESTART) {
                    logmsg("info", "restarting the connector of %s", w->path);
                    if (!worker_start(w, &wd))
                        rc = -1;
                } else if (wrc == RUN_FAILED) {
                    rc = -1;
                    worker_retry_later(w);
                }
            } else if (service && !w->running && w->retry_at > 0 && now >= w->retry_at) {
                worker_start(w, &wd);
            }
            if (w->running)
                alive++;
        }
        /* Without `discover`, every connector having stopped -- the --duration
         * elapsed, or none could keep running -- leaves nothing to do. */
        if (alive == 0 && !service)
            break;

        unsigned gen = atomic_load(&g_reload_gen);
        if (gen != seen_gen) {
            seen_gen = gen;
            supervise_reload(&ws, opts, &wd);
        }
    }

out:
    /* Stop whatever still runs (a signal already stopped every connector;
     * this covers leaving the loop any other way) and wait for it. */
    for (size_t i = 0; i < ws.n; i++)
        atomic_store(&ws.items[i]->ctl.stop, 1);
    for (size_t i = 0; i < ws.n; i++) {
        worker_t *w = ws.items[i];
        if (w->running) {
            pthread_join(w->thread, NULL);
            if (w->rc == RUN_FAILED)
                rc = -1;
            w->running = false;
            watchdog_remove(&wd, &w->progress);
            w->conn->destroy(w->conn);
            tdot_config_free(w->cfg);
        }
        worker_free(w);
    }
    if (watching) {
        atomic_store(&g_stop_threads, 1); /* wake the watchdog out of its sleep loop */
        pthread_join(wd_thread, NULL);
    }
    free(ws.items);
    free(wd.slots);
    if (opts->output == TDOT_OUTPUT_MQTT)
        mosquitto_lib_cleanup();
    /* The service was stopped as asked (a signal, --duration): a connector
     * that failed on the way was retried and logged, and does not make the stop
     * a failure -- `systemctl stop` would otherwise leave the unit "failed".
     * The Rust build exits 0 the same way. */
    if (service)
        return 0;
    return rc;
}
