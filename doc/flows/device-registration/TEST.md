# Testing `device-registration`

A device's link comes up; register it once. A second link message for the same device
produces no duplicate registration:

```sh
tedge flows test --flows-dir ./device-registration/ <<'EOF'
[te/device/plc-1/ot/modbus/status/link] {"status":"connected","since":"2026-05-30T09:59:00.000Z"}
[te/device/plc-1/ot/modbus/status/link] {"status":"connected","since":"2026-05-30T10:05:00.000Z"}
EOF
```

Expected output (registration emitted only on the first message):

```text
[te/device/plc-1//] {"@type":"child-device","name":"plc-1","type":"modbus-device","ot-protocol":"modbus"}
```

> Deduplication uses `context.mapper`, which is in-memory only. After a mapper restart the
> device is registered again on its next link message — which is harmless because registration
> is a retained, idempotent message.
