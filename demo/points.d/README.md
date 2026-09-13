# Point libraries

A **point library** is the data point list of one *device type*, in its own
file, with no connection information in it (contract
[§3.4](../../doc/contract/ot-connector-contract.md#34-point-libraries),
[RFC 0004](../../doc/rfc/0004-point-libraries.md)). Device instances reference
it and supply only their own address:

```toml
[[device]]
name             = "plc-7"
protocol_address = { transport = "tcp", host = "192.168.0.17", port = 502, unit_id = 1 }
points_from      = ["demo-sim"]
```

This directory ships one library per demo simulator, all named `demo-sim` and
told apart by their protocol directory:

| Library | Points of |
| --- | --- |
| `modbus/demo-sim.toml` | the pymodbus simulator |
| `opcua/demo-sim.toml` | the demo OPC UA server |
| `canbus/demo-sim.toml` | the demo CAN sender (signals from `test.dbc`) |
| `canopen/demo-sim.toml` | the demo CANopen node |
| `profibus/demo-sim.toml` | the demo DP slave |

They are what the demo configs in `/usr/share/tedge-dot/demo/` reference, and
the quickest way to experiment: point a `[[device]]` at an address, add
`points_from = ["demo-sim"]`, and you have a working configuration without
typing a single point definition.

Libraries are looked up by name under `<dir>/<protocol>/<name>.toml`, in each
directory of the search path, first match winning:

| Directory | For |
| --- | --- |
| `/etc/tedge/plugins/ot/points.d` | a site's own libraries; shadow packaged ones of the same name |
| `/usr/share/tedge-dot/points.d` | libraries shipped by a package (this directory, once installed) |

Override the path with `[connector] point_library_path` or, when running from a
source checkout, `TEDGE_DOT_POINT_LIBRARY_PATH` (colon-separated).

Because `/etc` is searched first, copying one of these libraries there to edit
it means the packaged copy stops being used — deliberately, so your edits
survive an upgrade. Give it a different name if you want both.

## The device type

A library can name the **device type** its points describe:

```toml
[library]
protocol = "modbus"
type     = "acme-meter-v2"
```

Every device that references the library inherits it (a `type` on the
`[[device]]` wins), and it is what the device's *parameter set* names are
derived from — `acme_meter_v2_control_parameters` rather than
`modbus_control_parameters`. Those names are tenant-wide identifiers in
Cumulocity, so without a type two different Modbus device types would claim the
same one. It also becomes the thin-edge entity type of the registered child
device.

Points choose their group with `parameter.group`, and a point may name several:

```toml
[[point]]
id   = "coil_rw"
parameter = { group = ["control", "commissioning"] }
```

gives `..._control_parameters` and `..._commissioning_parameters`, both carrying that point —
which is how one signal appears on two operator screens without either going stale. The Modbus
demo library above does exactly this. See
[RFC 0005](../../doc/rfc/0005-device-types-and-parameter-sets.md).
