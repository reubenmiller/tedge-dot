/* tedge-dot C PoC — minimal Vector DBC parser (BO_ / SG_ lines only). */
#include "dbc.h"

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void *grow(void *arr, size_t n, size_t elem) {
    /* doubling growth, starting at 4 */
    if ((n & (n - 1)) == 0 && n >= 4)
        return realloc(arr, n * 2 * elem);
    if (n == 0)
        return realloc(arr, 4 * elem);
    return arr;
}

static const char *skip_ws(const char *p) {
    while (*p == ' ' || *p == '\t')
        p++;
    return p;
}

/* "BO_ 416 ENGINE_STATUS: 8 Vector__XXX" */
static int parse_bo(const char *p, dbc_message_t *m) {
    memset(m, 0, sizeof *m);
    unsigned long raw_id;
    char *end;
    raw_id = strtoul(p, &end, 10);
    if (end == p)
        return -1;
    m->extended = (raw_id & 0x80000000ul) != 0;
    m->can_id = (uint32_t)(raw_id & 0x1FFFFFFFul);
    p = skip_ws(end);
    size_t i = 0;
    while (*p && *p != ':' && *p != ' ' && i < sizeof m->name - 1)
        m->name[i++] = *p++;
    m->name[i] = '\0';
    p = skip_ws(p);
    if (*p != ':' || i == 0)
        return -1;
    if (sscanf(p + 1, " %d", &m->dlc) != 1)
        return -1;
    return 0;
}

/* "SG_ RPM : 0|16@1+ (1,0) [0|65535] \"rpm\" Vector__XXX"
 * (an optional multiplexer indicator between name and ':' is skipped) */
static int parse_sg(const char *p, dbc_signal_t *s) {
    memset(s, 0, sizeof *s);
    p = skip_ws(p);
    size_t i = 0;
    while (*p && *p != ' ' && *p != '\t' && *p != ':' &&
           i < sizeof s->name - 1)
        s->name[i++] = *p++;
    s->name[i] = '\0';
    if (i == 0)
        return -1;
    const char *colon = strchr(p, ':');
    if (!colon)
        return -1;
    unsigned sb, bl;
    char order, sign;
    double factor, offset;
    if (sscanf(colon + 1, " %u|%u@%c%c ( %lf , %lf )", &sb, &bl, &order,
               &sign, &factor, &offset) != 6)
        return -1;
    if ((order != '0' && order != '1') || (sign != '+' && sign != '-'))
        return -1;
    s->start_bit = sb;
    s->bit_len = bl;
    s->little_endian = order == '1';
    s->is_signed = sign == '-';
    s->factor = factor;
    s->offset = offset;
    return 0;
}

int dbc_load(const char *path, dbc_file_t *out, char *err, size_t errlen) {
    memset(out, 0, sizeof *out);
    FILE *fp = fopen(path, "r");
    if (!fp) {
        snprintf(err, errlen, "cannot read DBC file %s: %s", path,
                 strerror(errno));
        return -1;
    }

    char line[512];
    int lineno = 0;
    while (fgets(line, sizeof line, fp)) {
        lineno++;
        const char *p = skip_ws(line);
        if (strncmp(p, "BO_ ", 4) == 0) {
            dbc_message_t m;
            if (parse_bo(p + 4, &m) != 0) {
                snprintf(err, errlen, "%s:%d: malformed BO_ line", path,
                         lineno);
                goto fail;
            }
            dbc_message_t *ms = grow(out->messages, out->n, sizeof *ms);
            if (!ms)
                goto oom;
            out->messages = ms;
            out->messages[out->n++] = m;
        } else if (strncmp(p, "SG_ ", 4) == 0) {
            if (out->n == 0) {
                snprintf(err, errlen, "%s:%d: SG_ before any BO_", path,
                         lineno);
                goto fail;
            }
            dbc_signal_t s;
            if (parse_sg(p + 4, &s) != 0) {
                snprintf(err, errlen, "%s:%d: malformed SG_ line", path,
                         lineno);
                goto fail;
            }
            dbc_message_t *m = &out->messages[out->n - 1];
            dbc_signal_t *ss = grow(m->signals, m->nsignals, sizeof *ss);
            if (!ss)
                goto oom;
            m->signals = ss;
            m->signals[m->nsignals++] = s;
        }
    }
    fclose(fp);
    return 0;

oom:
    snprintf(err, errlen, "out of memory parsing %s", path);
fail:
    fclose(fp);
    dbc_free(out);
    return -1;
}

void dbc_free(dbc_file_t *f) {
    if (!f)
        return;
    for (size_t i = 0; i < f->n; i++)
        free(f->messages[i].signals);
    free(f->messages);
    f->messages = NULL;
    f->n = 0;
}

const dbc_message_t *dbc_find_message(const dbc_file_t *f, const char *name) {
    for (size_t i = 0; i < f->n; i++)
        if (strcmp(f->messages[i].name, name) == 0)
            return &f->messages[i];
    return NULL;
}

const dbc_signal_t *dbc_find_signal(const dbc_message_t *m,
                                    const char *name) {
    for (size_t i = 0; i < m->nsignals; i++)
        if (strcmp(m->signals[i].name, name) == 0)
            return &m->signals[i];
    return NULL;
}
