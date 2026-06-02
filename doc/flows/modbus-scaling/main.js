// modbus-scaling: turn a connector "sample" envelope into a thin-edge measurement.
//
// Replaces the legacy mapper.py scaling formula and templatestring JSON shaping.
// The driver only decoded the primitive value; this flow applies the engineering
// transform and names the measurement.

const decoder = new TextDecoder();

export function onMessage(message, context) {
  const sample = JSON.parse(decoder.decode(message.payload));

  const {
    point = "",
    multiplier = 1,
    divisor = 1,
    offset = 0,
    target_topic = "te/device/main///m/modbus",
    group = "modbus",
    series = "value",
    unit = "",
  } = context.config;

  // Only handle the configured point, good-quality, numeric typed samples.
  if (point && sample.point !== point) return [];
  if (sample.quality !== "good") return [];
  if (sample.value_repr !== "number" || typeof sample.value !== "number") return [];

  // Linear transform: (value * multiplier / divisor) + offset
  const scaled =
    (sample.value * Number(multiplier)) / Number(divisor) + Number(offset);

  // Shape into thin-edge measurement JSON: { <group>: { <series>: value } }
  const payload = {
    [group]: { [series]: scaled },
    time: sample.ts,
  };
  if (unit) {
    payload[group][series] = { value: scaled, unit };
  }

  return [{ topic: target_topic, payload: JSON.stringify(payload) }];
}
