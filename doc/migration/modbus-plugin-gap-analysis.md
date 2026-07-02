# Gap analysis: replacing `thin-edge/modbus-plugin` with the tedge-dot Modbus connector

| Field | Value |
| --- | --- |
| Status | Analysis (2026-07) |
| Legacy | [thin-edge/modbus-plugin](https://github.com/thin-edge/modbus-plugin) (Python, `tedge_modbus.reader` + `tedge_modbus.operations`) |
| Replacement | [`crates/connector-modbus`](../../crates/connector-modbus/src/lib.rs) + [`flows/`](../../flows/) + [`operations/`](../../operations/) shims |
| Related | [Migration guide](migration-guide.md), [Modbus connector spec](../connectors/modbus-connector-spec.md), [RFC 0002](../rfc/0002-cloud-fieldbus-integration.md) |

This document compares the legacy Python plugin feature-by-feature against what tedge-dot
ships today (`cloud/modbus/` end-to-end harness included), names each gap, and proposes a
concrete closure for it. The legacy behaviour statements below are taken from the plugin
sources (`tedge_modbus/reader/reader.py`, `tedge_modbus/reader/mapper.py`,
`tedge_modbus/operations/*.py`, `operations/*` templates, `nfpm.yaml`, `tests/`).

## 1. Feature comparison

Status legend: **covered** — parity (or superset) exists today; **partial** — the primitive
exists but a translation/behaviour piece is missing; **missing** — no equivalent yet.

### 1.1 Configuration surface

| modbus-plugin | tedge-dot equivalent | Status |
| --- | --- | --- |
| `modbus.toml [modbus].pollinterval` (global, per-device override) | `connector.poll_interval`, plus per-device and per-point `poll_interval` ([`crates/sdk/src/config.rs`](../../crates/sdk/src/config.rs)) | covered |
| `[modbus].transmitinterval` (stored by `c8y_ModbusConfiguration`, **never enforced** by the reader) | `ot-measurement` `min_interval` / `combine_interval` params (actually enforced) | covered |
| `[modbus].loglevel` | `connector.log_level` | covered |
| `[modbus].combinemeasurements` (+ per-device, per-mapping override) | `ot-measurement` `combine` + `combine_interval` (flow-wide). Per-signal override via `point.meta` exists for `on_change`/`deadband`/`min_interval`/`debounce` but **not** for `combine` | partial |
| `[serial].*` (port, baudrate, stopbits, parity, databits) | `[connection.serial]` defaults + per-device `protocol_address` overrides (spec §3.1–3.2) | covered |
| `[thinedge].mqtthost` / `mqttport` | `[mqtt] host` / `port` | covered |
| `[thinedge].subscribe_topics` | implicit — command topics derived from the [contract](../contract/ot-connector-contract.md) | covered |
| `devices.toml [[device]]` (name, address, ip, port, protocol, littlewordendian) | `[[device]]` + `protocol_address` (tcp/rtu, `unit_id`), per-point `word_order` | covered |
| `[[device.registers]]` number/startbit/nobits/signed/input | `point.address` (`table`, `address`, `count`, `start_bit`, `bit_count`) + `datatype` (`bitfield` feature) | covered |
| Per-register `littleendian` | `point.endianness` | covered |
| Scaling `multiplier`/`divisor`/`decimalshiftright`/`offset` | `point.transform` `{multiplier, divisor, decimal_shift, offset}` applied by the connector ([`crates/sdk/src/model.rs`](../../crates/sdk/src/model.rs)) | covered |
| `measurementmapping.templatestring` (`{"G":{"S":%%}}`) | `ot-measurement` `group`/`series`/`target_topic`/`point_separator` params | covered |
| Per-register `on_change` | `point.meta.on_change` honoured by `ot-measurement` (plus `deadband`, `min_interval`, `debounce` — superset) | covered |
| `alarmmapping` (raise on 0→1 edge, never clears) | [`ot-alarm`](../../flows/ot-alarm/) flow (threshold + hysteresis, raises **and clears**); requires the point to be mapped as a measurement first (`include_boolean`) | covered |
| `eventmapping` (event on value change) | [`ot-event`](../../flows/ot-event/) flow | covered |
| Coils / discrete inputs | `table = "coil"` / `"discrete_input"`, `datatype = "bool"` | covered |
| `datatype = "float"` with `nobits = 16` (half precision) | no `float16` in the SDK datatype set (`bool`..`float64`, `string`, `bytes`) | missing |
| Config hot-reload on file change (watchdog) | SDK runtime live-reload (config watch + `set-config` persistence) | covered |

### 1.2 Cumulocity operations

The legacy operation templates live under `modbus-plugin/operations/`; the tedge-dot shims
under [`operations/`](../../operations/) and are bridged by
[`ot-command-forward`](../../flows/ot-command-forward/) /
[`ot-command-result`](../../flows/ot-command-result/) (internal command ids carry the `ot--`
marker so mapper-cleared retained commands are not re-forwarded).

| Legacy operation | Legacy behaviour | tedge-dot equivalent | Status |
| --- | --- | --- | --- |
| `c8y_SetRegister` | Explicit-address payload (`address`, `register`, `startBit`, `noBits`, `ipAddress`, `value`) **and** name-based `metrics[]` payload with prefix matching; integer read-modify-write masking; float 16/32/64 writes | Shim → `ot_write` → connector `write` verb. Payload is **point-id based only**: `{"point": "temp_u16", "value": 4242}`. Bit-field read-modify-write per spec §6 | partial |
| `c8y_SetCoil` | Explicit-address (`coil`, `address`, `ipAddress`, `value`) and name-based `metrics[]` | Shim → `ot_write_coil` → connector `write-coil`; `{"point": "coil_rw", "value": true}` | partial |
| `c8y_ModbusConfiguration` | Writes `transmitRate`+`pollingRate` into `modbus.toml`; publishes retained twin `te/device/main///twin/c8y_ModbusConfiguration` | Shim → `ot_set_config` patching `connector.poll_interval` only; `transmitRate` dropped; **no twin echo** | partial |
| `c8y_SerialConfiguration` | Writes `[serial]` into `modbus.toml`; publishes twin `c8y_SerialConfiguration` on main | Shim → `ot_set_config` patching `connection.serial`; **no twin echo** | partial |
| `c8y_ModbusDevice` | Cloud Fieldbus flow: registers external id `<device.id>:device:<name>` (type `c8y_Serial`) for the **UI-created child MO** via the local c8y proxy, fetches the device-type MO from `payload.type` (an inventory path), translates `c8y_Registers[*]` (address, scaling, `measurementMapping.type/series`) into `devices.toml` | Shim → `ot_define_device`, but the payload must already be a connector-shaped `device` object (`protocol_address`, `point[]`). The stock Cloud Fieldbus payload (`protocol`, `address`, `ipAddress`, `type`, `id`, `name`) is **not understood**; no device-type fetch, no external-id linking | **missing** (biggest gap) |
| `c8y_Registers` / `c8y_Coils` | Effectively stubs (dump the raw argument to a file; the data is consumed via `c8y_ModbusDevice`'s type fetch instead) | Intentionally dropped — points travel inside `ot_define_device` (`operations/README.md`) | covered (by design) |
| Command status flow (`init`→`executing`→`successful`/`failed`, `reason`) | reader handles `modbus_SetRegister`/`modbus_SetCoil` thin-edge commands | contract command lifecycle + `ot-command-result` mirror | covered |

### 1.3 Cloud Fieldbus device-type polling / translation

This is where the legacy plugin — itself only a partial Cloud Fieldbus implementation — still
does more than tedge-dot.

| Capability | modbus-plugin | tedge-dot | Status |
| --- | --- | --- | --- |
| Fetch `c8y_ModbusDeviceType` MO on `c8y_ModbusDevice` operation (via `http://127.0.0.1:8001/c8y` proxy) | yes (`c8y_modbus_device.py`) | no | missing |
| Translate type `c8y_Registers[*]` → device points (number/startBit/noBits/signed/multiplier/divisor/offset-as-decimal-shift/input) | yes | no (translation absent; target primitives — `point.address`, `point.transform` — exist) | missing |
| Translate `measurementMapping.type/series` → measurement shaping | yes (builds `templatestring`) | no (target exists: `point.meta` + `ot-measurement` `point_separator`/params) | missing |
| Translate type `c8y_Coils[*]` | **no** (legacy only reads `c8y_Registers`) | no | missing in both |
| Translate `alarmMapping` / `eventMapping` / `statusMapping` from the device type | **no** (legacy ignores them; alarms/events only via hand-edited `devices.toml`) | no (targets exist: `ot-alarm` / `ot-event` params, `point.meta`) | missing in both |
| Link the UI-created child MO by creating external id `<device.id>:device:<name>` | yes | no (`ot-registration` registers by name; the c8y mapper would create a *second* MO instead of adopting the UI one) | missing |
| Re-sync when a device type is edited in the tenant (inventory polling, as the reference c8y fieldbus agent does) | no (fetch happens only when the operation fires) | no | missing in both (optional) |
| Export device-side config to the cloud UI (twin) | `te/device/<child>///twin/c8y_ModbusDevice` (port/address/protocol/ipAddress) | `ot-registration` `twin_fragment = "c8y_ModbusDevice"` publishing the connector's `LinkReport.info` descriptor ([`cloud/modbus/params/ot-registration.params.toml`](../../cloud/modbus/params/ot-registration.params.toml)) | covered |

### 1.4 Child devices, transports, packaging

| Capability | modbus-plugin | tedge-dot | Status |
| --- | --- | --- | --- |
| Child registration (retained `te/device/<name>//`, `@type: child-device`, `type: modbus-device`) | on config (re)load | [`ot-registration`](../../flows/ot-registration/) on first `status/link = connected`; `device_type` param | covered |
| External id naming `<device.id>:device:<name>` | via c8y mapper (same) | via c8y mapper (asserted in [`cloud/modbus/tests/modbus_c8y.robot`](../../cloud/modbus/tests/modbus_c8y.robot)) | covered |
| Command capability advertisement (`cmd/modbus_SetRegister`, `cmd/modbus_SetCoil`) | reader publishes empty retained capability topics | `ot-registration` `command_capabilities = "ot_write,ot_write_coil"` | covered |
| Service registration (`te/device/main/service/...`) | `tedge-modbus-plugin` | `tedge-dot` service health (asserted in the robot suite) | covered |
| Modbus TCP | pymodbus `ModbusTcpClient` | `tokio-modbus` tcp | covered |
| Modbus RTU (serial) | pymodbus `ModbusSerialClient` + `[serial]` defaults merged per device | `tokio-modbus` rtu + `tokio-serial`, `[connection.serial]` defaults ([`crates/connector-modbus/src/lib.rs`](../../crates/connector-modbus/src/lib.rs) `build_context`) | covered |
| Contiguous-range read batching | `_build_query_model` | spec §5 batching | covered |
| Failed reads visible | logged only (silently dropped downstream) | `quality: "bad"` samples with `error` | covered (superset) |
| deb/rpm packaging | nfpm, package `tedge-modbus-plugin`, systemd `tedge-modbus-plugin.service`, config under `/etc/tedge/plugins/modbus/` | goreleaser deb/rpm (`.goreleaser.yaml`), `packaging/tedge-dot.service`, config `/etc/tedge/plugins/ot/modbus.toml` | covered |
| `conflicts`/`replaces` on the legacy package for clean cut-over | n/a | not declared in `.goreleaser.yaml` | partial |
| tedge-log-plugin / tedge-configuration-plugin integration snippets (`type = "modbus"`, `type = "modbus-devices"`) | documented in README | not yet documented for the connector config | partial (docs only) |
| Install via c8y Software Management | yes (deb) | yes (deb; exercised by `cloud/modbus/Dockerfile.tedge`) | covered |

## 2. Gaps and proposed closures

Ordered by recommended implementation priority.

### G1 — Cloud Fieldbus `c8y_ModbusDevice` translation (missing)

The legacy handler accepts the payload the Cloud Fieldbus UI actually sends
(`{"protocol","address","ipAddress","id","name","type"}` where `type` is an inventory path to
the `c8y_ModbusDeviceType` MO), fetches the type, and rewrites `devices.toml`. tedge-dot's
shim assumes a connector-shaped `device` object that no stock UI produces.

**Closure:** the `ot-fieldbus-import` flow of [RFC 0002](../rfc/0002-cloud-fieldbus-integration.md)
(increment 1). Concretely:

1. Change the [`operations/c8y_ModbusDevice`](../../operations/c8y_ModbusDevice) shim to emit an
   `ot_fieldbus_import` thin-edge command carrying the raw UI payload (keep the current
   connector-shaped path as a fallback when `payload.device` is present).
2. New flow `flows/ot-fieldbus-import`: on `cmd/ot_fieldbus_import/+`, fetch
   `GET <c8y_proxy><payload.type>` via the mapper's HTTP proxy, then
   - `POST /identity/globalIds/<payload.id>/externalIds` with
     `{"externalId": "<device.id>:device:<name>", "type": "c8y_Serial"}` so the c8y mapper
     adopts the UI-created child MO (legacy parity);
   - map each `c8y_Registers[*]` (and, beyond legacy parity, `c8y_Coils[*]`) to a
     `device.point[]` entry: `number/startBit/noBits/signed` → `address` + `datatype`,
     `multiplier/divisor/offset` → `transform`, `unit` → `point.unit`,
     `measurementMapping.type/series` → `point.meta` measurement naming,
     `noUpdateIfEqual`/send-on-change → `meta.on_change`, alarm/event/status mappings →
     `meta` fields read by `ot-alarm`/`ot-event`;
   - emit one `ot_define_device` command; the SDK runtime persists it into the TOML
     (`crates/sdk/src/runtime.rs` `apply_define_device`) and live-reloads.
3. Round-trip Robot test in `cloud/modbus/tests/` (RFC 0002 increment 2, keywords in §4).

> **Update (2026-07): closed** — but in the c8y shim layer, not a flow: the tedge flows JS
> runtime has no HTTP client (no `fetch`/`XMLHttpRequest`/modules), so the fetch + external-id
> steps cannot run in a flow. [`operations/c8y-fieldbus-import`](../../operations/c8y-fieldbus-import)
> (bash + jq, executed by the [`operations/c8y_ModbusDevice`](../../operations/c8y_ModbusDevice)
> template) now handles both payload shapes, creates the child external id, translates
> `c8y_Registers`/`c8y_Coils` (measurementMapping → `meta.measurement`, honoured by
> `ot-measurement`), and drives `ot_define_device`. Offline unit test:
> [`cloud/modbus/tests/test_fieldbus_import.sh`](../../cloud/modbus/tests/test_fieldbus_import.sh).
> Alarm/event/status mappings remain G4. See the RFC 0002 status update for details.

### G2 — Legacy `c8y_SetRegister` / `c8y_SetCoil` payload compatibility (partial)

Explicit-address payloads (asset-table widget, existing runbooks, the legacy Robot tests) and
name-based `metrics[]` payloads fail against the new point-id shims.

**Closure:** extend the two operation shims (or add a translation step in
`ot-command-forward`) to accept all three shapes:
`{point,value}` (native), `{register|coil, address, ipAddress, startBit, noBits, value}`
(resolve to a point by matching `point.address` against the connector config / twin
descriptor; synthesise a raw bit-field write if no point matches), and
`{metrics:[{name,value}]}` (prefix-match `name` against point ids, legacy matching rule).
The connector's `write` verb (spec §6, incl. read-modify-write bit-fields) already covers the
execution side.

### G3 — Config twin echo for `c8y_ModbusConfiguration` / `c8y_SerialConfiguration` (partial)

Legacy publishes the applied settings as retained twin fragments on the main device
(`te/device/main///twin/c8y_ModbusConfiguration|c8y_SerialConfiguration`); the legacy Robot
suite asserts them. The new `ot_set_config` path applies and persists the change but nothing
reflects it back to the inventory.

**Closure:** a small `ot-config-twin` flow subscribed to
`te/+/+/ot/+/cmd/set-config/+` that, on `status: "successful"`, republishes the applied
`config` object as the matching twin fragment(s). Alternatively the SDK runtime publishes an
"effective config" descriptor after every reload (also serves RFC 0002's export path).
`transmitRate` should be accepted and mapped to the `ot-measurement` `min_interval` param (or
explicitly documented as dropped — the legacy reader stored but never enforced it).

### G4 — Cloud Fieldbus alarm/event/status mappings from device types (missing in both)

Neither implementation translates `alarmMapping`/`eventMapping`/`statusMapping` from the
device type; tedge-dot has the runtime pieces (`ot-alarm` threshold+hysteresis, `ot-event`
on-change, `point.meta` echoed in every sample).

**Closure:** part of G1's mapping table — emit the type's alarm/event definitions into
`point.meta`, and teach `ot-alarm`/`ot-event` to read per-signal `meta` overrides the same way
`ot-measurement` already does (RFC 0002 "Signal metadata is the bridge"). This is a
parity-plus item; it can land after G1 ships with measurement mappings only.

### G5 — Cut-over ergonomics and small parity items

- **Packaging:** declare `conflicts`/`replaces: tedge-modbus-plugin` in `.goreleaser.yaml`
  (nfpm supports both) so installing `tedge-dot` via Software Management cleanly removes the
  Python plugin; document `tedge-log-plugin.toml` / `tedge-configuration-plugin.toml` entries
  for the connector log and `/etc/tedge/plugins/modbus/modbus.toml`.
- **`float16`:** legacy supports 16-bit half-precision floats (`datatype="float"`,
  `nobits=16`). Either add `float16` to the SDK datatype set + `decode_primitive`, or state in
  the migration guide that it is unsupported and must be widened at the source.
- **Per-signal `combine` override:** honour `meta.combine` in `ot-measurement` for parity with
  the legacy per-mapping `combinemeasurements`.
- **Migration tool:** the `tedge-ot-migrate` converter sketched in the
  [migration guide §5](migration-guide.md) does not exist yet; note that scaling now lands in
  `point.transform` (connector), not a separate scaling flow, which simplifies its output.

## 3. Migration mapping: config files

See the [migration guide](migration-guide.md) for the full field-by-field rules. Summary and a
worked example against the plugin's shipped sample config (`modbus-plugin/config/*.toml`):

| Legacy file / key | tedge-dot home |
| --- | --- |
| `modbus.toml [modbus]` | `[connector]` (`poll_interval`, `log_level`) |
| `modbus.toml [serial]` | `[connection.serial]` |
| `modbus.toml [thinedge]` | `[mqtt]` |
| `devices.toml [[device]]` | `[[device]]` + `protocol_address` |
| `devices.toml [[device.registers]]` / `[[device.coils]]` | `[[device.point]]` (`address`, `datatype`, `transform`, `unit`, `meta`) |
| `measurementmapping` / `combinemeasurements` / `on_change` | `ot-measurement` params + `point.meta` |
| `alarmmapping` / `eventmapping` | `ot-alarm` / `ot-event` params |

Legacy (`devices.toml`):

```toml
[[device]]
name = "TestCase1"
address = 1
ip = "simulator"
port = 502
protocol = "TCP"
littlewordendian = false

[[device.registers]]
number = 3
startbit = 0
nobits = 16
signed = true
offset = -20
input = false
name = "Test_Int16"
measurementmapping.templatestring = "{\"Test\":{\"Int16\":%% }}"

[[device.coils]]
number = 2
input = false
alarmmapping.severity = "MAJOR"
alarmmapping.text = "This alarm should be created once"
alarmmapping.type = "TestAlarm"
```

Replacement connector config (`/etc/tedge/plugins/modbus/modbus.toml`):

```toml
[connector]
protocol      = "modbus"
poll_interval = "2s"          # was [modbus].pollinterval = 2

[[device]]
name             = "TestCase1"
protocol_address = { transport = "tcp", host = "simulator", port = 502, unit_id = 1 }
default_mode     = "typed"

  [[device.point]]
  id        = "Test.Int16"     # point_separator "." -> group "Test", series "Int16"
  datatype  = "int16"          # signed, 16 bits, holding table (input = false)
  address   = { table = "holding", address = 3, count = 1 }
  transform = { offset = -20 } # was multiplier/divisor/decimalshiftright/offset

  [[device.point]]
  id       = "TestAlarm"
  datatype = "bool"
  address  = { table = "coil", address = 2, count = 1 }
```

Flow params: `ot-measurement` with `point_separator = "."` reproduces the
`{"Test":{"Int16": ...}}` measurement shape from the point id alone; one `ot-alarm` instance
with `series = "TestAlarm"`, `threshold = 1`, `hysteresis = 0`, `severity = "major"`,
`alarm_type = "TestAlarm"` reproduces the coil alarm (and additionally clears it when the coil
drops — the legacy plugin never cleared).

Operations mapping is unchanged from the [migration guide §3](migration-guide.md): all five
`c8y_*` operations remain, backed by `ot_write` / `ot_write_coil` / `ot_set_config` /
`ot_define_device` instead of Python handlers.

## 4. Missing Robot Framework keywords for Cloud Fieldbus e2e tests

[`cloud/modbus/tests/modbus_c8y.robot`](../../cloud/modbus/tests/modbus_c8y.robot) uses the
`Cumulocity` library (`robotframework-c8y`, inspected at
`code/robotframework-c8y/Cumulocity/Cumulocity.py`, the same library the legacy plugin's
`tests/` pin). The RFC 0002 increment-2 round-trip test (create a modbus device type in the
tenant, assign it to a child via `c8y_ModbusDevice`, verify TOML + measurements, remove it)
needs the following.

Already provided by the library (sufficient for everything the suites do today):
`Set Device`, `Set Managed Object`, `Device Should Exist`, `External Identity Should Exist`,
`Create Operation` (arbitrary fragments), `Operation Should Be SUCCESSFUL/FAILED/...`,
`Should Contain Supported Operations`, `Managed Object Should Have Fragments` /
`... Fragment Values`, `Delete Managed Object`, `Device Should Have Measurements/Alarm/s/Event/s`,
`Device Should Have A Child Devices`, `Should Have Services`, `Execute Shell Command`.

Missing — generic inventory CRUD (candidates for upstreaming to `robotframework-c8y`):

| Proposed keyword | Signature | Purpose |
| --- | --- | --- |
| `Create Managed Object` | `Create Managed Object    fragments=<json>    -> Dict` | Create the fieldbus device-type MO (`{"name": "SimType", "type": "c8y_ModbusDeviceType", "c8y_IsDeviceType": {}, "c8y_ModbusDeviceType": {...}, "c8y_Registers": [...], "c8y_Coils": [...]}`) and the UI-style child-device placeholder MO. No generic inventory-create keyword exists today (only `Create Inventory Binary`). |
| `Update Managed Object` | `Update Managed Object    ${mo_id}    fragments=<json>    -> Dict` | Edit register definitions of an existing device type to test re-import/re-sync. |
| `Get Managed Object` | `Get Managed Object    ${mo_id}    -> Dict` | Fetch the type/child MO to assert stored definitions (complements the existing assertion keywords, which only work on the "current" device context). |
| `Add Child Device Reference` | `Add Child Device Reference    ${parent_id}    ${child_id}` | Mirror the UI, which attaches the placeholder child MO to the gateway before issuing `c8y_ModbusDevice`. |

Missing — fieldbus-level convenience keywords (start as a suite resource
`cloud/modbus/tests/fieldbus.resource`, upstream later if they prove general):

| Proposed keyword | Signature | Purpose |
| --- | --- | --- |
| `Create Modbus Device Type` | `Create Modbus Device Type    ${name}    registers=<json list>    coils=<json list>    protocol=TCP    -> ${type_mo}` | Wraps `Create Managed Object` with the `c8y_ModbusDeviceType` shape (register fields: `number`, `startBit`, `noBits`, `signed`, `multiplier`, `divisor`, `offset`, `input`, `name`, `unit`, `measurementMapping.{type,series}`, optional alarm/event/status mappings). |
| `Assign Modbus Device Type To Child` | `Assign Modbus Device Type To Child    ${child_name}    ${type_mo_id}    address=1    ip_address=simulator    protocol=TCP    -> ${operation}` | Creates the child placeholder MO, references it under the gateway, then `Create Operation` with `{"c8y_ModbusDevice": {"protocol", "address", "ipAddress", "id": <child mo id>, "name", "type": "/inventory/managedObjects/<type id>"}}` — exactly the payload the Cloud Fieldbus UI sends and `c8y_modbus_device.py` parses. |
| `Remove Modbus Device Type` | `Remove Modbus Device Type    ${type_mo_id}` | Deletes the type MO (wraps existing `Delete Managed Object`) and, for the unassign path, sends the follow-up `c8y_ModbusDevice` operation without the type / an `ot_remove_device`-backed operation. |
| `Child Device Should Have Point` | `Child Device Should Have Point    ${child}    ${point_id}` | Device-side assertion that the imported point landed in `/etc/tedge/plugins/modbus/modbus.toml` (wraps existing `Execute Shell Command` + `Should Contain`). |

Everything else the round-trip test needs (operation status, external identity of
`<device.id>:device:<child>`, measurements with the type's group/series, alarm assertions) is
already covered by existing keywords.

## 5. Recommended implementation order

1. **G1** `ot-fieldbus-import` flow + shim change (the only *missing* legacy capability; RFC 0002 increment 1).
2. **G2** legacy write-payload compatibility in the `c8y_SetRegister`/`c8y_SetCoil` shims (unblocks drop-in replacement for existing tenants/widgets).
3. **§4 keywords + round-trip Robot test** in `cloud/modbus/tests/` (RFC 0002 increment 2; gates G1/G2 against a real tenant).
4. **G3** config twin echo (+ `transmitRate` handling) so the c8y UI reflects applied configuration.
5. **G5** cut-over items: package `conflicts`/`replaces`, log/config-plugin doc snippets, `float16` decision, `meta.combine`, and the `tedge-ot-migrate` converter.
