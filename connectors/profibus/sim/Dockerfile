# PROFIBUS-DP slave simulator container.
#
# The slave's serial line is exposed as a TCP byte stream on port 9200; the
# simulator listens directly (slave_sim.py --port tcp-listen://9200), serving
# one client at a time and surviving client reconnects.
#
# Earlier revisions bridged a socat pty pair to TCP instead; the bridge's
# listener wedged after the first client disconnected (socat only listens
# while its first address is open), so every session after the first hung
# until the container was restarted.
#
# The connector speaks serial-over-TCP natively (port = "tcp://<host>:9200"),
# so nothing else is needed on the client side.

FROM python:3.11-slim

COPY connectors/profibus/sim/requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt

COPY connectors/profibus/sim/slave_sim.py /slave_sim.py

EXPOSE 9200

CMD ["python3", "/slave_sim.py", \
     "--port", "tcp-listen://9200", \
     "--address", "7", "--baudrate", "19200", \
     "--input-bytes", "8", "--output-bytes", "4"]
