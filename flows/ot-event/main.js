// ot-event: raise a thin-edge.io event when a monitored measurement value changes.
//
// Direction: thin-edge.io data model -> thin-edge.io data model.
//   in:  te/device/<device>///m/<group>      (measurement produced by ot-measurement)
//   out: te/device/<device>///e/<event_type> (thin-edge event)
//
// Protocol-neutral: it runs on the standard measurement, so the same flow works for any OT
// connector (modbus, opcua, ...). Mirrors the legacy register/coil `eventmapping`, which raised
// an event each time the value changed.

const decoder = new TextDecoder();

export function onMessage(message, context) {
  const payload = JSON.parse(decoder.decode(message.payload));
  const cfg = context.config || {};

  // Group defaults to the m/<group> segment of the source topic, so this flow follows whatever
  // protocol produced the measurement.
  const group = cfg.group || message.topic.split("/")[6] || "value";
  const series = cfg.series || "value";
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
