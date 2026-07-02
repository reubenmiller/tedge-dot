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

## Status update (2026-07): increment 1 implemented — in the shim layer, not a flow

Increment 1 has landed, but as a **Cumulocity custom-operation shim script** rather than the
`ot-fieldbus-import` flow of option 1. The flow home is not currently possible: the tedge
flows JS runtime (tedge 2.x) exposes no HTTP client — no `fetch`, no `XMLHttpRequest`, no
module system. This was verified empirically by running a probe flow through
`tedge flows test` and dumping `Object.getOwnPropertyNames(globalThis)`; the global set is
the bare ECMAScript library plus `TextEncoder`/`TextDecoder`/`console`/`crypto`. Fetching the
`c8y_ModbusDeviceType` managed object and creating the child external identity therefore
cannot happen inside a flow.

The translator lives in [`operations/c8y-fieldbus-import`](../../operations/c8y-fieldbus-import)
(bash + jq), executed by the c8y mapper via the
[`operations/c8y_ModbusDevice`](../../operations/c8y_ModbusDevice) template — the same
execution point the legacy Python plugin used. On a Cloud Fieldbus assignment it

1. creates the `<main device id>:device:<name>` external identity (type `c8y_Serial`) for the
   UI-created child MO via the mapper's local proxy (`http://127.0.0.1:8001/c8y`) — but **only
   when the device user owns that MO**. Cumulocity's JSON-over-MQTT rejects telemetry for
   managed objects the device does not own ("Current device is not the owner of the given
   source ID") and a device cannot transfer ownership (403), so adopting a tenant-user-owned
   UI placeholder would permanently strand the child's measurements — a constraint the legacy
   plugin shared without detecting it. For foreign-owned placeholders the link is skipped
   (logged): thin-edge registers its own device-owned child under the same name, and the UI
   placeholder remains an unlinked artifact. Reconciling/cleaning the placeholder needs a
   tenant-side actor (increment 4 / a future c8y microservice);
2. fetches the device-type MO from the operation's `type` inventory path and translates
   `c8y_Registers` **and** `c8y_Coils` (beyond legacy parity) into contract points:
   `number`/`startBit`/`noBits`/`signed`/`input` → `address` + `datatype`,
   `multiplier`/`divisor`/`offset` → `transform` (Cloud Fieldbus `offset` is a decimal shift,
   i.e. `transform.decimal_shift`), `unit` → `unit`, and `measurementMapping.type/series` →
   `meta.measurement.group/series`, which `ot-measurement` now honours per signal;
3. publishes one `ot_define_device` command and waits for the connector to persist the TOML
   (so the cloud operation only turns SUCCESSFUL once the config file is updated).

The translation is offline-unit-tested by
[`cloud/modbus/tests/test_fieldbus_import.sh`](../../cloud/modbus/tests/test_fieldbus_import.sh);
the live round-trip is covered by
[`cloud/modbus/tests/fieldbus_c8y.robot`](../../cloud/modbus/tests/fieldbus_c8y.robot)
(increment 2) — verified green (5/5, plus the 6/6 base modbus suite) against a live tenant
on 2026-07-02.

Two further constraints the live run surfaced, now encoded in flows/harness:

- **Measurement series must be bare numbers.** The tedge c8y mapper's measurement converter
  silently drops object-shaped series values (`{value}` and `{value, unit}` alike), so
  `ot-measurement` no longer embeds `sample.unit` in the measurement body; units stay in the
  sample envelope.
- **Gateway-level custom operations need suffix-less declaration files.** A
  `c8y_<Op>.template` alone never reaches the main device's `c8y_SupportedOperations`, so
  Cumulocity keeps the operation PENDING forever; `cloud/modbus/Dockerfile.tedge` installs
  `c8y_ModbusDevice`/`c8y_ModbusConfiguration`/`c8y_SerialConfiguration` as plain files too.

Deferred with TODOs in the script header: alarm/event/status mappings (gap G4), RTU
serial-port resolution (env default until `[connection.serial]` is consulted), signed and
multi-register bit fields. Should a future thin-edge release expose HTTP to flows, the jq
translation can move into the originally planned `ot-fieldbus-import` flow unchanged.
