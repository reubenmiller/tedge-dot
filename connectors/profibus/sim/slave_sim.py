"""
Minimal PROFIBUS DP-V0 slave simulator.

Implements just enough of the FDL/DP protocol to allow the profirust master to
complete a bus cycle:

  1. FDL token passing (ignored — not addressed to us)
  2. Set_Prm  (SAP 61 → SAP 62): accept any parameters, respond SC
  3. Chk_Cfg  (SAP 61 → SAP 62): accept any config, respond SC
  4. Data_Exchange: return canned input bytes, accept output bytes

Physical layer: a pair of virtual serial ports created by socat.
The simulator reads from /dev/ttyPROFIBUS1 and the connector uses
/dev/ttyPROFIBUS0.

Usage (inside the Docker simulator container):
    python3 slave_sim.py [--port /dev/ttyPROFIBUS1] [--address 7]
                         [--baudrate 19200] [--input-bytes 8] [--output-bytes 4]
"""

import argparse
import logging
import os
import select
import socket
import struct
import sys
import time

import serial

logging.basicConfig(
    level=logging.DEBUG,
    format="%(asctime)s %(levelname)s %(name)s: %(message)s",
)
log = logging.getLogger("profibus-slave")


# ── PROFIBUS framing constants ────────────────────────────────────────────────

SD1 = 0x10   # Fixed length frame without data
SD2 = 0x68   # Variable length frame
SD3 = 0xA2   # Fixed length frame with data
SD4 = 0xDC   # Token frame
SC  = 0xE5   # Short acknowledge
ED  = 0x16   # End delimiter

# Service Access Points used in DP initialisation
SAP_DEFAULT   = 0x3E  # 62 — default SAP (Data_Exchange has no SAP extension when DA/SA msb=0)
SAP_MS0       = 0x3D  # 61 — master-slave class 0 (Set_Prm, Chk_Cfg)
SAP_SET_PRM   = 0x3E  # 62 destination SAP for Set_Prm
SAP_CHK_CFG   = 0x3E  # 62 destination SAP for Chk_Cfg

# Frame Control byte values (FC)
FC_SRD_REQUEST  = 0x7C  # Send and Request Data (with Low/High variant)
FC_SRD_HI       = 0x7D
FC_SDN_REQ      = 0x44  # Send Data No Acknowledge (token)


# ── CRC / FCS ────────────────────────────────────────────────────────────────

def fcs(data: bytes) -> int:
    """PROFIBUS FCS: arithmetic sum (wrapping add mod 256) of all bytes from DA to end of PDU."""
    result = 0
    for b in data:
        result = (result + b) & 0xFF
    return result


# ── Frame parsing ─────────────────────────────────────────────────────────────

class TcpSerial:
    """Serial-over-TCP server transport with the small subset of the pyserial
    API this simulator uses (fileno/read/write/close).

    Serves one client at a time; a disconnect simply waits for the next
    connection. This replaces the socat pty/TCP bridge chain, whose
    listener wedged after the first client disconnected (each session after
    the first hung until the container restarted).
    """

    def __init__(self, port: int):
        self.srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.srv.bind(("0.0.0.0", port))
        self.srv.listen(1)
        self.conn: socket.socket | None = None

    def fileno(self) -> int:
        # select()s on the client when connected, the listener otherwise
        return self.conn.fileno() if self.conn else self.srv.fileno()

    def poll_accept(self):
        """Switch to a newly arrived client, dropping the current one.

        Docker's userland proxy does not always forward the client's FIN, so
        a disconnect can be invisible; without this, the dead connection
        would be served forever and new clients would hang in the backlog.
        Called from the main loop between frames.
        """
        ready, _, _ = select.select([self.srv], [], [], 0)
        if not ready:
            return
        conn, peer = self.srv.accept()
        conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        if self.conn:
            log.info("new tcp client — dropping the previous connection")
            self._drop()
        self.conn = conn
        log.info("tcp client connected: %s:%d", *peer)

    def _drop(self):
        if self.conn:
            try:
                self.conn.close()
            except OSError:
                pass
            self.conn = None
            log.info("tcp client disconnected — waiting for the next one")

    def read(self, n: int) -> bytes:
        if self.conn is None:
            self.conn, peer = self.srv.accept()
            self.conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            log.info("tcp client connected: %s:%d", *peer)
            return b""
        # pyserial's read(n) returns up to n bytes within the timeout; frames
        # may arrive fragmented, so accumulate until n bytes or the deadline.
        buf = bytearray()
        deadline = time.monotonic() + 0.2
        while len(buf) < n:
            left = deadline - time.monotonic()
            if left <= 0:
                break
            ready, _, _ = select.select([self.conn], [], [], left)
            if not ready:
                break
            try:
                chunk = self.conn.recv(n - len(buf))
            except OSError:
                chunk = b""
            if not chunk:
                self._drop()
                break
            buf.extend(chunk)
        return bytes(buf)

    def write(self, data: bytes):
        if self.conn is None:
            return
        try:
            self.conn.sendall(data)
        except OSError:
            self._drop()

    def close(self):
        self._drop()
        self.srv.close()


