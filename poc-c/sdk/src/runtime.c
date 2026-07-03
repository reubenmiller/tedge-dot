#include "tedge_dot/runtime.h"

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdarg.h>
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

static volatile sig_atomic_t g_stop = 0;

static void on_signal(int sig) {
    (void)sig;
    g_stop = 1;
}

typedef struct {
    tdot_connector_t *conn;
    tdot_config_t *cfg;
    struct mosquitto *mosq; /* NULL in stdout mode */
    tdot_output_t output;
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

static void publish_link(rt_t *rt, tdot_device_t *dev, tdot_link_t status) {
    if (dev->link == status)
        return;
    dev->link = status;
    const char *name = status == TDOT_LINK_CONNECTED      ? "connected"
                       : status == TDOT_LINK_DEGRADED     ? "degraded"
                                                          : "disconnected";
    logmsg("info", "device %s: link %s", dev->name, name);
    char ts[40];
    tdot_now_rfc3339(ts, sizeof ts);
    char topic[256], payload[128];
    snprintf(topic, sizeof topic, "te/device/%s/ot/%s/status/link", dev->name,
             rt->cfg->protocol);
    snprintf(payload, sizeof payload, "{\"status\":\"%s\",\"since\":\"%s\"}",
             name, ts);
    publish(rt, topic, payload, true);
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

static void connect_device(rt_t *rt, tdot_device_t *dev) {
    char err[TDOT_ERR_MAX];
    if (rt->conn->connect_device(rt->conn, dev, err, sizeof err) == 0) {
        dev->backoff_s = 0;
        publish_link(rt, dev, TDOT_LINK_CONNECTED);
    } else {
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

static void on_message(struct mosquitto *mosq, void *ud,
                       const struct mosquitto_message *msg) {
    (void)mosq;
    rt_t *rt = ud;
    if (!msg->payload || msg->payloadlen == 0)
        return;

    /* Parse topic segments. */
    char topic[256];
    snprintf(topic, sizeof topic, "%s", msg->topic);
    char *seg[8] = {0};
    int nseg = 0;
    for (char *p = strtok(topic, "/"); p && nseg < 8; p = strtok(NULL, "/"))
        seg[nseg++] = p;
    /* te device <dev> ot <proto> cmd <verb> <id> */
    if (nseg != 8 || strcmp(seg[5], "cmd") != 0)
        return;
    const char *dev_name = seg[2], *verb = seg[6];

    cJSON *req = cJSON_ParseWithLength(msg->payload, (size_t)msg->payloadlen);
    if (!req)
        return;
    const cJSON *status = cJSON_GetObjectItem(req, "status");
    if (!cJSON_IsString(status) || strcmp(status->valuestring, "init") != 0) {
        cJSON_Delete(req); /* our own result echo, or already-processed */
        return;
    }

    char reason[TDOT_ERR_MAX] = "";
    tdot_device_t *dev = tdot_config_device(rt->cfg, dev_name);
    const cJSON *jpoint = cJSON_GetObjectItem(req, "point");
    const char *point_id =
        cJSON_IsString(jpoint) ? jpoint->valuestring : NULL;
    tdot_point_t *pt =
        (dev && point_id) ? tdot_device_point(dev, point_id) : NULL;
    tdot_value_t value;
    bool ok = false;

    if (strcmp(verb, "write") != 0)
        snprintf(reason, sizeof reason, "unsupported verb: %s", verb);
    else if (!dev)
        snprintf(reason, sizeof reason, "unknown device: %s", dev_name);
    else if (!pt)
        snprintf(reason, sizeof reason, "unknown point: %s",
                 point_id ? point_id : "(missing)");
    else if (!(pt->access & TDOT_ACCESS_WRITE))
        snprintf(reason, sizeof reason, "point %s is not writable", pt->id);
    else if (json_to_value(cJSON_GetObjectItem(req, "value"), &value) != 0)
        snprintf(reason, sizeof reason, "missing or invalid value");
    else if (rt->conn->write_point(rt->conn, dev, pt, &value, reason,
                                   sizeof reason) == 0)
        ok = true;

    cJSON *res = cJSON_CreateObject();
    cJSON_AddStringToObject(res, "status", ok ? "successful" : "failed");
    if (point_id)
        cJSON_AddStringToObject(res, "point", point_id);
    if (ok) {
        cJSON *jv = cJSON_GetObjectItem(req, "value");
        if (jv)
            cJSON_AddItemToObject(res, "value", cJSON_Duplicate(jv, 1));
        logmsg("info", "cmd write %s/%s: ok", dev_name,
               point_id ? point_id : "?");
    } else {
        cJSON_AddStringToObject(res, "reason", reason);
        logmsg("warn", "cmd write %s/%s: %s", dev_name,
               point_id ? point_id : "?", reason);
    }
    char *payload = cJSON_PrintUnformatted(res);
    mosquitto_publish(rt->mosq, NULL, msg->topic, (int)strlen(payload),
                      payload, 0, true);
    free(payload);
    cJSON_Delete(res);
    cJSON_Delete(req);
}

/* ---- main loop ------------------------------------------------------------ */

/* Run one connector to completion. Assumes the mosquitto library is already
 * initialised and the SIGINT/SIGTERM handlers are installed by the caller, so
 * it is safe to call from one of several worker threads (each owns its own
 * connector, config and mosquitto client). */
static int run_connector(tdot_connector_t *conn, tdot_config_t *cfg,
                         const tdot_run_opts_t *opts) {
    rt_t rt = {.conn = conn, .cfg = cfg, .output = opts->output};
    char err[256];

    if (conn->configure(conn, cfg, err, sizeof err) != 0) {
        logmsg("error", "configure failed: %s", err);
        return -1;
    }

    if (rt.output == TDOT_OUTPUT_MQTT) {
        char client_id[128];
        snprintf(client_id, sizeof client_id, "%s#%d", cfg->service_name,
                 (int)getpid());
        rt.mosq = mosquitto_new(client_id, true, &rt);
        mosquitto_message_callback_set(rt.mosq, on_message);
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
        char cmd_topic[256];
        snprintf(cmd_topic, sizeof cmd_topic, "te/device/+/ot/%s/cmd/+/+",
                 cfg->protocol);
        mosquitto_subscribe(rt.mosq, NULL, cmd_topic, 0);
        logmsg("info", "connected to MQTT broker %s:%d", cfg->mqtt_host,
               cfg->mqtt_port);

        publish_health(&rt, "up");
        if (conn->capabilities_json) {
            char cap_topic[256];
            snprintf(cap_topic, sizeof cap_topic,
                     "te/device/main/service/%s/ot/capabilities",
                     cfg->service_name);
            publish(&rt, cap_topic, conn->capabilities_json, true);
        }
    }

    /* Initial connect for all devices. */
    for (size_t i = 0; i < cfg->ndevices; i++)
        connect_device(&rt, &cfg->devices[i]);

    double deadline =
        opts->duration_s > 0 ? tdot_mono() + opts->duration_s : 0;

    while (!g_stop) {
        double now = tdot_mono();
        if (deadline > 0 && now >= deadline)
            break;

        for (size_t i = 0; i < cfg->ndevices && !g_stop; i++) {
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
                if (!(pt->access & TDOT_ACCESS_READ) || now < pt->next_due)
                    continue;
                tdot_sample_t s;
                tdot_sample_init(&s);
                int rc = conn->read_point(conn, dev, pt, &s);
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
            } else if (polled > 0) {
                /* whole batch failing degrades the link; any success is
                 * connected */
                publish_link(&rt, dev,
                             bad == polled ? TDOT_LINK_DEGRADED
                                           : TDOT_LINK_CONNECTED);
            }
        }

        if (rt.output == TDOT_OUTPUT_MQTT)
            mosquitto_loop(rt.mosq, TICK_MS, 1);
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
    return 0;
}

static void install_signal_handlers(void) {
    struct sigaction sa = {.sa_handler = on_signal};
    sigaction(SIGINT, &sa, NULL);
    sigaction(SIGTERM, &sa, NULL);
}

int tdot_runtime_run(tdot_connector_t *conn, tdot_config_t *cfg,
                     const tdot_run_opts_t *opts) {
    if (opts->output == TDOT_OUTPUT_MQTT)
        mosquitto_lib_init();
    install_signal_handlers();
    int rc = run_connector(conn, cfg, opts);
    if (opts->output == TDOT_OUTPUT_MQTT)
        mosquitto_lib_cleanup();
    return rc;
}

/* ---- multi-connector supervisor ------------------------------------------- */

/* One process runs every connector config found in a directory, each in its
 * own thread (mirrors the Rust runtime's single-service model). */

typedef struct {
    tdot_connector_t *conn;
    tdot_config_t *cfg;
    const tdot_run_opts_t *opts;
    int rc;
} worker_t;

static void *worker_main(void *arg) {
    worker_t *w = arg;
    w->rc = run_connector(w->conn, w->cfg, w->opts);
    return NULL;
}

int tdot_runtime_run_configs(const char *const *paths, size_t npaths,
                             const tdot_run_opts_t *opts) {
    if (npaths == 0) {
        logmsg("error", "no connector configs to run");
        return -1;
    }

    worker_t *workers = calloc(npaths, sizeof *workers);
    pthread_t *threads = calloc(npaths, sizeof *threads);
    size_t started = 0;

    /* Load every config and build its connector up front, so a bad config is
     * reported before anything starts publishing. */
    for (size_t i = 0; i < npaths; i++) {
        char err[256];
        tdot_config_t *cfg = tdot_config_load(paths[i], err, sizeof err);
        if (!cfg) {
            logmsg("error", "%s", err);
            continue;
        }
        tdot_connector_t *conn = tdot_connector_factory(cfg->protocol);
        if (!conn) {
            logmsg("error", "%s: unknown protocol '%s'", paths[i],
                   cfg->protocol);
            tdot_config_free(cfg);
            continue;
        }
        workers[started].conn = conn;
        workers[started].cfg = cfg;
        workers[started].opts = opts;
        logmsg("info", "loaded %s (%s)", paths[i], cfg->protocol);
        started++;
    }
    if (started == 0) {
        free(workers);
        free(threads);
        logmsg("error", "no valid connector configs");
        return -1;
    }

    if (opts->output == TDOT_OUTPUT_MQTT)
        mosquitto_lib_init();
    install_signal_handlers();

    for (size_t i = 0; i < started; i++)
        pthread_create(&threads[i], NULL, worker_main, &workers[i]);
    for (size_t i = 0; i < started; i++)
        pthread_join(threads[i], NULL);

    int rc = 0;
    for (size_t i = 0; i < started; i++) {
        if (workers[i].rc != 0)
            rc = -1;
        workers[i].conn->destroy(workers[i].conn);
        tdot_config_free(workers[i].cfg);
    }
    if (opts->output == TDOT_OUTPUT_MQTT)
        mosquitto_lib_cleanup();
    free(workers);
    free(threads);
    return rc;
}
