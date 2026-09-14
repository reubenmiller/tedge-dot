// ot-event: raise thin-edge.io events from OT signals.
//
// Direction: OT protocol format / thin-edge.io data model -> thin-edge.io data model.
//   in:  te/device/<device>/ot/<protocol>/sample/<point>  (samples: events declared per point)
//        te/device/<device>///m/<group>                   (measurements: the `series` param)
//   out: te/device/<device>///e/<event_type>              (thin-edge event)
//
// Per-signal events (from samples): the connector echoes the point's free-form `meta` table in
// every sample, so an event is declared next to the signal's address, for whatever the sample
// carries — a number, a boolean or a string:
//
//   meta = { event = { type = "firmware_changed", text = "Firmware changed to {value}" } }
//
//   type   event type, a single topic level; default "<point>_event"
//   text   {point}, {value}, {unit} and {device} are filled in; default "{point} changed to
//          {value}", or "{point} is {value}" with `when`
//   when   without it, an event is raised every time the value changes. With it, an event is
//          raised each time the condition starts to hold, not while it keeps holding. The keys
//          are an alarm's (see ot-alarm): equals, not_equals, above, below, hysteresis.
//
// `event` may also be a list of such tables. An entry the flow cannot use — a type that is not a
// topic level, a `when` with no condition it knows — is skipped. Only good samples are evaluated.
//
// The first reading of a signal after the flow starts is its baseline and raises nothing: the
// flow keeps no memory across a mapper restart, so it cannot tell a change from a restart, and
// raising on every restart would report changes that never happened. A change made while the
// mapper was down is therefore not reported.
//
// Measurement events (from measurements): the flow params watch ONE series and raise an event on
// every change of it, its first value included — the original mode, kept for existing
// deployments. It is off while `series` is empty, which is what lets the flow ship active:
// without params it only acts on declarations.
//
// State (context.script):
//   "event:<device>:<point>:<type>"  -> { value } last value, or { holds } with `when`
//   "<event topic>:last"             -> last value of the measurement series

const decoder = new TextDecoder();