def read_frame(port, timeout_s: float = 0.5) -> bytes | None:
    """Read one complete PROFIBUS frame from the serial port.
    Returns the raw bytes (start delimiter through ED) or None on timeout."""
    deadline = time.monotonic() + timeout_s
    buf = bytearray()

    while time.monotonic() < deadline:
        ready, _, _ = select.select([port], [], [], max(0, deadline - time.monotonic()))
        if not ready:
            break
        b = port.read(1)
        if not b:
            continue

        sd = b[0]

        if sd == SC:
            return bytes([SC])

        if sd == SD4:
            # Token frame: DA(1) SA(1) — 3 bytes total
            rest = port.read(2)
            return bytes([sd]) + rest

        if sd == SD1:
            # Fixed frame without data: SD1 DA SA FC FCS ED — 6 bytes
            rest = port.read(5)
            return bytes([sd]) + rest

        if sd == SD2:
            # Variable length: SD2 L L SD2 DA SA FC [DSAP SSAP] PDU... FCS ED
            length_bytes = port.read(2)
            if len(length_bytes) < 2:
                continue
            le = length_bytes[0]
            if le != length_bytes[1]:
                log.warning("SD2 length mismatch, discarding")
                continue
            # Read repeated SD2
            sd2_check = port.read(1)
            if not sd2_check or sd2_check[0] != SD2:
                continue
            # Read DA..PDU (le bytes) + FCS(1) + ED(1)
            payload = port.read(le + 2)
            if len(payload) < le + 2:
                continue
            return bytes([sd, le, le, SD2]) + payload

        if sd == SD3:
            # Fixed length with data: SD3 DA SA FC DATA(8) FCS ED — 14 bytes
            rest = port.read(13)
            return bytes([sd]) + rest

        # Unknown start byte — skip
        log.debug("unknown start byte 0x%02X, skipping", sd)

    return None


# ── DP state machine ──────────────────────────────────────────────────────────

