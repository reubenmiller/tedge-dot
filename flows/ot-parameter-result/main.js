// ot-parameter-result: mirror the bridged write-batch back onto its cloud command.
//
// Direction: thin-edge.io data model -> thin-edge.io data model.
//   in:  te/device/<device>///cmd/ot_write_batch/ot--<id>  (the connector's transitions)
//   out: te/device/<device>///cmd/parameter_update/<id>    (the cloud command, completed)
//
// The other half of ot-parameter-update (RFC 0006 §7). Separate flows because the engine
// refuses to let one flow publish to a topic matching its own input filter, and the bridge must
// both raise the batch and watch it.
//
// Only batches this bridge raised are answered — they carry the `ot--` id prefix. A batch from
// anywhere else (an operator, a script, another flow) is the connector's business alone: it is
// already answered on the topic it arrived on, and there is no cloud command to complete.

const decoder = new TextDecoder();

const INTERNAL_PREFIX = "ot--";
const PARAMETER_UPDATE = "parameter_update";

export function onMessage(message, context) {
  // Topic: te/device/<device>///cmd/ot_write_batch/<id>
  const parts = message.topic.split("/");
  const device = parts[2];
  const id = parts[parts.length - 1];
  if (!id.startsWith(INTERNAL_PREFIX)) return [];
  const cloudId = id.slice(INTERNAL_PREFIX.length);

  let payload;
  try {
    payload = JSON.parse(decoder.decode(message.payload));
  } catch (_e) {
    return []; // ignore clearing/empty/non-JSON messages
  }
  const status = payload?.status ?? "";

  // On init: cache the request so its metadata ("c8y-mapper", "origin") can be re-attached to
  // the connector's transitions, which do not carry the cloud's keys.
  if (status === "init") {
    context.script.set(id, payload);
    return [];
  }
  if (status === "") return [];

  const initPayload = context.script.get(id) ?? {};
  // `origin` from the cached request, else from the result itself: the connector echoes it into
  // every transition it publishes (§6.4), which is what makes a REPLAYED terminal result
  // routable after a mapper restart has emptied the in-memory cache. Without it the Cumulocity
  // operation would wait forever on a command that never completes.
  const originOf = (p) => (p?.origin && typeof p.origin === "object" ? p.origin : null);
  const origin = originOf(initPayload) ?? originOf(payload);

  // Merge the cached request's metadata with the connector's result; connector fields win. The
  // batch's own request body (`writes`) is not echoed: the cloud command keeps its own shape,
  // and `origin.set` / `origin.parameters` already say what was asked.
  const { writes: _writes, ...initMeta } = initPayload;
  const merged = { ...initMeta, ...payload };
  if (status === "failed" && origin?.error) {
    merged.reason = [origin.error, payload.reason].filter((r) => r).join("; ");
  }
  if (status === "successful" || status === "failed") {
    context.script.remove(id, null);
  }

  return [{
    topic: `te/device/${device}///cmd/${PARAMETER_UPDATE}/${cloudId}`,
    payload: JSON.stringify(merged),
    mqtt: { retain: true, qos: 1 },
  }];
}
