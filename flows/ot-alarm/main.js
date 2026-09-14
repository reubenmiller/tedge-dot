// ot-alarm: raise and clear thin-edge.io alarms from OT signals.
//
// Direction: OT protocol format / thin-edge.io data model -> thin-edge.io data model.
//   in:  te/device/<device>/ot/<protocol>/sample/<point>  (samples: alarms declared per point)
//        te/device/<device>/ot/<protocol>/status/link     (retained: the configured points)
//        te/device/<device>///m/<group>                   (measurements: the `series` param)
//   out: te/device/<device>///a/<alarm_type>              (retained: the alarm, or "" to clear)
//
// Per-signal alarms (from samples): the connector echoes the point's free-form `meta` table in
// every sample, so an alarm is declared next to the signal's address. It works on whatever the
// sample carries — a number, a boolean or a string — whether or not the signal is also a
// measurement (meta.measurement = false):
//
//   meta = { alarm = { type = "pump_fault", severity = "critical", when = { equals = "FAULT" } } }
//
//   type        alarm type, a single topic level; default "<point>_alarm". Unique per device:
//               when two points declare one type, the first to declare it keeps it, and the
//               other declaration is ignored until the first point stops declaring it.
//   severity    "critical" | "major" | "minor" | "warning"; default "major"
//   text        {point}, {value}, {unit} and {device} are filled in; default "{point} is {value}"
//   when        what the alarm is raised for; it stands while ANY of these holds:
//                 equals      the value is this value, or one of this list of values
//                 not_equals  the value is neither this value nor any of this list
//                 above       the value is a number greater than this
//                 below       the value is a number less than this
//               hysteresis   with above/below: once a limit raised the alarm, it only clears at
//                            that limit moved back by this much (<= above - hysteresis, >= below
//                            + hysteresis), so a value hovering at the limit does not flap it
//               Without `when`, the alarm stands while the value is `true` (a coil, a flag).
//
// `alarm` may also be a list of such tables. An entry the flow cannot use — a type that is not a
// topic level, an unknown severity, a `when` with no condition it knows — is skipped. Only good
// samples are evaluated: a failed read changes no alarm.
//
// Measurement alarms (from measurements): the flow params watch ONE series with a threshold and
// hysteresis — the original mode, kept for existing deployments. It is off while `series` is
// empty, which is what lets the flow ship active: without params it only acts on declarations.
//
// Alarms are retained, so they outlive this flow's in-memory state. After a mapper restart an
// alarm's state is first taken from its retained message, which the companion flow
// alarm-state.js records: an alarm still standing is not raised again (Cumulocity counts every
// repeat of an active alarm as a new occurrence). An alarm with no retained message is UNKNOWN —
// a cleared alarm leaves none — and the first reading settles it by publishing it, raised OR
// cleared, so an alarm whose condition went away while the mapper was down is cleared rather than
// left standing; a cleared alarm thus gets one more, harmless, clear after a restart. After that
// only changes are published. A reading inside a hysteresis band settles nothing: it is
// consistent with both. (A sample handled before the broker's retained alarm raises the alarm
// again, as if the companion flow were not there.)
//
// A declared alarm is also cleared when its declaration goes away: when its point's sample no
// longer declares it (the meta changed) or the link status no longer lists the point (it was
// removed from the configuration). That relies on the flow having seen the declaration since it
// started: a declaration removed while the mapper was down, or a whole device removed (no link
// status of it is published any more), leaves its alarm standing.
//
// State:
//   context.script "alarms:<device>"        -> { <type>: { point, protocol, active } } per
//                                              declared alarm; active is what the condition held
//                                              by ("equals", "above", ...), false, or absent
//                                              while unknown
//   context.script "<alarm topic>:active"   -> true / false for the measurement alarm
//   context.mapper "ot-alarm-retained:<alarm topic>" -> true / false, from alarm-state.js

const decoder = new TextDecoder();

const SEVERITIES = ["critical", "major", "minor", "warning"];

