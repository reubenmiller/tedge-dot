# Testing `modbus-scaling`

Run the flow against a sample envelope without any broker or device:

```sh
echo '[te/device/plc-1/ot/modbus/sample/boiler_temp_raw] {"ts":"2026-05-30T10:00:00.000Z","device":"plc-1","protocol":"modbus","point":"boiler_temp_raw","mode":"typed","datatype":"float32","value":42.5,"value_repr":"number","raw":"422a 0000","quality":"good","addr":{"table":"holding","address":7,"unit_id":1}}' \
  | tedge flows test --flows-dir ./modbus-scaling/
```

Expected output (with the default `params.toml.template`, `unit = "°C"`):

```text
[te/device/plc-1///m/environment] {"environment":{"temperature":{"value":42.5,"unit":"°C"}},"time":"2026-05-30T10:00:00.000Z"}
```

A `bad`-quality sample produces no output:

```sh
echo '[te/device/plc-1/ot/modbus/sample/boiler_temp_raw] {"ts":"2026-05-30T10:00:02.000Z","device":"plc-1","protocol":"modbus","point":"boiler_temp_raw","mode":"typed","datatype":"float32","raw":"","quality":"bad","error":"timeout","addr":{}}' \
  | tedge flows test --flows-dir ./modbus-scaling/
```

Expected: no output (the flow drops non-`good` samples).
