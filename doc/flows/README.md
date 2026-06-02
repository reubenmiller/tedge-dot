# Example flows

These flow packages show how **all transformation moves out of the driver** and into
[thin-edge.io flows](https://thin-edge.github.io/thin-edge.io/extend/flows/). They consume the
connector's [sample envelopes](../contract/ot-connector-contract.md#5-the-sample-envelope)
from `te/device/<device>/ot/<protocol>/sample/<point>` and produce the standard thin-edge
data model on `te/device/<device>///m|e|a/...`.

They are intentionally small and protocol-neutral: the same `modbus-scaling` flow works on
OPC-UA or BACnet samples, because every connector emits the same envelope.

| Flow | What it replaces from the old plugin | Reads | Emits |
| --- | --- | --- | --- |
| [modbus-scaling](modbus-scaling/) | `mapper.py` scaling + `templatestring` JSON shaping | `sample/<point>` | `m/<group>` measurement |
| [modbus-alarm](modbus-alarm/) | `mapper.py` alarm state machine | `m/<group>` | `a/<type>` alarm (with hysteresis) |
| [device-registration](device-registration/) | `reader.py` child-device registration | `ot/<protocol>/status/link` | `te/device/<device>//` registration |

Each package contains `flow.toml`, `main.js`, `params.toml.template`, and a `TEST.md` with a
runnable `tedge flows test` command and expected output, so correctness can be shown without a
device or the cloud.

## Why this is better than the old `templatestring`

The legacy plugin shipped per-register JSON like
`measurementmapping.templatestring = "{\"Test\":{\"Int16\":%%}}"` and applied a fixed scaling
formula in Python. To change the scale factor or the measurement name you edited config that
the driver parsed and re-deployed the driver. Here:

- the **driver** only emits a decoded number (`value`) with metadata,
- the **flow** owns naming, scaling, units and shaping in readable JavaScript,
- you test changes offline with `tedge flows test`,
- you hot-reload without restarting anything,
- and the same flow logic is reusable across every OT protocol.
