# OPC-UA simulator (python-asyncua) for the e2e harness.
FROM python:3.12-slim
WORKDIR /app
COPY connectors/opcua/sim/requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY connectors/opcua/sim/server.py .
EXPOSE 4840
CMD ["python", "server.py"]
