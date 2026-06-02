// ot-alarm: raise/clear a thin-edge.io alarm from a measurement, with hysteresis.
//
// Direction: thin-edge.io data model -> thin-edge.io data model.
//   in:  te/device/<device>///m/<group>     (measurement produced by ot-measurement)
//   out: te/device/<device>///a/<alarm_type> (thin-edge alarm)
//
// Protocol-neutral: it runs on the standard measurement, so the same flow works for any OT
// connector (modbus, opcua, ...) once the value has been shaped into a measurement.

const decoder = new TextDecoder();

export function onMessage(message, context) {
  const payload = JSON.parse(decoder.decode(message.payload));
  const cfg = context.config || {};

  // Group defaults to the m/<group> segment of the source topic, so this flow follows whatever
  // protocol produced the measurement.
  const group = cfg.group || message.topic.split("/")[6] || "value";
  const series = cfg.series || "value";
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
  const active = context.script.get(key) ?? false;

  if (value >= threshold) {
    if (active) return []; // already raised; no redundant publish
    context.script.set(key, true);
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
    if (!active) return []; // already clear
    context.script.set(key, false);
    return [{
      topic: alarmTopic,
      payload: "",
      mqtt: { retain: true, qos: 1 },
    }];
  }

  // Inside the hysteresis band: no state change.
  return [];
}
