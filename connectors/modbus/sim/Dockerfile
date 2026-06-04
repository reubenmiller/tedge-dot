### 1. Get Linux
FROM python:3.11

### 2. Install pymodbus and aiohttp
COPY connectors/modbus/sim/requirements.txt /tmp/requirements.txt
RUN pip install -r /tmp/requirements.txt

### 3. Copy modbus server configuration
COPY connectors/modbus/sim/modbus.json /modbus.json

### 3. Start server
EXPOSE 502
EXPOSE 8080    
ADD connectors/modbus/sim/simulator.py /
CMD ["python3", "simulator.py"]