// A value usable as one topic level.
function isTopicLevel(s) {
  return typeof s === "string" && /^[^/+#]+$/.test(s);
}

function listOf(v) {
  if (v === undefined || v === null) return [];
  return Array.isArray(v) ? v : [v];
}

// A stored true/false, or undefined when unknown (a missing key reads back as undefined or null
// depending on the runtime).
function knownBool(v) {
  return typeof v === "boolean" ? v : undefined;
}

// A stored condition state — what it held by, true (held, for a reason not known), false — or
// undefined when unknown.
function knownState(v) {
  return typeof v === "string" || typeof v === "boolean" ? v : undefined;
}

// Whether the retained alarm on `topic` is standing, as alarm-state.js saw it: undefined when it
// has seen nothing for the topic.
function retainedState(context, topic) {
  return knownBool(context.mapper.get(`ot-alarm-retained:${topic}`));
}

// Parse a `when` table into a condition, or null when it names no condition this flow knows.
// Kept identical in ot-event/main.js: flows cannot share modules.
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
// Kept identical in ot-event/main.js: flows cannot share modules.
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

// The usable alarms a sample declares, deduplicated by type (the first declaration wins).
function alarmsOf(sample, point) {
  const out = [];
  for (const entry of listOf(sample.meta?.alarm)) {
    if (!entry || typeof entry !== "object" || Array.isArray(entry)) continue;
    const type = entry.type === undefined ? `${point}_alarm` : entry.type;
    const severity = entry.severity === undefined ? "major" : entry.severity;
    const when = entry.when === undefined ? { equals: [true], hysteresis: 0 } : conditionOf(entry.when);
    if (!isTopicLevel(type) || !SEVERITIES.includes(severity) || !when) continue;
    if (out.some((a) => a.type === type)) continue;
    const text = typeof entry.text === "string" ? entry.text : "{point} is {value}";
    out.push({ type, severity, text, when });
  }
  return out;
}

function clearMessage(topic) {
  return { topic, payload: "", mqtt: { retain: true, qos: 1 } };
}

function onSample(parts, sample, context) {
  const device = parts[2];
  const protocol = parts[4];
  const point = typeof sample.point === "string" && sample.point ? sample.point : parts[6];
  const alarmTopic = (type) => `te/device/${device}///a/${type}`;
  const key = `alarms:${device}`;
  const state = context.script.get(key) || {};
  const alarms = alarmsOf(sample, point);
  const owns = (known) => known.point === point && known.protocol === protocol;
  // Most points declare no alarm: leave the state alone for them.
  if (!alarms.length && !Object.values(state).some(owns)) return [];
  const out = [];

  // A sample is the authority on which alarms its point declares now, so one it declared before
  // and no longer does is cleared. Only a sample that describes the point, though: one without
  // `access` or `meta` (from a connector outside the SDKs, which always echo `access`) says
  // nothing about its declarations.
  if (sample.access !== undefined || sample.meta !== undefined) {
    for (const [type, known] of Object.entries(state)) {
      if (!owns(known) || alarms.some((a) => a.type === type)) continue;
      if (knownState(known.active) !== false) out.push(clearMessage(alarmTopic(type)));
      delete state[type];
    }
  }

  const readable = sample.quality === "good" && sample.value !== undefined;
  for (const alarm of alarms) {
    const topic = alarmTopic(alarm.type);
    // Another point declared this type on the device first: it keeps the alarm.
    if (state[alarm.type] && !owns(state[alarm.type])) continue;
    let was = knownState(state[alarm.type]?.active);
    if (was === undefined) was = retainedState(context, topic);
    let active = was;
    if (readable) {
      const holds = evaluate(alarm.when, sample.value, was);
      if (holds !== undefined) {
        // Published on a change, and whenever the state was unknown: this reading settles it.
        if (was === undefined || Boolean(holds) !== Boolean(was)) {
          out.push(
            holds
              ? {
                  topic,
                  payload: JSON.stringify({
                    severity: alarm.severity,
                    text: render(alarm.text, sample, point, device),
                    time: sample.ts,
                  }),
                  mqtt: { retain: true, qos: 1 },
                }
              : clearMessage(topic)
          );
        }
        active = holds;
      }
    }
    state[alarm.type] = active === undefined ? { point, protocol } : { point, protocol, active };
  }

  context.script.set(key, state);
  return out;
}

// The link status lists the device's configured points (contract §8) and is republished on every
// configuration change, so an alarm declared by a point missing from it belongs to a removed
// point. A status without a list is from a connector that does not report one: nothing is known.
function onLinkStatus(parts, status, context) {
  if (!Array.isArray(status.points)) return [];
  const device = parts[2];
  const protocol = parts[4];
  const listed = new Set(status.points.filter((p) => typeof p === "string"));
  const key = `alarms:${device}`;
  const state = context.script.get(key) || {};
  const out = [];
  for (const [type, known] of Object.entries(state)) {
    if (known.protocol !== protocol || listed.has(known.point)) continue;
    if (knownState(known.active) !== false) out.push(clearMessage(`te/device/${device}///a/${type}`));
    delete state[type];
  }
  context.script.set(key, state);
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
  const threshold = Number(cfg.threshold ?? 70);
  const hysteresis = Number(cfg.hysteresis ?? 5);
  const severity = cfg.severity || "major";
  const alarmType = cfg.alarm_type || "ot_alarm";
  const text = cfg.text || "Value exceeded threshold";

  // Extract the series value, tolerating both { series: v } and { series: { value: v } }.
  const node = payload?.[group]?.[series];
  const value = typeof node === "object" && node !== null ? node.value : node;
  if (typeof value !== "number") return [];

  // Alarm topic derived from the device prefix of the incoming measurement topic.
  // e.g. "te/device/plc1///m/modbus" -> "te/device/plc1///a/<alarm_type>"
  const devicePrefix = message.topic.split("/").slice(0, 5).join("/");
  const alarmTopic = `${devicePrefix}/a/${alarmType}`;

  const clearBelow = threshold - hysteresis;
  const key = `${alarmTopic}:active`;
  // Unknown until the retained alarm or the first reading after a (re)start settles it, see the
  // header.
  let active = knownBool(context.script.get(key));
  if (active === undefined) active = retainedState(context, alarmTopic);

  if (value >= threshold) {
    context.script.set(key, true);
    if (active === true) return []; // already raised; no redundant publish
    return [{
      topic: alarmTopic,
      payload: JSON.stringify({
        severity,
        text: `${text} (${value} >= ${threshold})`,
        time: payload.time,
      }),
      mqtt: { retain: true, qos: 1 },
    }];
  }

  if (value < clearBelow) {
    context.script.set(key, false);
    if (active === false) return []; // already clear
    return [clearMessage(alarmTopic)];
  }

  // Inside the hysteresis band: no state change.
  return [];
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
  if (parts[3] === "ot" && parts[5] === "status") return onLinkStatus(parts, payload, context);
  if (parts[5] === "m") return onMeasurement(message, payload, context);
  return [];
}
