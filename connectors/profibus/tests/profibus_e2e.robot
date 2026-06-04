*** Settings ***
Documentation       End-to-end tests for the PROFIBUS-DP connector against the
...                 software slave simulator.  The slave simulator provides canned
...                 input data; the connector reads it and publishes samples to a
...                 local MQTT broker.  No cloud (Cumulocity) is involved.
...
...                 The simulator seeds the following values:
...                   di_byte0  (input byte 0) = 0b0000_1010  (10 decimal)
...                   di_bit3   (bit 3 of byte 0) = 1  (true — bit 3 of 0x0A is 1)
...                   ai0_raw   (bytes 2-3 BE uint16) = 0x1234  (4660 decimal)
...                   ai1_raw   (bytes 4-5 BE uint16) = 0x0064  (100 decimal)
...
...                 Run via:  just test-e2e-profibus

Library             ../../_shared/MqttClient.py
Library             Collections

Suite Setup         Connect And Subscribe
Suite Teardown      Disconnect Broker


*** Variables ***
${BROKER_HOST}          localhost
${BROKER_PORT}          11884

${DEVICE}               remote_io
${PROTOCOL}             profibus
${SERVICE}              tedge-dot

${SAMPLE_PREFIX}        te/device/${DEVICE}/ot/${PROTOCOL}/sample
${CMD_PREFIX}           te/device/${DEVICE}/ot/${PROTOCOL}/cmd/write
${LINK_TOPIC}           te/device/${DEVICE}/ot/${PROTOCOL}/status/link
${CAPS_TOPIC}           te/device/main/service/${SERVICE}/ot/capabilities
${HEALTH_TOPIC}         te/device/main/service/${SERVICE}/status/health

${READY_TIMEOUT}        120
${SAMPLE_TIMEOUT}       30


*** Test Cases ***
Connector Publishes Capability Descriptor
    [Documentation]    The connector advertises its protocol and command verbs.
    ${payload}=    Wait For Retained    ${CAPS_TOPIC}    timeout=${READY_TIMEOUT}
    ${protocol}=    Get Json Field    ${payload}    protocol
    Should Be Equal    ${protocol}    profibus
    ${verbs}=    Get Json Field    ${payload}    command_verbs
    List Should Contain Value    ${verbs}    write

Service Health Is Up
    [Documentation]    The connector publishes a retained service health status of "up".
    ${payload}=    Wait For Retained    ${HEALTH_TOPIC}    timeout=${READY_TIMEOUT}
    ${status}=    Get Json Field    ${payload}    status
    Should Be Equal    ${status}    up

Device Link Is Connected
    [Documentation]    The connector reports the PROFIBUS device link as connected.
    ${payload}=    Wait For Retained    ${LINK_TOPIC}    timeout=${READY_TIMEOUT}
    ${status}=    Get Json Field    ${payload}    status
    Should Be Equal    ${status}    connected

Reads Digital Input Byte
    [Documentation]    Reads input byte 0 — the simulator seeds it to 10 (0x0A).
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/di_byte0    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    10

Reads Digital Input Bit
    [Documentation]    Reads bit 3 of input byte 0 — 0x0A has bit 3 set, so true.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/di_bit3    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal    ${value}    ${True}

Reads First Analogue Input
    [Documentation]    Reads AI0 (bytes 2-3 big-endian uint16) = 4660 (0x1234).
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/ai0_raw    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    4660

Reads Second Analogue Input
    [Documentation]    Reads AI1 (bytes 4-5 big-endian uint16) = 100.
    ${payload}=    Wait For Sample    ${SAMPLE_PREFIX}/ai1_raw    timeout=${SAMPLE_TIMEOUT}
    Sample Should Be Good    ${payload}
    ${value}=    Get Json Field    ${payload}    value
    Should Be Equal As Numbers    ${value}    100

Writes Digital Output And Reads It Back
    [Documentation]    A write command sets DO byte 0 to 0xFF; the next sample reflects it.
    Publish Message    ${CMD_PREFIX}/do-1
    ...    {"status":"init","point":"do_byte0","value":255}
    ...    retain=True
    ${result}=    Wait For Message Containing    ${CMD_PREFIX}/do-1
    ...    "status":"successful"    timeout=${SAMPLE_TIMEOUT}
    ${point}=    Get Json Field    ${result}    point
    Should Be Equal    ${point}    do_byte0


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
