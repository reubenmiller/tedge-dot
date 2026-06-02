// ot-measurement: convert a connector "sample" envelope into a thin-edge.io measurement.
//
// Direction: OT protocol format  ->  thin-edge.io data model.
//   in:  te/device/<device>/ot/<protocol>/sample/<point>   (connector sample envelope)
//   out: te/device/<device>///m/<group>                    (thin-edge measurement)
//
// Protocol-neutral: it consumes the generic OT Connector Contract sample, so the same flow maps
// modbus, opcua or any other connector. The connector is the "dumb" driver plus the per-point
// engineering transform (multiplier/divisor/decimal_shift/offset is declared on the point and
// applied by the connector via the SDK), so the sample already carries the scaled value. This
// flow owns naming (group/series), units, change-detection (on_change) and optional batching of
// a device's series into one measurement (combine).

const decoder = new TextDecoder();

// Recursively merge src into target (nested objects merged, other values overwritten).
function deepMerge(target, src) {
  for (const key of Object.keys(src)) {
    const a = target[key];
    const b = src[key];
    if (
      a && typeof a === "object" && !Array.isArray(a) &&
      b && typeof b === "object" && !Array.isArray(b)
    ) {
      deepMerge(a, b);
    } else {
      target[key] = b;
    }
  }
  return target;
}

// Shape the measurement body (without the time field) from a scaled value:
//   { <group>: { <series>: value | { value, unit } } }
// The unit is a per-signal property declared on the connector point and echoed in the sample,
// so it is taken from sample.unit (not a flow-wide override).
function shapeBody(cfg, sample, scaled) {
  const { group, series } = resolveNaming(cfg, sample);
  const unit = sample.unit || "";
  const seriesValue = unit ? { value: scaled, unit } : scaled;
  return { [group]: { [series]: seriesValue } };
}

// Resolve the measurement group + series for a sample. Precedence:
//   1. explicit cfg.group / cfg.series (flow-wide overrides),
//   2. point-id convention: when point_separator is set and the point id contains it, the id is
//      split once into "<group><sep><series>" (e.g. "." maps "Environment.Temperature" ->
//      group "Environment", series "Temperature"). This lets ONE flow instance remap many
//      signals just by how their point ids are named on the connector.
//   3. defaults: group = sample protocol, series = point id.
function resolveNaming(cfg, sample) {
  let group = cfg.group || "";
  let series = cfg.series || "";
  const sep = cfg.point_separator || "";
  if (sep && (!group || !series)) {
    const id = sample.point || "";
    const idx = id.indexOf(sep);
    if (idx > 0 && idx < id.length - sep.length) {
      if (!group) group = id.slice(0, idx);
      if (!series) series = id.slice(idx + sep.length);
    }
  }
  group = group || sample.protocol || "ot";
  series = series || sample.point;
  return { group, series };
}

export function onMessage(message, context) {
  const sample = JSON.parse(decoder.decode(message.payload));
  const cfg = context.config || {};

  // Optionally restrict this flow instance to a single point id.
  const point = cfg.point || "";
  if (point && sample.point !== point) return [];

  // Only forward good-quality readings.
  if (sample.quality !== "good") return [];

  // Resolve a numeric value. Booleans (coils) become 1/0 when include_boolean is enabled.
  // The connector has already applied the point's engineering transform, so the sample value
  // is the final scaled reading.
  let value = sample.value;
  if (sample.value_repr === "boolean" || typeof value === "boolean") {
    if (String(cfg.include_boolean ?? "true") !== "true") return [];
    value = value ? 1 : 0;
  } else if (sample.value_repr !== "number" || typeof value !== "number") {
    return [];
  }
  const scaled = value;

  // Change detection: when on_change is enabled, suppress readings whose scaled value is
  // unchanged since the last emitted one (per point id, per flow instance).
  if (String(cfg.on_change ?? "false") === "true") {
    const key = `last:${sample.point}`;
    const last = context.script.get(key);
    if (last !== undefined && last !== null && Math.abs(scaled - last) < 1e-9) return [];
    context.script.set(key, scaled);
  }

  // Derive the device from the source topic: te/device/<device>/ot/<protocol>/sample/<point>
  const parts = message.topic.split("/");
  const device = parts[2] || "main";
  const { group } = resolveNaming(cfg, sample);
  const targetTopic = cfg.target_topic || `te/device/${device}///m/${group}`;
  const body = shapeBody(cfg, sample, scaled);

  // Combine mode: buffer each device's series and flush one merged measurement on interval.
  if (String(cfg.combine ?? "false") === "true") {
    const buffer = context.flow.get("buffer") || {};
    const merged = buffer[targetTopic] || {};
    deepMerge(merged, body);
    merged.time = sample.ts;
    buffer[targetTopic] = merged;
    context.flow.set("buffer", buffer);
    return [];
  }

  const payload = Object.assign({}, body, { time: sample.ts });
  return [{ topic: targetTopic, payload: JSON.stringify(payload) }];
}

// Flush the combine buffer: one merged measurement per target topic. A no-op unless combine is on.
export function onInterval(_time, context) {
  if (String(context.config?.combine ?? "false") !== "true") return [];
  const buffer = context.flow.get("buffer") || {};
  const out = [];
  for (const topic of Object.keys(buffer)) {
    out.push({ topic, payload: JSON.stringify(buffer[topic]) });
  }
  context.flow.set("buffer", {});
  return out;
}
