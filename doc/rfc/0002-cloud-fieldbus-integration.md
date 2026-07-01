# RFC 0002: Cumulocity Cloud Fieldbus integration

Status: proposed

## Goal

Let a user configure tedge-dot devices/points from the Cumulocity **Cloud Fieldbus** UI
(https://cumulocity.com/docs/device-integration/cloud-fieldbus/) with a seamless transition,
while the device itself stays **config-file driven**: the TOML file remains the single source
of truth on the device, editable offline, versionable, and identical whether or not a cloud is
attached.

## Position

Cloud Fieldbus models a fieldbus deployment as managed objects:

- **device types** (`c8y_ModbusDeviceType` etc.) hold register/coil definitions — name,
  address, scaling (multiplier/divisor/decimal shift), unit, and the measurement/alarm/event
  mappings a signal should produce;
- **child devices** reference a device type plus transport parameters (address/unit id);
- operations (`c8y_ModbusConfiguration`, `c8y_SetRegister`, `c8y_SetCoil`) push configuration
  and writes to the agent.

tedge-dot already has the matching device-side primitives, so **no new device mechanism is
needed**: the SDK's protocol-neutral management verbs (`set-config`, `define-device`,
`remove-device`) patch and persist the TOML config with live reload, and `cloud/modbus/`
already maps `c8y_SetRegister`/`c8y_SetCoil`/`c8y_ModbusDevice` operations onto connector
verbs through thin-edge custom operations and the `ot-command-forward`/`ot-command-result`
flows.

The missing piece is a **translator** from Cloud Fieldbus device-type objects to contract
`define-device` payloads. Two candidate homes:

1. **Device-side flow (preferred).** A `ot-fieldbus-import` flow subscribes to the mapper's
   operation topics. When the tenant assigns a device type to a child device, the flow fetches
   the device-type managed object (via the mapper's HTTP proxy), converts each register/coil
   definition into a contract point (`address`, `datatype`, `transform`, `unit`, and the
   measurement/alarm mappings into the point's `meta` table), and emits one `define-device`
   management command. The connector persists it into the TOML file — after which the device
   behaves exactly as if the file had been written by hand. No cloud microservice to operate,
   works for every OT protocol that has a device-type mapping.
2. **Cloud microservice.** A small Cumulocity microservice that watches device-type
   assignments and sends the equivalent `c8y_DeviceProfile`/custom operation. More UI control,
   but per-tenant deployment and lifecycle cost. Revisit only if the flow approach hits a wall
   (e.g. tenants that disallow the HTTP proxy).

The reverse direction also holds: because `define-device` persists into the TOML file, a
hand-edited device can be *exported* by publishing its device descriptor (`LinkReport.info`
already feeds `ot-registration`'s twin fragment, e.g. `c8y_ModbusDevice`) so the cloud UI
shows the deployed configuration.

## Signal metadata is the bridge

Cloud Fieldbus attaches per-signal behaviour (measurement mapping, alarm thresholds,
send-on-change) to each register definition. The contract now carries an uninterpreted
per-point `meta` table that the runtime echoes in every sample envelope, and the
`ot-measurement` flow honours `meta.on_change` / `meta.deadband` / `meta.min_interval` /
`meta.debounce`. A Cloud Fieldbus register definition therefore round-trips losslessly:

```
c8y_ModbusDeviceType register        →  [[device.point]]
  name, number, multiplier, ...      →  id, address, transform, unit
  sendMeasurementTemplate            →  meta = { group/series naming }
  noUpdateIfEqual / send-on-change   →  meta = { on_change = true }
  alarm mapping                      →  meta = { alarm threshold fields, read by ot-alarm }
```

## Increments

1. `ot-fieldbus-import` flow: `c8y_ModbusDevice`-assignment → `define-device` (modbus first).
2. Round-trip e2e test in `cloud/modbus/tests/` (Robot): assign type in tenant → TOML updated
   → samples flow → measurements appear with the type's mappings.
3. Generalise the translator per protocol (the device-type shape is protocol-specific; each
   connector spec gains a "fieldbus mapping" section).
4. Export path: publish the effective TOML-derived descriptor as a twin fragment so the UI
   reflects device-side edits.