class DPSlave:
    STATE_WAIT_PRM = "WAIT_PRM"
    STATE_WAIT_CFG = "WAIT_CFG"
    STATE_DATA_EXCHANGE = "DATA_EXCHANGE"

    def __init__(self, address: int, ident_number: int, input_bytes: int, output_bytes: int):
        self.address = address
        self.ident_number = ident_number
        self.state = self.STATE_WAIT_PRM
        self.master_address = 0xFF  # 0xFF = not yet parameterized
        # Fixed canned input values the master will read
        self.inputs = bytearray(input_bytes)
        self._seed_inputs()
        self.outputs = bytearray(output_bytes)

    def _seed_inputs(self):
        """Seed input bytes with recognisable test values."""
        if len(self.inputs) >= 1:
            self.inputs[0] = 0b0000_1010   # DI byte: bits 1 and 3 set
        if len(self.inputs) >= 4:
            # AI0 = 0x1234
            self.inputs[2] = 0x12
            self.inputs[3] = 0x34
        if len(self.inputs) >= 6:
            # AI1 = 0x0064 (100 decimal)
            self.inputs[4] = 0x00
            self.inputs[5] = 0x64

    def _diag_bytes(self) -> bytes:
        """Build a 6-byte DP-V0 diagnostic response."""
        if self.state == self.STATE_WAIT_PRM:
            # Station_Not_Ready (bit 0) — tells master to send Set_Prm
            st1 = 0x01
            st2 = 0x00
            master = 0xFF
        elif self.state == self.STATE_WAIT_CFG:
            # Still not ready, parameterised but not configured
            st1 = 0x01
            st2 = 0x00
            master = self.master_address
        else:
            # DATA_EXCHANGE: all good
            st1 = 0x00
            st2 = 0x04  # Output_OK
            master = self.master_address
        return bytes([
            st1, st2, 0x00, master,
            (self.ident_number >> 8) & 0xFF, self.ident_number & 0xFF,
        ])

    def handle_frame(self, frame: bytes) -> bytes | None:
        """Process an incoming frame and return the response bytes (if any)."""
        if not frame or len(frame) < 1:
            return None

        sd = frame[0]

        if sd == SD4:
            # Token — not addressed to us, ignore
            return None

        if sd == SC:
            return None

        if sd == SD2:
            return self._handle_sd2(frame)

        return None

    def _handle_sd2(self, frame: bytes) -> bytes | None:
        # SD2: [0x68, L, L, 0x68, DA, SA, FC, {DSAP, SSAP,} PDU..., FCS, ED]
        if len(frame) < 8:
            return None
        le = frame[1]
        da_raw = frame[4]
        sa_raw = frame[5]
        fc = frame[6]

        da = da_raw & 0x7F   # strip SAP flag
        da_has_sap = bool(da_raw & 0x80)
        sa = sa_raw & 0x7F
        sa_has_sap = bool(sa_raw & 0x80)

        # Not addressed to us (also not a broadcast 0x7F)
        if da != self.address and da != 0x7F:
            return None

        pdu_start = 7
        dsap = ssap = None
        if da_has_sap:
            if pdu_start >= len(frame) - 2:
                return None
            dsap = frame[pdu_start]
            pdu_start += 1
        if sa_has_sap:
            if pdu_start >= len(frame) - 2:
                return None
            ssap = frame[pdu_start]
            pdu_start += 1

        # PDU is between pdu_start and the last 2 bytes (FCS + ED)
        pdu_end = len(frame) - 2
        pdu = frame[pdu_start:pdu_end] if pdu_end > pdu_start else b""

        log.debug(
            "SD2 da=0x%02X sa=0x%02X fc=0x%02X dsap=%s ssap=%s state=%s pdu=%s",
            da, sa, fc, dsap, ssap, self.state, pdu.hex() if pdu else "(empty)",
        )

        # ── Slave_Diag (dsap=60) ──────────────────────────────────────────
        # Master requests diagnostic data (SAP 60 on the slave).
        # This is the first message in the DP-V0 init sequence.
        if dsap == 0x3C:  # SAP 60 = Slave_Diag
            diag = self._diag_bytes()
            log.debug("Slave_Diag → %s (state=%s)", diag.hex(), self.state)
            # Response: DSAP = master's response SAP (SSAP of original = 62),
            #           SSAP = slave's Slave_Diag SAP = 60 (0x3C)
            master_resp_sap = ssap if ssap is not None else 0x3E
            return self._build_sd2_sap_response(sa, diag, dsap_resp=master_resp_sap, ssap_resp=0x3C)

        # ── Set_Prm (dsap=61) ─────────────────────────────────────────────
        if dsap == 0x3D:  # SAP 61 = Set_Prm
            self.master_address = sa
            if self.state == self.STATE_WAIT_PRM:
                log.info("Set_Prm received from master %d — transitioning to WAIT_CFG", sa)
                self.state = self.STATE_WAIT_CFG
            return bytes([SC])

        # ── Chk_Cfg (dsap=62) ─────────────────────────────────────────────
        if dsap == 0x3E and ssap is not None:  # SAP 62 = Chk_Cfg (has SSAP)
            if self.state in (self.STATE_WAIT_CFG, self.STATE_DATA_EXCHANGE):
                log.info("Chk_Cfg received — entering DATA_EXCHANGE")
                self.state = self.STATE_DATA_EXCHANGE
                return bytes([SC])
            else:
                log.warning("Chk_Cfg before Set_Prm")
                return None

        # ── Data_Exchange (no SAP extension, FC=SRD) ──────────────────────
        if dsap is None and ssap is None and self.state == self.STATE_DATA_EXCHANGE:
            # Accept output bytes from master
            if pdu:
                out_len = min(len(self.outputs), len(pdu))
                self.outputs[:out_len] = pdu[:out_len]
                log.debug("outputs updated: %s", self.outputs.hex())

            # Build SD2 response with input bytes
            inp = bytes(self.inputs)
            return self._build_sd2_response(sa, inp)

        # Request in wrong state — no response (master will retry)
        log.debug("unhandled request dsap=%s in state %s", dsap, self.state)
        return None

    def _build_sd2_response(self, dest_addr: int, data: bytes) -> bytes:
        """Build a SD2 response frame without SAP extension (Data_Exchange)."""
        fc_resp = 0x08
        payload = bytes([dest_addr, self.address, fc_resp]) + data
        cs = fcs(payload)
        le = len(payload)
        return bytes([SD2, le, le, SD2]) + payload + bytes([cs, ED])

    def _build_sd2_sap_response(self, dest_addr: int, data: bytes, dsap_resp: int, ssap_resp: int) -> bytes:
        """Build a SD2 response frame with SAP extensions (e.g. Slave_Diag)."""
        fc_resp = 0x08
        da_byte = dest_addr | 0x80
        sa_byte = self.address | 0x80
        payload = bytes([da_byte, sa_byte, fc_resp, dsap_resp, ssap_resp]) + data
        cs = fcs(payload)
        le = len(payload)
        return bytes([SD2, le, le, SD2]) + payload + bytes([cs, ED])