// A value usable as one topic level.
function isTopicLevel(s) {
  return typeof s === "string" && /^[^/+#]+$/.test(s);
}

function listOf(v) {
  if (v === undefined || v === null) return [];
  return Array.isArray(v) ? v : [v];
}

// A stored condition state — what it held by, true (held, for a reason not known), false — or
// undefined when unknown.
function knownState(v) {
  return typeof v === "string" || typeof v === "boolean" ? v : undefined;
}

// Parse a `when` table into a condition, or null when it names no condition this flow knows.
// Kept identical in ot-alarm/main.js: flows cannot share modules.
function conditionOf(when) {
  if (!when || typeof when !== "object" || Array.isArray(when)) return null;
  const c = { hysteresis: 0 };
  if (when.equals !== undefined) c.equals = listOf(when.equals);
  if (when.not_equals !== undefined) c.not_equals = listOf(when.not_equals);
  if (typeof when.above === "number" && isFinite(when.above)) c.above = when.above;
  if (typeof when.below === "number" && isFinite(when.below)) c.below = when.below;
  if (typeof when.hysteresis === "number" && when.hysteresis > 0) c.hysteresis = when.hysteresis;
  const usable = c.equals || c.not_equals || c.above !== undefined || c.below !== undefined;
  return usable ? c : null;
}

// Whether condition `c` holds for `value`, and by what: "equals", "not_equals", "above" or "below"
// when it holds, false when it does not, undefined when the value is inside a hysteresis band and
// the previous state is unknown. `was` is the previous result, or true when the condition held
// for a reason not known. A band only keeps the condition holding when its own limit (or an
// unknown reason) raised it: a value recovering from below the low limit is not held by the high
// limit's band.
// Kept identical in ot-alarm/main.js: flows cannot share modules.
function evaluate(c, value, was) {
  const listed = (list) => list.some((v) => v === value);
  if (c.equals && listed(c.equals)) return "equals";
  if (c.not_equals && !listed(c.not_equals)) return "not_equals";
  let unknown = false;
  if (typeof value === "number" && isFinite(value)) {
    const limit = (side, beyond, inBand) => {
      if (beyond) return side;
      if (inBand && (was === side || was === true)) return side;
      if (inBand && was === undefined) unknown = true;
      return false;
    };
    if (c.above !== undefined) {
      const held = limit("above", value > c.above, value > c.above - c.hysteresis);
      if (held) return held;
    }
    if (c.below !== undefined) {
      const held = limit("below", value < c.below, value < c.below + c.hysteresis);
      if (held) return held;
    }
  }
  return unknown ? undefined : false;
}

function render(text, sample, point, device) {
  const vars = {
    point,
    value: typeof sample.value === "string" ? sample.value : JSON.stringify(sample.value),
    unit: typeof sample.unit === "string" ? sample.unit : "",
    device,
  };
  return text.replace(/\{(point|value|unit|device)\}/g, (_m, key) => vars[key]);
}

// The usable events a sample declares, deduplicated by type (the first declaration wins).
function eventsOf(sample, point) {
  const out = [];
  for (const entry of listOf(sample.meta?.event)) {
    if (!entry || typeof entry !== "object" || Array.isArray(entry)) continue;
    const type = entry.type === undefined ? `${point}_event` : entry.type;
    const when = entry.when === undefined ? null : conditionOf(entry.when);
    if (!isTopicLevel(type) || (entry.when !== undefined && !when)) continue;
    if (out.some((e) => e.type === type)) continue;
    const text =
      typeof entry.text === "string"
        ? entry.text
        : when
          ? "{point} is {value}"
          : "{point} changed to {value}";
    out.push({ type, text, when });
  }
  return out;
}

function onSample(parts, sample, context) {
  if (sample.quality !== "good" || sample.value === undefined) return [];
  const device = parts[2];
  const point = typeof sample.point === "string" && sample.point ? sample.point : parts[6];
  const out = [];
  for (const event of eventsOf(sample, point)) {
    const key = `event:${device}:${point}:${event.type}`;
    const seen = context.script.get(key);
    let raise;
    if (event.when) {
      const was = knownState(seen?.holds);
      const holds = evaluate(event.when, sample.value, was);
      if (holds === undefined) continue; // inside a hysteresis band with no baseline yet
      raise = was === false && Boolean(holds);
      context.script.set(key, { holds });
    } else {
      const known = !!seen && typeof seen === "object" && Object.prototype.hasOwnProperty.call(seen, "value");
      raise = known && JSON.stringify(seen.value) !== JSON.stringify(sample.value);
      context.script.set(key, { value: sample.value });
    }
    if (!raise) continue;
    out.push({
      topic: `te/device/${device}///e/${event.type}`,
      payload: JSON.stringify({ text: render(event.text, sample, point, device), time: sample.ts }),
    });
  }
  return out;
}

function onMeasurement(message, payload, context) {
  const cfg = context.config || {};
  // Off unless a series is configured: the flow ships active, and only declarations drive it then.
  const series = cfg.series || "";
  if (!series) return [];

  // Group defaults to the m/<group> segment of the source topic, so this flow follows whatever
  // protocol produced the measurement.
  const group = cfg.group || message.topic.split("/")[6] || "value";
  const eventType = cfg.event_type || "ot_event";
  const text = cfg.text || "OT value changed";

  // Extract the series value, tolerating both { series: v } and { series: { value: v } }.
  const node = payload?.[group]?.[series];
  const value = typeof node === "object" && node !== null ? node.value : node;
  if (value === undefined || value === null) return [];

  // Event topic derived from the device prefix of the incoming measurement topic.
  // e.g. "te/device/plc1///m/modbus" -> "te/device/plc1///e/<event_type>"
  const devicePrefix = message.topic.split("/").slice(0, 5).join("/");
  const eventTopic = `${devicePrefix}/e/${eventType}`;

  // Emit only when the value changed since the last seen one (per device+type).
  const key = `${eventTopic}:last`;
  const last = context.script.get(key);
  if (last !== undefined && last !== null && last === value) return [];
  context.script.set(key, value);

  return [{
    topic: eventTopic,
    payload: JSON.stringify({
      text,
      time: payload.time,
    }),
  }];
}

export function onMessage(message, context) {
  let payload;
  try {
    payload = JSON.parse(decoder.decode(message.payload));
  } catch (_e) {
    return [];
  }
  if (!payload || typeof payload !== "object") return [];
  const parts = message.topic.split("/");
  if (parts[3] === "ot" && parts[5] === "sample") return onSample(parts, payload, context);
  if (parts[5] === "m") return onMeasurement(message, payload, context);
  return [];
}
