// ot-command-forward: forward a thin-edge.io command to the connector command.
//
// Direction: thin-edge.io data model -> OT protocol format.
//   in:  te/device/<device>///cmd/ot_<verb>/<id>          {"status":"init", ...}
//   out: te/device/<device>/ot/<protocol>/cmd/<verb>/<id> {"status":"init", ...}
//
// Protocol-neutral and verb-neutral: a generic `ot_<verb>` command type drives any connector;
// the target protocol is the one ot-parameter-state recorded for the device (from its samples),
// else params.protocol (modbus, opcua, ...). The thin-edge command type maps to a connector verb
// by dropping the `ot_` prefix and turning `_` into `-`:
//   ot_write         -> write          (point write; c8y_SetRegister)
//   ot_write_coil    -> write-coil     (coil write; c8y_SetCoil — alias for `write` in the connector,
//                                       kept separate to work around the one-operation-per-command-type limit)
//   ot_set_config    -> set-config     (covers c8y_ModbusConfiguration / c8y_SerialConfiguration)
//   ot_define_device -> define-device  (covers c8y_ModbusDevice / c8y_Coils / c8y_Registers)
//   ot_remove_device -> remove-device
//
// One command type is reshaped rather than passed through: `parameter_update` (device
// parameters — the command type of the tedge-parameter-plugin, whose c8y_ParameterUpdate
// template maps the Cumulocity operation onto it; on OT child devices nobody but this flow
// handles it, since tedge-agent only runs workflows for its own entity) becomes ONE connector
// `write-batch`. Two request shapes:
//   1. Cumulocity: { "operation": { "c8y_ParameterUpdate":{}, "c8y_ParameterUpdate_<set>":{},
//                                   "<set>": { "<key>": <value>, ... } }, "c8y-mapper": {...} }
//   2. Direct:     { "set": "<set>", "parameters": { "<key>": <value>, ... } }
// The keys of a set are the connector point ids, unless a point names its own key
// (meta.parameter.key): ot-parameter-state records which point each key of a set belongs to, and
// a key no point has claimed is taken as the point id. The batch request carries an `origin`
// object (command type, set, requested values) that the connector ignores; ot-command-result
// reads it back from the retained init to complete the right thin-edge command.
//
// Only new requests (status:"init") are forwarded; for the pass-through verbs the whole init
// payload is forwarded so both point writes (point/value/raw) and management verbs
// (target/config/device) work unchanged. The connector drives the command to completion;
// ot-command-result mirrors the transitions back.

const decoder = new TextDecoder();

// Marks connector-side command ids so mapper-declared operations can't collide with the
// generic ot_<verb> commands (one-operation-per-command-type limitation). Bracket-free on
// purpose: `[`/`]` in a topic breaks the `[topic] payload` line format of `tedge flows test`.
const INTERNAL_PREFIX = "ot--";

// The management verbs (contract §6.3). They change one connector instance's configuration, so
// they are addressed to that instance's service rather than to a device topic that every
// instance of the protocol hears. See `managementService` for how the service is chosen.
const MANAGEMENT_VERBS = new Set(["set-config", "define-device", "remove-device"]);

