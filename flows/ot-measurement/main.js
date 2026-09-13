// ot-measurement: convert a connector "sample" envelope into a thin-edge.io measurement.
//
// Direction: OT protocol format  ->  thin-edge.io data model.
//   in:  te/device/<device>/ot/<protocol>/sample/<point>   (connector sample envelope)
//   out: te/device/<device>///m/<group>                    (thin-edge measurement)
//
// Protocol-neutral: it consumes the generic OT Connector Contract sample, so the same flow maps
// modbus, opcua or any other connector. The connector is the "dumb" driver plus the per-point
// engineering transform (multiplier/divisor/decimal_shift/offset is declared on the point and
// applied by the connector via the SDK), so the sample already carries the scaled value.
//
// PER-SIGNAL PUBLISH POLICY IS NOT THIS FLOW'S JOB ANY MORE (contract §5.4, RFC 0006 §5.1).
// A point's `publish` table — on_change, deadband, min_interval, debounce — is applied by the
// connector runtime to the sample stream itself, so every consumer (this flow, the parameter
// twin, a historian) sees one stream with the policy already applied and none of them carries
// the per-signal lookup. What remains here are the FLOW-WIDE params of the same names, which
// apply to every signal this instance sees: a second, coarser filter for a deployment that
// wants one, and the only filter for points that declare no `publish` of their own.
//
// What this flow still owns: naming (group/series), the measurement opt-out, boolean handling,
// and optional batching of a device's series into one measurement (combine).
//
// Naming comes from the device's retained manifest (te/device/<device>/ot/<protocol>/manifest,
// contract §8.2), which this flow keeps in context.mapper under "ot-manifest:<device>": a
// point's typed `measurement` field names its group and series (e.g. written by the Cloud
// Fieldbus import from a device type's measurementMapping), and `measurement = false` keeps the
// signal out of the measurements altogether (e.g. a parameter whose value should reach the
// cloud only through its twin fragment). A sample carries none of this — it is a time series
// row (contract §5) — and a signal whose manifest has not been seen yet falls back to the
// flow-wide params.

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

// Resolve a flow-wide boolean param.
function boolSetting(cfgValue, dflt) {
  if (cfgValue === undefined || cfgValue === null || cfgValue === "") return dflt;
  return String(cfgValue) === "true";
}

// Resolve a flow-wide numeric param.
function numSetting(cfgValue, dflt) {
  const n = typeof cfgValue === "number" ? cfgValue : parseFloat(cfgValue);
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

// The point's typed `measurement` field (contract §5.5), from the device manifest: `false`,
// a { group, series } table, or undefined when the manifest has not been seen yet (or does not
// list the point) — which makes the flow-wide naming apply, the same fallback as a point that
// declares no `measurement` at all.
function measurementOf(context, device, point) {
  const manifest = context.mapper.get(`ot-manifest:${device}`);
  return manifest?.points?.[point]?.measurement;
}

// Shape the measurement body (without the time field) from a scaled value:
//   { <group>: { <series>: value } }
// Series values are BARE numbers on purpose: the tedge c8y mapper's measurement converter
// silently drops any object-shaped series value ({ value } and { value, unit } alike), so
// embedding the unit here would strand the measurement on the device. The unit remains
// available to consumers on the device manifest.
function shapeBody(cfg, sample, measurement, scaled) {
  const { group, series } = resolveNaming(cfg, sample, measurement);
  return { [group]: { [series]: scaled } };
}

// Resolve the measurement group + series for a sample. Precedence:
//   1. the point's typed `measurement.group` / `.series` (from the device manifest — e.g.
//      written by the Cloud Fieldbus import from a device type's measurementMapping.type /
//      series): per-signal wins over the flow params,
//   2. explicit cfg.group / cfg.series (flow-wide overrides),
//   3. point-id convention: when point_separator is set and the point id contains it, the id is
//      split once into "<group><sep><series>" (e.g. "." maps "Environment.Temperature" ->
//      group "Environment", series "Temperature"). This lets ONE flow instance remap many
//      signals just by how their point ids are named on the connector.
//   4. defaults: group = sample protocol, series = point id.
function resolveNaming(cfg, sample, measurement) {
  const mm =
    (measurement &&
      typeof measurement === "object" &&
      !Array.isArray(measurement) &&
      measurement) ||
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

// True when the signal opted out of measurements: `measurement = false`, a boolean. Same shape
// as the `parameter = false` opt-out (ot-parameter-state), and like it a string is not a switch.
function measurementDisabled(measurement) {
  return measurement === false;
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
  const measurement = measurementOf(context, device, sample.point);

  // Optionally restrict this flow instance to a single point id.
  const point = cfg.point || "";
  if (point && sample.point !== point) return [];

  // Opt-out: `measurement = false` keeps the signal out of the measurements entirely —
  // typically a parameter (contract §5.2) whose value belongs on its twin fragment only, rather
  // than also being sent to the cloud as a time series. Checked before any change-detection or
  // combine state is touched. The connector still publishes the sample, so ot-parameter-state
  // (and every other flow) sees it as usual.
  if (measurementDisabled(measurement)) return [];

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

  // Flow-wide policy only. The per-signal one (`publish`, contract §5.4) has already been
  // applied by the connector to the stream this flow is reading, so a signal that declares its
  // own policy arrives pre-filtered and these settings act on top of it.
  const ts = Date.parse(sample.ts);
  const now = isFinite(ts) ? ts : Date.now();
  const debounceMs = durationMs(cfg.debounce);
  const deadband = numSetting(cfg.deadband, 0);
  const onChange = boolSetting(cfg.on_change, false) || deadband > 0 || debounceMs > 0;
  const minIntervalMs = durationMs(cfg.min_interval);

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

  const { group } = resolveNaming(cfg, sample, measurement);
  const targetTopic = cfg.target_topic || `te/device/${device}///m/${group}`;
  const body = shapeBody(cfg, sample, measurement, scaled);

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
