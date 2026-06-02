# Testing `modbus-alarm`

Raise then clear, demonstrating hysteresis (threshold 70, clears below 65):

```sh
tedge flows test --flows-dir ./modbus-alarm/ <<'EOF'
[te/device/plc-1///m/environment] {"environment":{"temperature":{"value":75.0,"unit":"°C"}},"time":"2026-05-30T10:00:00.000Z"}
[te/device/plc-1///m/environment] {"environment":{"temperature":{"value":67.0,"unit":"°C"}},"time":"2026-05-30T10:00:01.000Z"}
[te/device/plc-1///m/environment] {"environment":{"temperature":{"value":62.0,"unit":"°C"}},"time":"2026-05-30T10:00:02.000Z"}
EOF
```

Expected output:

```text
[te/device/plc-1///a/boiler_overheat] {"severity":"major","text":"Boiler temperature high (75 >= 70)","time":"2026-05-30T10:00:00.000Z"}
[te/device/plc-1///a/boiler_overheat]
```

- `75.0` raises the alarm.
- `67.0` is inside the hysteresis band (65–70): no output.
- `62.0` is below the clear threshold: the alarm is cleared (empty retained message).

This flow consumes the measurement produced by `modbus-scaling`, so the alarm logic is
entirely independent of Modbus — point it at any scaled series from any OT connector.
