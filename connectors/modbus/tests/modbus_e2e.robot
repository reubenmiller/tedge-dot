*** Settings ***
Documentation       End-to-end tests for the Rust tedge-dot against a real Modbus
...                 simulator (pymodbus). The connector reads the simulator and publishes raw
...                 samples + status to a local MQTT broker; these tests assert on that output.
...                 No cloud (Cumulocity) is involved.
...
...                 Run via:  just test-e2e   (brings the Docker stack up/down automatically)

Library             ../../_shared/MqttClient.py
Library             Collections

Suite Setup         Connect And Subscribe
Suite Teardown      Disconnect Broker


*** Variables ***
${BROKER_HOST}          localhost
${BROKER_PORT}          11883

${DEVICE}               plc1
${PROTOCOL}             modbus
${SERVICE}              tedge-dot

${SAMPLE_PREFIX}        te/device/${DEVICE}/ot/${PROTOCOL}/sample
${CMD_PREFIX}           te/device/${DEVICE}/ot/${PROTOCOL}/cmd/write
${LINK_TOPIC}           te/device/${DEVICE}/ot/${PROTOCOL}/status/link
${CAPS_TOPIC}           te/device/main/service/${SERVICE}/ot/capabilities
${HEALTH_TOPIC}         te/device/main/service/${SERVICE}/status/health

# Generous timeout: the connector waits for the simulator/broker before it starts.
${READY_TIMEOUT}        90
${SAMPLE_TIMEOUT}       15


*** Test Cases ***
Connector Publishes Capability Descriptor
    [Documentation]    The connector advertises its protocol and supported command verbs.
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${protocol}=    Get Json Field    ${payload}    protocol
    Should Be Equal    ${protocol}    modbus
    ${verbs}=    Get Json Field    ${payload}    command_verbs
    List Should Contain Value    ${verbs}    write

Service Health Is Up
    [Documentation]    The connector publishes a retained service health status of "up".
    ${payload}=    Wait For Retained    ${HEALTH_TOPIC}    timeout=${READY_TIMEOUT}
    ${status}=    Get Json Field    ${payload}    status
    Should Be Equal    ${status}    up

Device Link Is Connected
    [Documentation]    The connector reports the Modbus device link as connected.
    ${payload}=    Wait For Retained    ${LINK_TOPIC}    timeout=${READY_TIMEOUT}
    ${status}=    Get Json Field    ${payload}    status
    Should Be Equal    ${status}    connected

Reads Uint16 Holding Register
    [Documentation]    Reads a uint16 holding register seeded to 17001 in the simulator.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${datatype}=    Get Json Field    ${payload}    datatype
    Should Be Equal    ${datatype}    uint16
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    17001

Reads Uint32 Across Two Registers
    [Documentation]    Reads a uint32 value (617001) spanning two registers.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/count_u32    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    617001

Reads Float32 Across Two Registers
    [Documentation]    Reads a float32 value (~404.17) spanning two registers.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/level_f32    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${value}=    Get Json Field    ${payload}    value
    Should Be True    abs(${value} - 404.17) < 0.05

Invalid Register Reports Bad Quality
    [Documentation]    Reading a flagged-invalid address yields a bad-quality sample with an error.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/bad_point    timeout=${SAMPLE_TIMEOUT}
    ${quality}=    Get Json Field    ${payload}    quality
    Should Be Equal    ${quality}    bad
    ${error}=    Get Json Field    ${payload}    error
    Should Not Be Empty    ${error}

Writes A Coil And Reads It Back
    [Documentation]    A write command sets coil 48 true; the next sample reflects it.
    Publish Message    ${CMD_PREFIX}/coil-1    {"status":"init","point":"coil_rw","value":true}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/coil-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${point}=    Get Json Field    ${result}    point
    Should Be Equal    ${point}    coil_rw
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/coil_rw    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal    ${value}    ${True}

Writes A Holding Register And Reads It Back
    [Documentation]    A write command sets holding register 3 to 4242; the next sample reflects it.
    ...                 Runs after the uint16 read assertion (the stack is recreated per run).
    Publish Message    ${CMD_PREFIX}/reg-1    {"status":"init","point":"temp_u16","value":4242}    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/reg-1    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${point}=    Get Json Field    ${result}    point
    Should Be Equal    ${point}    temp_u16
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/temp_u16    timeout=${SAMPLE_TIMEOUT}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    4242


*** Keywords ***
Connect And Subscribe
    Connect Broker    ${BROKER_HOST}    ${BROKER_PORT}
    Subscribe    te/#

Sample Should Be Good
    [Arguments]    ${payload}
    ${quality}=    Get Json Field    ${payload}    quality
    Should Be Equal    ${quality}    good
    ${mode}=    Get Json Field    ${payload}    mode
    Should Be Equal    ${mode}    typed
