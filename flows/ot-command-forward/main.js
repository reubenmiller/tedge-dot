// ot-command-forward: forward a thin-edge.io command to the connector command.
//
// Direction: thin-edge.io data model -> OT protocol format.
//   in:  te/device/<device>///cmd/ot_<verb>/<id>          {"status":"init", ...}
//   out: te/device/<device>/ot/<protocol>/cmd/<verb>/<id> {"status":"init", ...}
//
// Protocol-neutral and verb-neutral: a generic `ot_<verb>` command type drives any connector;
// the target protocol is selected by params.protocol (modbus, opcua, ...). The thin-edge command
// type maps to a connector verb by dropping the `ot_` prefix and turning `_` into `-`:
//   ot_write         -> write          (point write; c8y_SetRegister)
//   ot_write_coil    -> write-coil     (coil write; c8y_SetCoil — alias for `write` in the connector,
//                                       kept separate to work around the one-operation-per-command-type limit)
//   ot_set_config    -> set-config     (covers c8y_ModbusConfiguration / c8y_SerialConfiguration)
//   ot_define_device -> define-device  (covers c8y_ModbusDevice / c8y_Coils / c8y_Registers)
//   ot_remove_device -> remove-device
//
// Only new requests (status:"init") are forwarded; the whole init payload is passed through so
// both point writes (point/value/raw) and management verbs (target/config/device) work unchanged.
// The connector drives the command to completion; ot-command-result mirrors the transitions back.

const decoder = new TextDecoder();

export function onMessage(message, context) {
  const protocol = context.config?.protocol || "modbus";

  const parts = message.topic.split("/");
  const device = parts[2];
  const commandType = parts[parts.length - 2];
  const id = parts[parts.length - 1];
  const internalPrefix = "[ot]";

  // Only forward generic OT commands (cmd type prefixed with `ot_`).
  if (!commandType.startsWith("ot_")) return [];
  const verb = commandType.slice(3).split("_").join("-");

  let payload;
  try {
    payload = JSON.parse(decoder.decode(message.payload));
  } catch (_e) {
    return []; // ignore clearing/empty/non-JSON messages
  }
  if ((payload?.status ?? "") !== "init") return []; // only act on new requests

  return [{
    topic: `te/device/${device}/ot/${protocol}/cmd/${verb}/${internalPrefix}${id}`,
    payload: JSON.stringify(payload),
    mqtt: { retain: true, qos: 1 },
  }];
}
