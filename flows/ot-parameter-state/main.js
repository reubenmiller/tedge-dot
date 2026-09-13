// ot-parameter-state: keep the device twin's parameter sets in sync with the connector.
//
// Direction: OT protocol format -> thin-edge.io data model.
//   in:  te/device/<device>/ot/<protocol>/manifest                 (retained: what the points are)
//        te/device/<device>/ot/<protocol>/sample/<point>           (reads of parameter points)
//        te/device/<device>///cmd/ot_write/<id>                    (single write results)
//        te/device/<device>///cmd/ot_write_batch/<id>              (batch write results)
//   out: te/device/<device>///twin/<set>                           (retained: { <point>: value })
//
// A *parameter* is a point the device manifest (contract §8.2) lists with a `parameter.sets`
// entry: the connector derives the sets once — a point whose `access` permits writes, or that
// opts in via its `parameter` field, grouped by the RFC 0005 naming rule — and publishes the result,
// so this flow never re-derives a name and never needs the connector's configuration file.
// Each set is one twin fragment keyed by point id: the same sets `tedge-dot describe` declares
// in the cloud.
//
// Where values come from:
//   * readable parameters: every good sample (so the twin follows the device, including
//     changes made locally on the PLC/HMI);
//   * write-only parameters (access = "write"): the last acknowledged write — the device cannot
//     be read back, so this is the *commanded* state, not a measured one. It is unknown (absent
//     from the twin) until the first successful write after the mapper started;
//   * read/write parameters are also updated optimistically from a successful write, then
//     confirmed/corrected by the next sample.
//
// A sample or a write result for a device whose manifest has not been seen yet is left alone:
// the manifest is retained and replayed on a restart, and the next sample after it arrives
// places the value. Nothing is buffered.
//
// Shared state (context.mapper):
//   "ot-manifest:<device>"                -> the device manifest (also kept by ot-measurement)
//   "ot-parameter-values:<device>:<set>"  -> { <point>: value }
//
// The `ot-protocol:<device>` entry is gone with ot-command-forward (RFC 0006 §7): nothing needs
// the protocol to ADDRESS a device any more, so nothing has to learn it from a sample first.

const decoder = new TextDecoder();

// A set name becomes BOTH a twin fragment key and a segment of the topic it is published on,
// so it must be a plain identifier — the same rule `tedge-dot describe` refuses to render
// without. The manifest applies it before publishing; checked again here because `default_set`
// is free text from a params file, and `#` or `+` would make an illegal PUBLISH topic.
function isValidSet(name) {
  return typeof name === "string" && /^[A-Za-z0-9_]+$/.test(name);
}

// Trimmed with the SDKs' definition of whitespace (C's isspace, which `tedge-dot describe`
// applies to --set) rather than JS's Unicode-aware trim, so the flow and the CLI agree on what
// a blank `default_set` is, and on the exact spelling of a padded one.
function trimC(s) {
  return String(s).replace(/^[ \t\n\v\f\r]+|[ \t\n\v\f\r]+$/g, "");
}

// Remember (or forget, on a clearing message) a device's manifest.
function rememberManifest(context, device, payloadBytes) {
  let manifest = false;
  try {
    const parsed = JSON.parse(decoder.decode(payloadBytes));
    if (parsed && typeof parsed === "object" && parsed.points && typeof parsed.points === "object") {
      manifest = parsed;
    }
  } catch (_e) {
    // an empty retained message clears the manifest of a removed device
  }
  context.mapper.set(`ot-manifest:${device}`, manifest);
}

// The sets a point belongs to: what the manifest resolved, or — with `default_set` — that one
// name for every parameter. `null` when the manifest has not been seen; `false` when the point
// is not a parameter (or does not exist).
function setsOf(context, device, point) {
  const manifest = context.mapper.get(`ot-manifest:${device}`);
  if (!manifest) return null;
  const entry = manifest.points?.[point];
  const sets = entry?.parameter?.sets;
  if (!Array.isArray(sets) || sets.length === 0) return false;
  const forced = trimC(context.config?.default_set || "");
  if (forced) return isValidSet(forced) ? [forced] : false;
  const usable = sets.filter(isValidSet);
  return usable.length ? usable : false;
}

// The points an ot_write / ot_write_batch result names: the `results` of a terminal
// transition, or the single `point` of an ot_write.
function successfulWrites(commandType, payload) {
  const updates = {};
  if (commandType === "ot_write" && typeof payload.point === "string" && payload.value !== undefined) {
    updates[payload.point] = payload.value;
  } else if (commandType === "ot_write_batch") {
    for (const r of payload.results ?? []) {
      if (r?.status === "successful" && typeof r.point === "string" && r.value !== undefined) {
        updates[r.point] = r.value;
      }
    }
  }
  return updates;
}

// Apply {point: value} updates for a device; returns the twin messages of the changed sets.
// A point in several sets updates each of them, so the groups never disagree about its value.
function applyValues(context, device, updates) {
  const changed = new Set();
  for (const [point, value] of Object.entries(updates)) {
    if (value === undefined) continue;
    const sets = setsOf(context, device, point);
    if (!sets) continue;
    for (const set of sets) {
      const key = `ot-parameter-values:${device}:${set}`;
      const values = context.mapper.get(key) || {};
      if (JSON.stringify(values[point]) === JSON.stringify(value)) continue;
      values[point] = value;
      context.mapper.set(key, values);
      changed.add(set);
    }
  }
  return [...changed].map((set) => ({
    topic: `te/device/${device}///twin/${set}`,
    payload: JSON.stringify(context.mapper.get(`ot-parameter-values:${device}:${set}`) || {}),
    mqtt: { retain: true, qos: 1 },
  }));
}

export function onMessage(message, context) {
  // Topic shapes: te/device/<device>/ot/<protocol>/manifest
  //               te/device/<device>/ot/<protocol>/sample/<point>
  //               te/device/<device>///cmd/<command type>/<id>
  const parts = message.topic.split("/");
  const device = parts[2];
  const kind = parts[5];

  if (kind === "manifest") {
    rememberManifest(context, device, message.payload);
    return [];
  }

  let payload;
  try {
    payload = JSON.parse(decoder.decode(message.payload));
  } catch (_e) {
    return [];
  }
  if (!payload || typeof payload !== "object") return [];

  if (kind === "sample") {
    const point = payload.point || parts[6];
    if (payload.quality !== "good" || payload.value === undefined) return [];
    return applyValues(context, device, { [point]: payload.value });
  }

  if (kind === "cmd" && payload.status === "successful") {
    return applyValues(context, device, successfulWrites(parts[6], payload));
  }
  return [];
}
