# RFC 0005: Device types, and parameter sets that do not collide

Status: proposed — implemented alongside this RFC (contract §3.1/§3.4/§5/§5.2/§8, both SDKs,
flows, e2e and cloud suites)

## Problem

A parameter set is one twin fragment on the device *and* one Digital Twin Manager property
definition in the tenant ([RFC 0003](0003-parameter-writes.md)). The device-side name is
per-device and harmless; the cloud-side one is **tenant-wide**. The default set name was
`<protocol>_parameters`, so every Modbus device in a tenant declared `modbus_parameters` — and
a tenant with two Modbus device types has two *different* point lists claiming one identifier:

* whichever definition is registered last wins, and the Parameters tab of every other device
  type then renders properties that device does not have, and hides the ones it does;
* the twin fragments still carry each device's own keys, so the UI shows a set whose schema
  belongs to a different device type — the values are right and the labels, limits and ordering
  are someone else's;
* `tedge-dot describe` output from two gateways cannot both be registered, and nothing says so.

The escape hatch existed — `meta.parameter.set` names a set explicitly — but it had to be
repeated on every point of every config, and the *obvious* thing to do (leave it out) was the
thing that collided. RFC 0003 listed this as an open item.

The information needed to name the set correctly is exactly what
[RFC 0004](0004-point-libraries.md) had just made explicit: **a point library is the point list
of one device type**. What was missing was a name for that device type.

## Decision

Give a device a `type` and derive the set name from it.

```toml
# /usr/share/tedge-dot/points.d/modbus/acme-meter-v2.toml
[library]
protocol = "modbus"
type     = "acme-meter-v2"      # what these points describe
```

```toml
[[device]]
name             = "plc-1"
protocol_address = { transport = "tcp", host = "192.168.0.10", port = 502, unit_id = 1 }
points_from      = ["acme-meter-v2"]   # type inherited; or declare `type` here
```

```text
<device type, else the protocol>_<group, default "control">_parameters
   -> acme_meter_v2_control_parameters
```