// A value usable as one MQTT topic level: non-empty, no level separator, no wildcard.
function isTopicSegment(value) {
  return typeof value === "string" && /^[^/+#]+$/.test(value);
}

function parameterRequest(payload) {
  const op = payload?.operation;
  if (op && typeof op === "object") {
    const marker = Object.keys(op).find((k) => k.startsWith("c8y_ParameterUpdate_"));
    if (!marker) return { error: "c8y_ParameterUpdate operation names no parameter set" };
    const set = marker.slice("c8y_ParameterUpdate_".length);
    const values = op[set];
    if (!values || typeof values !== "object") return { error: `operation carries no '${set}' fragment` };
    return { set, values };
  }
  if (typeof payload?.set === "string" && payload.parameters && typeof payload.parameters === "object") {
    return { set: payload.set, values: payload.parameters };
  }
  return { error: "unsupported parameter_update payload (expected operation or set+parameters)" };
}

// The point a key of `set` belongs to on `device`, as ot-parameter-state recorded it; the key
// itself when no point has claimed it (it is then the point id).
function pointOf(context, device, set, key) {
  const owner = context.mapper.get(`ot-parameter-point:${device}:${set}:${key}`);
  return typeof owner === "string" && owner ? owner : key;
}

// Reshape an parameter_update request into a write-batch request. A request the flow cannot
// interpret is still forwarded, with no writes: the connector rejects an empty batch and
// ot-command-result completes the command as failed with the runtime's reason plus the note
// recorded in origin.error. (This flow cannot publish the failure itself — its output would
// match its own input filter.)
function parameterBatch(payload, context, device) {
  const req = parameterRequest(payload);
  const origin = { command: "parameter_update", set: req.set ?? null, parameters: req.values ?? null };
  if (req.error) origin.error = req.error;
  const writes = req.error
    ? []
    : Object.entries(req.values).map(([key, value]) => ({
        point: pointOf(context, device, req.set, key),
        value,
      }));
  const out = { status: "init", writes, origin };
  if (payload["c8y-mapper"] !== undefined) out["c8y-mapper"] = payload["c8y-mapper"];
  return out;
}

// The service a management command goes to: the one it names, else `tedge-dot-<protocol>` — the
// connector's default service_name. Deterministic on purpose: guessing from retained capability
// descriptors counts services that are long gone (a descriptor outlives its connector).
function managementService(requested, protocol) {
  return requested ?? `tedge-dot-${protocol}`;
}

export function onMessage(message, context) {
  const parts = message.topic.split("/");
  const device = parts[2];
  const commandType = parts[parts.length - 2];
  const id = parts[parts.length - 1];

  // Only forward generic OT commands (cmd type prefixed with `ot_`) and the parameter
  // plugin's `parameter_update` (reshaped below).
  const PARAMETER_UPDATE = "parameter_update";
  if (!commandType.startsWith("ot_") && commandType !== PARAMETER_UPDATE) return [];

  let payload;
  try {
    payload = JSON.parse(decoder.decode(message.payload));
  } catch (_e) {
    return []; // ignore clearing/empty/non-JSON messages
  }
  if ((payload?.status ?? "") !== "init") return []; // only act on new requests

  const protocol = context.mapper.get(`ot-protocol:${device}`) || context.config?.protocol || "modbus";

  let verb;
  let request;
  if (commandType === PARAMETER_UPDATE) {
    verb = "write-batch";
    request = parameterBatch(payload, context, device);
  } else {
    verb = commandType.slice(3).split("_").join("-");
    request = payload;
  }

  if (MANAGEMENT_VERBS.has(verb)) {
    const { service: requested, ...rest } = payload;
    const service = managementService(requested, protocol);
    // Ambiguous, or not a topic segment: forwarding would publish somewhere no connector listens,
    // or to a wildcard. (This flow cannot fail the command itself — its output would match its
    // input.)
    if (!isTopicSegment(service)) return [];
    const origin = rest.origin && typeof rest.origin === "object" ? rest.origin : {};
    return [{
      topic: `te/device/main/service/${service}/ot/cmd/${verb}/${INTERNAL_PREFIX}${id}`,
      // The service topic does not name the entity the command was issued on, so it travels in
      // `origin` (echoed by the connector) for ot-command-result to complete the command there.
      payload: JSON.stringify({ ...rest, origin: { ...origin, device } }),
      mqtt: { retain: true, qos: 1 },
    }];
  }

  return [{
    topic: `te/device/${device}/ot/${protocol}/cmd/${verb}/${INTERNAL_PREFIX}${id}`,
    payload: JSON.stringify(request),
    mqtt: { retain: true, qos: 1 },
  }];
}
