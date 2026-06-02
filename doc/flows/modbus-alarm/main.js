// modbus-alarm: raise/clear a thin-edge alarm from a scaled measurement, with hysteresis.
//
// Replaces the legacy mapper.py alarm state machine. It runs on the standard measurement
// produced by modbus-scaling, so it is fully protocol-neutral: the same flow works for any
// OT connector once the value has been scaled into a measurement.

const decoder = new TextDecoder();

export function onMessage(message, context) {
  const payload = JSON.parse(decoder.decode(message.payload));

  const {
    group = "environment",
    series = "temperature",
    threshold = 70.0,
    hysteresis = 5.0,
    severity = "major",
    alarm_type = "overheat",
    text = "Value exceeded threshold",
  } = context.config;

  // Extract the series value, tolerating both { series: v } and { series: { value: v } }.
  const node = payload?.[group]?.[series];
  const value = typeof node === "object" && node !== null ? node.value : node;
  if (typeof value !== "number") return [];

  // Alarm topic derived from the device prefix of the incoming measurement topic.
  // e.g. "te/device/plc-1///m/environment" -> "te/device/plc-1///a/<alarm_type>"
  const devicePrefix = message.topic.split("/").slice(0, 5).join("/");
  const alarmTopic = `${devicePrefix}/a/${alarm_type}`;

  const clearBelow = Number(threshold) - Number(hysteresis);
  const key = `${alarmTopic}:active`;
  const active = context.script.get(key) ?? false;

  if (value >= Number(threshold)) {
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
