/* tedge-dot C PoC — minimal Vector DBC parser for the canbus connector.
 *
 * Parses only BO_ (message) and SG_ (signal) lines; everything else in the
 * file is ignored. Mirrors the subset of can-dbc the Rust connector uses.
 */
#ifndef TDOT_DBC_H
#define TDOT_DBC_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    char name[64];
    uint32_t start_bit; /* LSB position for Intel, MSB position for Motorola */
    uint32_t bit_len;
    bool little_endian; /* '@1' = Intel/little, '@0' = Motorola/big */
    bool is_signed;     /* '-' = signed, '+' = unsigned */
    double factor;
    double offset;
} dbc_signal_t;

typedef struct {
    uint32_t can_id; /* extended-frame flag (0x80000000) already masked off */
    bool extended;
    char name[64];
    int dlc;
    dbc_signal_t *signals;
    size_t nsignals;
} dbc_message_t;

typedef struct {
    dbc_message_t *messages;
    size_t n;
} dbc_file_t;

/* Load and parse a DBC file. Returns 0 on success, -1 with err filled. */
int dbc_load(const char *path, dbc_file_t *out, char *err, size_t errlen);
void dbc_free(dbc_file_t *f);

const dbc_message_t *dbc_find_message(const dbc_file_t *f, const char *name);
const dbc_signal_t *dbc_find_signal(const dbc_message_t *m, const char *name);

#ifdef __cplusplus
}
#endif

#endif /* TDOT_DBC_H */
