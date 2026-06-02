# OPC-UA simulator (python-asyncua) for the e2e harness.
FROM python:3.12-slim
WORKDIR /app
COPY e2e-opcua/simulator/requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY e2e-opcua/simulator/server.py .
EXPOSE 4840
CMD ["python", "server.py"]
