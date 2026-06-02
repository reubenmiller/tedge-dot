// ot-command-result: mirror a connector command result back to the thin-edge command.
//
// Direction: OT protocol format -> thin-edge.io data model.
//   in:  te/device/<device>/ot/<protocol>/cmd/<verb>/<id>  {"status":"executing|successful|failed",...}
//   out: te/device/<device>///cmd/ot_<verb>/<id>           (same payload, retained)
//
// Protocol-neutral and verb-neutral: mirrors any connector's command result onto the matching
// generic `ot_<verb>` command (the connector verb's `-` becomes `_` and gains the `ot_` prefix:
// write -> ot_write, set-config -> ot_set_config, define-device -> ot_define_device, ...).
// Only connector-driven transitions are mirrored (status != init), so the original request that
// ot-command-forward sends to the connector is not echoed back (no loop).

const decoder = new TextDecoder();

export function onMessage(message, context) {
  const prefix = context.config?.command_prefix || "ot_";

  const parts = message.topic.split("/");
  const device = parts[2];
  const verb = parts[parts.length - 2];
  const id = `${parts[parts.length - 1]}`.replace("[ot]", "");
  const commandType = prefix + verb.split("-").join("_");

  let payload;
  try {
    payload = JSON.parse(decoder.decode(message.payload));
  } catch (_e) {
    return []; // ignore clearing/empty/non-JSON messages
  }
  const status = payload?.status ?? "";

  // On init: cache the full payload so metadata (e.g. "c8y-mapper") can be
  // re-attached to connector result messages that won't carry those keys.
  if (status === "init") {
    context.script.set(id, payload);
    return [];
  }
  if (status === "") return []; // ignore clearing/non-JSON messages

  // Merge stored init metadata with the connector result; connector fields win.
  const initPayload = context.script.get(id) ?? {};
  const merged = { ...initPayload, ...payload };

  // Clean up cached state once the command reaches a terminal state.
  if (status === "successful" || status === "failed") {
    context.script.remove(id, null);
  }

  return [{
    topic: `te/device/${device}///cmd/${commandType}/${id}`,
    payload: JSON.stringify(merged),
    mqtt: { retain: true, qos: 1 },
  }];
}