The full rules are normative in
[contract §5.2](../contract/ot-connector-contract.md#52-parameters-writable-points-as-device-state)
(naming), [§3.1](../contract/ot-connector-contract.md#31-common-protocol-neutral-point-fields)
(the `type` field) and [§3.4](../contract/ot-connector-contract.md#34-point-libraries)
(inheritance). The decisions worth arguing about are below.

### 1. The qualifier is the device type, not the protocol or the device

Three things could qualify a set name, and only one of them has the same lifetime as the
schema:

| Qualifier | Verdict |
| --- | --- |
| **Protocol** (before this RFC) | Says how to reach the device, nothing about which points it has. Every device type on the protocol collides — the bug. |
| **Device type** (this RFC) | Exactly what determines the point list, and shared by every instance of it: one definition per type, registered once, matching every gateway that has that type. |
| Device instance | Never collides, and never shares: a fleet of 500 meters would need 500 tenant-wide definitions of the same schema, and adding a meter would need a tenant admin. |

The type is declared, not derived from the library's file name. A file name is a local
convenience (`points.d/<protocol>/<name>.toml`, and a site copy deliberately shadows a packaged
one under the *same* name); the identifier it would produce is tenant-wide and permanent, and
two protocols' `demo-sim.toml` are different device types with the same file name. Declaring it
also means a library may stay untyped, which is the right answer for a list that is a fragment
of points rather than a device type ("site-extras").

### 2. `group` is relative, `set` is absolute

`meta.parameter.group` names a second set of the *same* device type; it is concatenated into
the derived name, so it cannot reintroduce the collision. `meta.parameter.set` is used verbatim
and bypasses the rule entirely.

Both are needed, and for opposite reasons. A device type with a commissioning set and an
operating set wants two names that are still qualified — `group` gives that, and a config that
says `group = "commissioning"` cannot accidentally claim `commissioning` tenant-wide. But a
tenant that already registered an identifier, or a fleet that deliberately wants one set shared
across device types (a site-wide `plant_setpoints`), needs a way to say a name outright —
`set` is that, and keeping it verbatim is what makes an existing configuration keep working.

Either key also accepts a **list**, and the point joins every set it names:

```toml
meta.parameter = { group = ["control", "commissioning"] }
```

That is the shape operators actually ask for — they group settings by what they are *for*, and
one setpoint legitimately belongs on the commissioning screen and the daily-operation one. The
point's schema goes into each definition and its value into each fragment, so the groups cannot
disagree about the device: a change through either screen updates both. Nothing else in the
design had to move for it, because a set was already a *derived* name rather than a place a
point lives — the implementations simply yield one parameter per (point, set) pair.

**Alternative rejected:** making `set` relative too (qualifying every name). It would have
changed the meaning of a key that already had one, and left no way to express "this exact
identifier", which is the only way to talk to a definition someone else created.

### 3. The device type travels on the wire

The flow that publishes the twin fragments (`ot-parameter-state`) does not read the connector's
TOML — that separation is what makes it protocol-neutral and hot-reloadable — so it has to
learn the type from the broker, and it has to derive *exactly* the name `describe` renders, or
the fragment and the definition would not match.

So the runtime echoes the device `type`:

* in **every sample**, next to `device` and `protocol`. It is per-device routing information,
  not per-point documentation: a short string that costs the same as the `protocol` field
  already there — unlike a point's `name`/`description`, which are static per point and so live
  in the retained capability descriptor (§7) instead;
* on the **link status**, which is retained and published before the first sample. That is what
  covers a device whose parameters are all write-only: those points never produce a sample, so
  without it their set would fall back to the protocol name at exactly the moment a write is
  acknowledged.

Both, rather than either: the sample makes the type available with no ordering assumption, and
the link status makes it available with no sample at all.

**Alternative rejected:** echoing the *resolved set name* per parameter point instead of the
type. It would remove the duplicated naming rule (SDK, C SDK, flow JS) — but it puts a derived,
cloud-shaped value in the driver's envelope, makes the connector interpret `meta.parameter`
(which §3.1 says it never does), and costs more bytes than the type on exactly the points that
are published most often. The rule is a handful of lines — assemble
`<qualifier>_<group>_parameters` and fold every run of non-alphanumerics to one `_` (a *run*, so
that folding bytes in C and characters in Rust cannot disagree on a non-ASCII type name) — and a
mirrored test in each implementation plus the flow suite keep the three copies honest.

**Alternative rejected:** a naming *template* in `[connector]`. It reintroduces the duplication
this RFC removes — the flow would need the same template, from a file it does not read.

### 4. A missing type is a warning, not an error

A device with parameters and no type keeps working: the sets fall back to
`<protocol>_control_parameters`, and `tedge-dot describe` prints a warning on stderr naming the
devices and what to do about it. Refusing to render would break every configuration that has
exactly one device type in its tenant — the common case, and the one for which the old default
was never wrong.

### 5. The same field is the thin-edge entity type

`ot-registration` used to register every OT device as `<protocol>-device` (a flow-level
parameter could change it, for all devices at once). The connector now reports the configured
type on the link status, and the flow prefers it: a child device shows up in the cloud as an
`acme-meter-v2`, not a `modbus-device`, from the same declaration. The flow parameter remains
the fallback for devices that declare no type.

## Implementation

| Where | What |
| --- | --- |
| [contract §3.1](../contract/ot-connector-contract.md#31-common-protocol-neutral-point-fields), [§3.4](../contract/ot-connector-contract.md#34-point-libraries), [§5](../contract/ot-connector-contract.md#5-the-sample-envelope), [§5.2](../contract/ot-connector-contract.md#52-parameters-writable-points-as-device-state), [§8](../contract/ot-connector-contract.md#8-status-and-health) | `type` on the device and the library, the naming rule, the sample and link-status echoes |
| [schemas](../contract/schemas/) | `device.type`, `library.type`, `sample.type`, link `type` |
| [descriptor.rs](../../impl/rust/crates/sdk/src/descriptor.rs) / [descriptor.c](../../impl/c/sdk/src/descriptor.c) | `SetNaming` / `tdot_set_naming_t`: the derived name, the `group`/`set` split, the untyped-device warning list |
| [library.rs](../../impl/rust/crates/sdk/src/library.rs) / [config.c](../../impl/c/sdk/src/config.c) | the type a device inherits from its first typed library |
| [runtime.rs](../../impl/rust/crates/sdk/src/runtime.rs) / [envelope.c](../../impl/c/sdk/src/envelope.c), [runtime.c](../../impl/c/sdk/src/runtime.c) | `type` in samples and on the link status, rebuilt on a live config reload |
| [flows/ot-parameter-state](../../flows/ot-parameter-state/) | learns the type from samples and the link status, derives the same names |
| [flows/ot-registration](../../flows/ot-registration/) | the declared type becomes the entity type |
| Tests | mirrored unit tests in both SDKs, `just test-flows`, `just c-describe-parity`, the Modbus (type from a library) and OPC UA (type on the device) e2e suites, and the Cumulocity suite |

## Breaking changes

Set names change, which matters to anyone who registered the old ones:

* the default is now `<type-or-protocol>_control_parameters`, not `<protocol>_parameters`;
* a tenant's existing DTM definitions must be re-registered under the new identifiers
  (`tedge-dot describe` prints them), or pinned to the old name — `--set <name>` on `describe`
  and `default_set` in the `ot-parameter-state` flow still force one name for everything;
* the old twin fragment is retained under its old name until it is cleared — nothing removes
  it, so both the device twin and the Cumulocity managed object keep a stale copy of the
  values. Clear it once per device after upgrading:

```sh
tedge mqtt pub -r -q 1 'te/device/plc1///twin/modbus_parameters' ''
```

A write-only parameter is the one value that does not follow a live `type` change at all: the
flow records its set when the write is acknowledged and never re-derives it (the point produces
no samples to correct it), so it keeps publishing to the pre-change set until the mapper
restarts.

When a `type` changes on a *running* connector (`set-config`, `define-device`) the sets are
renamed from the next publish. Each readable parameter's next sample names its new set, and
`ot-parameter-state` drops it from the old one, so a fragment under the old name empties and is
cleared on its own; one still holding a write-only parameter stays retained until it is cleared
the same way. `ot-registration` — which registers a device once per mapper lifetime — keeps the
entity type it first published until the mapper restarts.

Dropping a point from one of several groups, or making it read-only, is handled the same way:
its next sample drops it from the sets it left. A point removed from the configuration is dropped
from every set as soon as the link status, which lists the configured points, is republished.

`type` itself is optional everywhere, so no configuration fails to load.

The SDK APIs changed for out-of-tree connectors (a source break, loud at compile time, not a
silent one): Rust's `parameter_of` became `parameters_of` (one entry per set), `default_set` is
gone, and `parameters`/`invalid_keys` take an `Option<&str>` override; C's `tdot_param_of` and
`tdot_param_default_set` were replaced by `tdot_param_sets`/`tdot_param_sets_free`/
`tdot_param_is` and `tdot_param_set_name`.
