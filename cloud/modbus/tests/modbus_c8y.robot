*** Settings ***
Documentation       Full end-to-end test for the Rust tedge-dot against Cumulocity.
...                 The tedge container installs the connector, its thin-edge flows and the
...                 Cumulocity operation shims (no Python plugin), reads a real Modbus simulator,
...                 and bridges everything to Cumulocity. These tests assert the cloud-facing
...                 behaviour: child registration, the device twin, measurements (get), and
...                 c8y_SetRegister / c8y_SetCoil operations (set).
...
...                 Requires C8Y_BASEURL / C8Y_USER / C8Y_PASSWORD / C8Y_TENANT and DEVICE_ID,
...                 plus a running stack (see just test-e2e-c8y).

Library             Cumulocity
Library             Collections

Suite Setup         Set Main Device


*** Variables ***
${DEVICE_ID}            %{DEVICE_ID=}
${CHILD_NAME}           plc1
${CHILD_EXTERNAL_ID}    ${DEVICE_ID}:device:${CHILD_NAME}

${MEAS_TIMEOUT}         60
${OP_TIMEOUT}           30


*** Test Cases ***
Connector Service Is Registered
    [Documentation]    The connector runs as a tedge service on the main device.
    Set Main Device
    Cumulocity.Should Have Services    name=tedge-dot    min_count=1    timeout=${MEAS_TIMEOUT}

Child Device Is Registered
    [Documentation]    The connector's device is auto-registered as a modbus child device.
    Cumulocity.Device Should Exist    ${CHILD_EXTERNAL_ID}

Child Device Has Modbus Twin Fragment
    [Documentation]    ot-registration publishes the connector descriptor as c8y_ModbusDevice.
    Cumulocity.Device Should Exist    ${CHILD_EXTERNAL_ID}
    ${mo}=    Managed Object Should Have Fragments    c8y_ModbusDevice
    Should Be Equal    ${mo}[c8y_ModbusDevice][protocol]    modbus
    Should Be Equal    ${mo}[c8y_ModbusDevice][transport]    tcp

Measurements Are Sent To Cumulocity
    [Documentation]    The uint16 holding register (17001) arrives as a modbus measurement.
    Cumulocity.Device Should Exist    ${CHILD_EXTERNAL_ID}
    Cumulocity.Device Should Have Measurements
    ...    minimum=1    type=modbus    fragment=modbus    series=temp_u16    timeout=${MEAS_TIMEOUT}

Set Register Operation Round-Trips
    [Documentation]    c8y_SetRegister writes 4242 to temp_u16; the next reading reflects it.
    # Cumulocity.Execute Shell Command    text=tedge mqtt pub te/device/plc1//cmd/
    Cumulocity.Device Should Exist    ${CHILD_EXTERNAL_ID}
    ${operation}=    Cumulocity.Create Operation
    ...    fragments={"c8y_SetRegister":{"point":"temp_u16","value":4242}}
    ...    description=Set temp_u16 to 4242
    Cumulocity.Operation Should Be SUCCESSFUL    ${operation}    timeout=${OP_TIMEOUT}
    Cumulocity.Device Should Have Measurements
    ...    minimum=1    type=modbus    fragment=modbus    series=temp_u16
    ...    timeout=${MEAS_TIMEOUT}
    Should Be Equal As Integers    ${measurements[0]["modbus"]["temp_u16"]["value"]}    4242

Set Coil Operation Round-Trips
    [Documentation]    c8y_SetCoil sets coil_rw true; the next reading reflects it.
    Cumulocity.Device Should Exist    ${CHILD_EXTERNAL_ID}
    ${operation}=    Cumulocity.Create Operation
    ...    fragments={"c8y_SetCoil":{"point":"coil_rw","value":true}}
    ...    description=Set coil_rw true
    Cumulocity.Operation Should Be SUCCESSFUL    ${operation}    timeout=${OP_TIMEOUT}
    Sleep    1s
    ${measurements}    Cumulocity.Device Should Have Measurements
    ...    minimum=1    type=modbus    fragment=modbus    series=coil_rw
    ...    timeout=${MEAS_TIMEOUT}
    Should Be Equal As Numbers    ${measurements[0]["modbus"]["coil_rw"]["value"]}    1.0    precision=1


*** Keywords ***
Set Main Device
    Cumulocity.Set Device    ${DEVICE_ID}
