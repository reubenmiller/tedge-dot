"""Minimal MQTT keyword library for the Modbus connector e2e suite.

Subscribes to the broker and records messages so tests can assert on the raw
samples, retained status messages, and command results the connector publishes.
"""

import json
import re
import threading
import time

import paho.mqtt.client as mqtt
from robot.api import logger
from robot.api.deco import keyword, library
from robot.utils import timestr_to_secs


@library(scope="SUITE")
class MqttClient:
    """Robot keyword library wrapping a paho-mqtt subscriber/publisher."""

    def __init__(self):
        self._client = None
        self._lock = threading.Lock()
        # topic -> list of (recv_time, payload)
        self._messages = {}

    # -- connection management ------------------------------------------------

    @keyword
    def connect_broker(self, host="localhost", port=1883):
        """Connect to the MQTT broker and start the network loop."""
        try:
            client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)
        except AttributeError:  # paho-mqtt < 2.0 fallback
            client = mqtt.Client()
        client.on_message = self._on_message
        client.connect(host, int(port), keepalive=30)
        client.loop_start()
        self._client = client
        logger.info(f"connected to mqtt broker {host}:{port}")

    @keyword
    def disconnect_broker(self):
        """Stop the network loop and disconnect."""
        if self._client is not None:
            self._client.loop_stop()
            self._client.disconnect()
            self._client = None

    @keyword
    def subscribe(self, topic="#"):
        """Subscribe to a topic filter (default everything)."""
        self._client.subscribe(topic, qos=1)
        logger.info(f"subscribed to {topic}")

    def _on_message(self, _client, _userdata, msg):
        payload = msg.payload.decode("utf-8", errors="replace")
        with self._lock:
            self._messages.setdefault(msg.topic, []).append((time.time(), payload))

    # -- publishing -----------------------------------------------------------

    @keyword
    def publish_message(self, topic, payload, retain=False, qos=1):
        """Publish a message to the broker."""
        retain = str(retain).lower() in ("true", "1", "yes")
        info = self._client.publish(topic, payload, qos=int(qos), retain=retain)
        info.wait_for_publish(timeout=5)
        logger.info(f"published to {topic}: {payload}")

    # -- assertions / waits ---------------------------------------------------

    def _latest(self, topic):
        with self._lock:
            entries = self._messages.get(topic, [])
            return entries[-1] if entries else None

    @keyword
    def get_message(self, topic):
        """Return the latest payload seen on a topic, or fail if none."""
        entry = self._latest(topic)
        if entry is None:
            raise AssertionError(f"no message received on topic {topic}")
        return entry[1]

    @keyword
    def get_messages(self, topic):
        """Return every payload seen on an exact topic so far, oldest first.

        For asserting on a whole exchange rather than its latest state: a command topic is
        retained, so a second responder's transitions simply overwrite the first's, and only
        the full history shows that two parties answered one command.
        """
        with self._lock:
            return [payload for _recv_time, payload in self._messages.get(topic, [])]

    @keyword
    def wait_for_retained(self, topic, timeout=10):
        """Wait until any message has been seen on the topic; return its payload.

        Suitable for retained messages (capabilities, health, link) which the
        broker delivers right after subscribe.
        """
        deadline = time.time() + float(timeout)
        while time.time() < deadline:
            entry = self._latest(topic)
            if entry is not None:
                return entry[1]
            time.sleep(0.1)
        raise AssertionError(f"timed out waiting for a message on topic {topic}")

    @keyword
    def wait_for_sample(self, topic, timeout=10):
        """Wait for a message that arrives AFTER this call; return its payload.

        Suitable for polled, non-retained samples: guarantees a fresh reading.
        """
        start = time.time()
        deadline = start + float(timeout)
        while time.time() < deadline:
            with self._lock:
                entries = self._messages.get(topic, [])
                for recv_time, payload in reversed(entries):
                    if recv_time >= start:
                        return payload
            time.sleep(0.1)
        raise AssertionError(f"timed out waiting for a fresh sample on topic {topic}")

    @keyword
    def get_message_mark(self):
        """Return a mark (the current time) for `Wait For Fresh Message With Field`'s `since`."""
        return time.time()

    @keyword
    def wait_for_fresh_message_with_field(self, topic, field, *values, timeout=10, since=None):
        """Wait for a message arriving after `since` (a `Get Message Mark`, default: this call)
        whose JSON `field` (dotted path) is one of `values`; return its payload.

        For state published on change, such as a link status: the recorded history holds
        earlier transitions, which must not satisfy the wait. Take the mark BEFORE the action
        that causes the transition when that action can block past it (`docker stop` waits for
        the process to exit, and the connector may already have reacted by then).
        """
        start = float(since) if since is not None else time.time()
        deadline = time.time() + timestr_to_secs(timeout)
        while time.time() < deadline:
            with self._lock:
                fresh = [p for recv_time, p in self._messages.get(topic, []) if recv_time >= start]
            for payload in fresh:
                try:
                    if self.get_json_field(payload, field) in values:
                        return payload
                except (ValueError, KeyError, TypeError):
                    continue
            time.sleep(0.1)
        raise AssertionError(
            f"timed out waiting for a fresh message with {field} in {values} on topic {topic}"
        )

    @keyword
    def wait_for_message_containing(self, topic, substring, timeout=10):
        """Wait until a message on the topic contains the substring; return it."""
        deadline = time.time() + float(timeout)
        while time.time() < deadline:
            with self._lock:
                entries = list(self._messages.get(topic, []))
            for _recv_time, payload in reversed(entries):
                if substring in payload:
                    return payload
            time.sleep(0.1)
        raise AssertionError(
            f"timed out waiting for '{substring}' on topic {topic}"
        )

    @keyword
    def get_json_field(self, payload, field):
        """Return a field from a JSON payload (dotted path supported)."""
        data = json.loads(payload)
        for part in field.split("."):
            data = data[part]
        return data

    @keyword
    def no_messages_on_topic(self, pattern, timeout=3):
        """Assert nothing is received on a topic filter (MQTT + / # wildcards).

        Waits the full timeout, then fails if any recorded message — including ones
        received before the call — matches the filter. Suitable for topic-discipline
        tests ("the connector must never publish here").
        """
        time.sleep(float(timeout))
        regex = self._filter_to_regex(pattern)
        with self._lock:
            offending = sorted(t for t in self._messages if regex.match(t))
        if offending:
            raise AssertionError(
                f"expected no messages matching {pattern}, but saw: {', '.join(offending)}"
            )

    @keyword
    def no_new_messages_on_topic(self, topic, timeout=5):
        """Assert nothing NEW arrives on an exact topic during the timeout.

        Unlike `No Messages On Topic`, history is ignored: only messages received
        after this call count, so it can be used mid-suite without clearing the
        recorded traffic other tests rely on.

        This is what tells push delivery apart from polling. A subscription
        reports value CHANGES, so a point on a static node publishes once and
        then goes quiet; polling republishes it every interval whether or not
        anything changed. A rate check cannot make that distinction, because the
        runtime hands the connector the point's poll interval as the monitored
        item's sampling interval -- push and polling run at the same rate by
        design.
        """
        start = time.time()
        time.sleep(float(timeout))
        with self._lock:
            fresh = [
                payload
                for recv_time, payload in self._messages.get(topic, [])
                if recv_time >= start
            ]
        if fresh:
            raise AssertionError(
                f"expected no new messages on {topic} within {timeout}s, "
                f"but saw {len(fresh)}; first: {fresh[0]}"
            )

    @staticmethod
    def _filter_to_regex(pattern):
        """Convert an MQTT topic filter into an anchored regex.

        Follows MQTT matching rules: `+` is one level, a trailing `#` matches the
        parent level and everything below it.
        """
        parts = []
        multi = False
        for level in pattern.split("/"):
            if level == "#":
                multi = True
                break
            parts.append("[^/]*" if level == "+" else re.escape(level))
        regex = "/".join(parts)
        if multi:
            regex += "(/.*)?" if parts else ".*"
        return re.compile("^" + regex + "$")

    @keyword
    def clear_messages(self):
        """Forget all recorded messages."""
        with self._lock:
            self._messages.clear()
