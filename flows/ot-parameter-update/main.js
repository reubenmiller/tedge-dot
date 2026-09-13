// ot-parameter-update: a cloud `parameter_update` operation -> one connector write-batch.
//
// Direction: thin-edge.io data model -> thin-edge.io data model (a reshape, not a transport).
//   in:  te/device/<device>///cmd/parameter_update/<id>   {"status":"init", ...}
//   out: te/device/<device>///cmd/ot_write_batch/ot--<id> {"status":"init","writes":[...],...}
//
// With ot-parameter-result, this is ALL that remains of the old ot-command-forward +
// ot-command-result pair (RFC 0006 §7). Every other command type now goes straight to the
// connector: `ot_write`, `ot_write_batch` and the management verbs are command types this
// project defines, whose payload is the contract's own request and whose state machine is the
// contract's own — so the connector subscribes to those topics and drives them itself, and
// there is nothing for a flow to carry.
//
// `parameter_update` is different, and is why this flow exists: its request is the
// tedge-parameter-plugin's shape (a c8y_ParameterUpdate operation with a set fragment), which
// IS a cloud shape. Turning that into a contract write-batch is exactly the kind of work flows
// are for.
//
// Two request shapes are accepted:
//   1. Cumulocity: { "operation": { "c8y_ParameterUpdate":{}, "c8y_ParameterUpdate_<set>":{},
//                                   "<set>": { "<point>": <value>, ... } }, "c8y-mapper": {...} }
//   2. Direct:     { "set": "<set>", "parameters": { "<point>": <value>, ... } }
// The keys of a set ARE the connector point ids.
//
// Nothing here names a protocol or a service: the batch is addressed to the DEVICE, and the
// connector that owns its points answers it (contract §6.5). The 0.1 flow had to know the
// protocol, and could not write to a device before a sample had revealed it.

const decoder = new TextDecoder();

// The bridged command gets an id of its own: it is a SECOND command on a second command type,
// and reusing the cloud operation's id would put a command the c8y mapper did not create on an
// id it recognises. ot-parameter-result answers only ids carrying this prefix, so a batch from
// anywhere else passes it by. Bracket-free on purpose — `[`/`]` in a topic breaks the
// `[topic] payload` line format of `tedge flows test`.
const INTERNAL_PREFIX = "ot--";

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

export function onMessage(message, _context) {
  // Topic: te/device/<device>///cmd/parameter_update/<id>
  const parts = message.topic.split("/");
  const device = parts[2];
  const id = parts[parts.length - 1];

  let payload;
  try {
    payload = JSON.parse(decoder.decode(message.payload));
  } catch (_e) {
    return []; // ignore clearing/empty/non-JSON messages
  }
  // Only new requests; the transitions ot-parameter-result mirrors back land here too.
  if ((payload?.status ?? "") !== "init") return [];

  // A request this flow cannot interpret is still forwarded, with no writes: the connector
  // rejects an empty batch and ot-parameter-result completes the command as failed with the
  // runtime's reason plus the note recorded in origin.error. (This flow cannot publish the
  // failure itself — its output would match its own input filter.)
  const req = parameterRequest(payload);
  const origin = { command: "parameter_update", set: req.set ?? null, parameters: req.values ?? null };
  if (req.error) origin.error = req.error;
  const writes = req.error ? [] : Object.entries(req.values).map(([point, value]) => ({ point, value }));
  const batch = { status: "init", writes, origin };
  if (payload["c8y-mapper"] !== undefined) batch["c8y-mapper"] = payload["c8y-mapper"];

  return [{
    topic: `te/device/${device}///cmd/ot_write_batch/${INTERNAL_PREFIX}${id}`,
    payload: JSON.stringify(batch),
    mqtt: { retain: true, qos: 1 },
  }];
}
