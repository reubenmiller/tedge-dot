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
// Per-signal overrides: the connector publishes each point's free-form `meta` table once, on
// the device's retained manifest (te/device/<device>/ot/<protocol>/manifest, contract §8.2),
// which this flow keeps in context.mapper under "ot-manifest:<device>". When present,
// meta.on_change / meta.deadband / meta.min_interval / meta.debounce override the flow-wide
// params for that signal only — so one flow instance can serve a whole plant while individual
// signals opt into their own behaviour, declared next to the signal's address in the connector
// config. Naming works the same way: meta.measurement.group / meta.measurement.series name the
// signal's measurement per point (e.g. written by the Cloud Fieldbus import from a device type's
// measurementMapping), and meta.measurement = false keeps the signal out of the measurements
// altogether (e.g. a parameter whose value should reach the cloud only through its twin
// fragment). A sample carries none of this: it is a time series row (contract §5), and a
// signal whose manifest has not been seen yet falls back to the flow-wide params.

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

// Remember (or forget, on a clearing message) a device's manifest. Shared with the other OT
// flows through context.mapper: the key and content are the same wherever it is stored from.
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

// The point's `meta` table, from the device manifest. Empty when the manifest has not been
// seen yet (or does not list the point), which makes the flow-wide params apply — the same
// fallback as a point that declares no meta at all.
function metaOf(context, device, point) {
  const manifest = context.mapper.get(`ot-manifest:${device}`);
  const entry = manifest?.points?.[point];
  return entry && typeof entry.meta === "object" && entry.meta !== null ? entry.meta : {};
}

// Shape the measurement body (without the time field) from a scaled value:
//   { <group>: { <series>: value } }
// Series values are BARE numbers on purpose: the tedge c8y mapper's measurement converter
// silently drops any object-shaped series value ({ value } and { value, unit } alike), so
// embedding the unit here would strand the measurement on the device. The unit remains
// available to consumers on the device manifest.
function shapeBody(cfg, sample, meta, scaled) {
  const { group, series } = resolveNaming(cfg, sample, meta);
  return { [group]: { [series]: scaled } };
}

// Resolve the measurement group + series for a sample. Precedence:
//   1. per-signal meta.measurement.group / .series (the connector point's `meta` table, from
//      the device manifest — e.g. written by the Cloud Fieldbus import from a device type's
//      measurementMapping.type/series) — meta wins over the flow params, consistent with the
//      other per-signal meta overrides,
//   2. explicit cfg.group / cfg.series (flow-wide overrides),
//   3. point-id convention: when point_separator is set and the point id contains it, the id is
//      split once into "<group><sep><series>" (e.g. "." maps "Environment.Temperature" ->
//      group "Environment", series "Temperature"). This lets ONE flow instance remap many
//      signals just by how their point ids are named on the connector.
//   4. defaults: group = sample protocol, series = point id.
function resolveNaming(cfg, sample, meta) {
  const mm =
    (meta &&
      typeof meta.measurement === "object" &&
      !Array.isArray(meta.measurement) &&
      meta.measurement) ||
    {};
  let group = mm.group || cfg.group || "";
  let series = mm.series || cfg.series || "";
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

// True when the signal opted out of measurements: `meta.measurement = false`, a boolean. Same
// shape as the `meta.parameter = false` opt-out (ot-parameter-state), and like it a string is not
// a switch.
function measurementDisabled(meta) {
  return meta ? meta.measurement === false : false;
}

export function onMessage(message, context) {
  // Topic shapes: te/device/<device>/ot/<protocol>/sample/<point>
  //               te/device/<device>/ot/<protocol>/manifest
  const parts = message.topic.split("/");
  const device = parts[2] || "main";
  if (parts[5] === "manifest") {
    rememberManifest(context, device, message.payload);
    return [];
  }
  const sample = JSON.parse(decoder.decode(message.payload));
  const cfg = context.config || {};
  const meta = metaOf(context, device, sample.point);

  // Optionally restrict this flow instance to a single point id.
  const point = cfg.point || "";
  if (point && sample.point !== point) return [];

  // Opt-out: `meta.measurement = false` keeps the signal out of the measurements entirely —
  // typically a parameter (contract §5.2) whose value belongs on its twin fragment only, rather
  // than also being sent to the cloud as a time series. Checked before any change-detection or
  // combine state is touched. The connector still publishes the sample, so ot-parameter-state
  // (and every other flow) sees it as usual.
  if (measurementDisabled(meta)) return [];

  // Only forward good-quality readings.
  if (sample.quality !== "good") return [];

  // Resolve a numeric value. Booleans (coils) become 1/0 when include_boolean is enabled.
  // The connector has already applied the point's engineering transform, so the sample value
  // is the final scaled reading.
  let value = sample.value;
  // A 0.2 sample says what its value is by the JSON type alone (contract §5): `value_repr`
  // is gone, because `datatype` plus the JSON type carried the same information.
  if (typeof value === "boolean") {
    if (String(cfg.include_boolean ?? "true") !== "true") return [];
    value = value ? 1 : 0;
  } else if (typeof value !== "number") {
    return [];
  }
  const scaled = value;

  // Per-signal settings from the point's meta (the device manifest), falling back to the
  // flow-wide params.
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

  const { group } = resolveNaming(cfg, sample, meta);
  const targetTopic = cfg.target_topic || `te/device/${device}///m/${group}`;
  const body = shapeBody(cfg, sample, meta, scaled);

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
