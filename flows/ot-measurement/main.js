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
// flow owns naming (group/series), units, change-detection (on_change/deadband), debounce,
// rate limiting (min_interval) and optional batching of a device's series into one
// measurement (combine).
//
// Per-signal overrides: the connector echoes the point's free-form `meta` table in every
// sample (sample.meta). When present, meta.on_change / meta.deadband / meta.min_interval /
// meta.debounce override the flow-wide params for that signal only — so one flow instance can
// serve a whole plant while individual signals opt into their own behaviour, declared next to
// the signal's address in the connector config.

const decoder = new TextDecoder();

// Parse a duration into milliseconds: "500ms", "2s", "5m", "1h" or a bare number (seconds).
// Invalid/empty -> 0 (disabled).
function durationMs(v) {
  if (v === undefined || v === null || v === "") return 0;
  if (typeof v === "number") return isFinite(v) && v > 0 ? v * 1000 : 0;
  const s = String(v).trim();
  const m = s.match(/^([0-9]*\.?[0-9]+)\s*(ms|s|m|h)?$/);
  if (!m) return 0;
  const n = parseFloat(m[1]);
  const scale = { ms: 1, s: 1000, m: 60000, h: 3600000 }[m[2] || "s"];
  return n * scale;
}

// Resolve a boolean setting: sample.meta value (real bool or string) wins over the flow param.
function boolSetting(metaValue, cfgValue, dflt) {
  const v = metaValue !== undefined ? metaValue : cfgValue;
  if (v === undefined || v === null || v === "") return dflt;
  return String(v) === "true";
}

// Resolve a numeric setting: sample.meta value wins over the flow param.
function numSetting(metaValue, cfgValue, dflt) {
  const v = metaValue !== undefined ? metaValue : cfgValue;
  const n = typeof v === "number" ? v : parseFloat(v);
  return isFinite(n) ? n : dflt;
}

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

  // Per-signal settings from the sample's meta (echoed from the connector point config),
  // falling back to the flow-wide params.
  const meta = sample.meta || {};
  const ts = Date.parse(sample.ts);
  const now = isFinite(ts) ? ts : Date.now();
  const debounceMs = durationMs(meta.debounce !== undefined ? meta.debounce : cfg.debounce);
  const deadband = numSetting(meta.deadband, cfg.deadband, 0);
  const onChange =
    boolSetting(meta.on_change, cfg.on_change, false) || deadband > 0 || debounceMs > 0;
  const minIntervalMs = durationMs(
    meta.min_interval !== undefined ? meta.min_interval : cfg.min_interval
  );

  // Debounce: a changed value is only accepted once it has been observed stable for the
  // debounce period (by sample timestamps). Message-driven, so acceptance happens on the
  // first sample seen after the quiet period; debounce implies on_change.
  if (debounceMs > 0) {
    const key = `debounce:${sample.point}`;
    const cand = context.script.get(key);
    if (cand && Math.abs(cand.v - scaled) < 1e-9) {
      if (now - cand.since < debounceMs) return []; // still settling
    } else {
      context.script.set(key, { v: scaled, since: now });
      return []; // new candidate: wait for it to prove stable
    }
  }

  // Change detection: suppress readings whose scaled value is unchanged (within the deadband)
  // since the last emitted one (per point id, per flow instance).
  if (onChange) {
    const last = context.script.get(`last:${sample.point}`);
    const minDelta = deadband > 0 ? deadband : 1e-9;
    if (last !== undefined && last !== null && Math.abs(scaled - last) < minDelta) return [];
  }

  // Rate limit: drop readings that arrive within min_interval of the last emitted one.
  if (minIntervalMs > 0) {
    const lastTs = context.script.get(`lastts:${sample.point}`);
    if (lastTs !== undefined && lastTs !== null && now - lastTs < minIntervalMs) return [];
  }

  context.script.set(`last:${sample.point}`, scaled);
  context.script.set(`lastts:${sample.point}`, now);

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
