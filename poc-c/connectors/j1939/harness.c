/*
 * Step-0 spike harness for the J1939 connector.
 *
 * Drives the vendored Open-SAE-J1939 library on a SocketCAN vcan0 bus and proves
 * we can capture (source_address, PGN, payload, len) for arbitrary PGNs.
 *
 * KEY FINDING (verified on rpi4 aarch64 + vcan): under the SOCKETCAN platform the
 * library reads frames DIRECTLY via socketcan_receive — CAN_Read_Message does not
 * call a registered read callback (that path is INTERNAL_CALLBACK only). So there
 * are no callbacks to register; each new frame lands in j1939.ID / j1939.data and
 * Listen returns non-RX_MSG_NONE. We capture straight from the struct. (Note:
 * RX_MSG_UNKNOWN is also Listen's idle/default return, so it must NOT be used as
 * the "a frame arrived" signal — rx != RX_MSG_NONE is the correct test.)
 *
 * Multi-packet (Transport Protocol) PGNs are reassembled inside the library and,
 * once the Step-0 TP.DT patch lands, delivered via on_raw_pgn (README.md#the-patch).
 *
 * Build: see CMakeLists.txt. This file is a throwaway spike, not the connector.
 */

#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "Open_SAE_J1939/Open_SAE_J1939.h"
#include "Hardware/SocketCAN.h"

/* J1939 id -> (PGN, SA), mirroring the library's own extraction (PDU1 vs PDU2). */
static uint32_t id_to_pgn(uint32_t id) {
    uint8_t pf = (uint8_t)(id >> 16);
    return (pf >= 240) ? ((id >> 8) & 0x3FFFFu) : ((id >> 8) & 0x3FF00u);
}
static uint8_t id_to_sa(uint32_t id) { return (uint8_t)(id & 0xFF); }

/* Generic capture sink. Non-static so the patched TP.DT default case (multi-packet
 * path) links against it too. In the real connector this is the PGN-cache write. */
void on_raw_pgn(uint8_t sa, uint32_t pgn, const uint8_t *data, uint32_t len) {
    printf("CAPTURED  SA=0x%02X  PGN=%lu (0x%lX)  len=%u  data=",
           sa, (unsigned long)pgn, (unsigned long)pgn, len);
    for (uint32_t i = 0; i < len; i++)
        printf("%02X ", data[i]);
    printf("\n");

    /* EEC1: decode SPN 190 (engine speed), bytes 4-5, 0.125 rpm/bit, little-endian
     * — the same math as dbc.c extract_intel(start_bit=24, bit_len=16). */
    if (pgn == 61444 && len >= 5) {
        uint16_t raw = (uint16_t)(data[3] | ((uint16_t)data[4] << 8));
        printf("          -> SPN190 engine_speed = %.1f rpm\n", raw * 0.125);
    }
    fflush(stdout);
}

static volatile sig_atomic_t g_run = 1;
static void on_sigint(int _s) { (void)_s; g_run = 0; }

int main(void) {
    J1939 j1939;
    memset(&j1939, 0, sizeof j1939);
    signal(SIGINT, on_sigint);

    if (socketcan_setup("vcan0") < 0) {
        fprintf(stderr, "socketcan_setup(vcan0) failed — is vcan0 up? "
                        "run scripts/setup-vcan.sh\n");
        return 1;
    }
    Open_SAE_J1939_Startup_ECU(&j1939);
    printf("listening on vcan0 (Ctrl-C to stop)...\n");
    fflush(stdout);

    while (g_run) {
        ENUM_J1939_RX_MSG rx = Open_SAE_J1939_Listen_For_Messages(&j1939);
        if (rx == RX_MSG_NONE)
            continue; /* no new frame */
        /* A frame was read; the library stored it in j1939.ID / j1939.data.
         * (Reassembled multi-packet PGNs arrive via on_raw_pgn from the patch.) */
        on_raw_pgn(id_to_sa(j1939.ID), id_to_pgn(j1939.ID), j1939.data, 8);
    }

    Open_SAE_J1939_Closedown_ECU(&j1939);
    return 0;
}