# ── main loop ─────────────────────────────────────────────────────────────────

def run(port_path: str, address: int, baudrate: int, input_bytes: int, output_bytes: int):
    log.info(
        "starting PROFIBUS slave addr=%d port=%s baud=%d inputs=%d outputs=%d",
        address, port_path, baudrate, input_bytes, output_bytes,
    )

    slave = DPSlave(address, ident_number=0, input_bytes=input_bytes, output_bytes=output_bytes)

    if port_path.startswith("tcp-listen://"):
        port = TcpSerial(int(port_path.removeprefix("tcp-listen://")))
        log.info("listening on %s — slave ready", port_path)
    else:
        port = serial.Serial(
            port=port_path,
            baudrate=baudrate,
            bytesize=serial.EIGHTBITS,
            parity=serial.PARITY_EVEN,
            stopbits=serial.STOPBITS_ONE,
            timeout=0.1,
        )
        log.info("serial port %s opened — slave ready", port_path)

    try:
        while True:
            if isinstance(port, TcpSerial):
                port.poll_accept()
            frame = read_frame(port)
            if frame is None:
                continue
            response = slave.handle_frame(frame)
            if response:
                log.debug("→ %s", response.hex())
                port.write(response)
    except KeyboardInterrupt:
        log.info("slave stopping")
    finally:
        port.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Minimal PROFIBUS DP-V0 slave simulator")
    parser.add_argument("--port", default="/dev/ttyPROFIBUS1")
    parser.add_argument("--address", type=int, default=7)
    parser.add_argument("--baudrate", type=int, default=19200)
    parser.add_argument("--input-bytes", type=int, default=8)
    parser.add_argument("--output-bytes", type=int, default=4)
    args = parser.parse_args()
    run(args.port, args.address, args.baudrate, args.input_bytes, args.output_bytes)
