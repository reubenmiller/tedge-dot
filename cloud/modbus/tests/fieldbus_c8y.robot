*** Settings ***
Documentation       Cloud Fieldbus round-trip (RFC 0002 increment 2): create a modbus device
...                 type in the tenant, assign it to a child device the way the Cloud Fieldbus
...                 UI does (placeholder child MO + c8y_ModbusDevice operation), and verify the
...                 device translates it into connector TOML and produces the type's
...                 measurements.
...
...                 The assignment translation is implemented by the c8y-fieldbus-import shim
...                 (operations/c8y-fieldbus-import, executed via the c8y_ModbusDevice custom
...                 operation template — see the RFC 0002 status update in
...                 doc/rfc/0002-cloud-fieldbus-integration.md), which creates the child's
...                 external identity, fetches the device type via the mapper's c8y proxy, and
...                 drives the connector's define-device verb.
...
...                 Requires C8Y_BASEURL / C8Y_USER / C8Y_PASSWORD / C8Y_TENANT and DEVICE_ID,
...                 plus a running stack (see `just test-cloud modbus`).

Library             Cumulocity
Library             Collections
Library             ../../_shared/FieldbusLibrary.py

Suite Setup         Setup Gateway Context


*** Variables ***
${DEVICE_ID}            %{DEVICE_ID=}
${TYPE_NAME}            tedge-dot-sim-type
${FB_CHILD}             fieldbus1
${OP_TIMEOUT}           60
${MEAS_TIMEOUT}         90

# Cloud Fieldbus register definition matching the pymodbus simulator:
# holding register 3 = 17001 (uint16), scaled /1000 -> 17.001 as modbus/temperature.
${REGISTERS}            [{"number": 3, "startBit": 0, "noBits": 16, "signed": false, "multiplier": 1, "divisor": 1000, "offset": 0, "input": false, "name": "temperature", "unit": "°C", "measurementMapping": {"type": "modbus", "series": "temperature"}}]


*** Test Cases ***
Create Fieldbus Device Type
    [Documentation]    A c8y_ModbusDeviceType managed object with register definitions can be
    ...                created and read back (exercises the inventory CRUD keywords).
    ${type}=    Create Modbus Device Type    ${TYPE_NAME}    registers=${REGISTERS}
    Set Suite Variable    ${TYPE_MO}    ${type}
    ${fetched}=    FieldbusLibrary.Get Managed Object    ${type}[id]
    Should Be Equal    ${fetched}[type]    c8y_ModbusDeviceType
    ${regs}=    Get From Dictionary    ${fetched}    c8y_Registers
    Length Should Be    ${regs}    1
    Should Be Equal As Integers    ${regs}[0][number]    3

Assign Device Type Sends Cloud Fieldbus Operation
    [Documentation]    Assigning the type to a child creates the UI-shaped c8y_ModbusDevice
    ...                operation against the gateway.
    ${gateway}=    Set Managed Object    ${DEVICE_ID}
    ${result}=    Assign Modbus Device Type To Child
    ...    ${gateway}[id]    ${FB_CHILD}    ${TYPE_MO}[id]
    ...    address=1    ip_address=simulator    protocol=TCP
    Set Suite Variable    ${ASSIGN_OP}    ${result}[operation]
    Set Suite Variable    ${FB_CHILD_MO}    ${result}[child]
    Dictionary Should Contain Key    ${ASSIGN_OP}    c8y_ModbusDevice

Assignment Is Translated Into Connector Config
    [Documentation]    The c8y-fieldbus-import shim converts the c8y_ModbusDevice operation
    ...                into a define-device management command; the operation only turns
    ...                SUCCESSFUL once the point is persisted into the connector TOML.
    Operation Should Be SUCCESSFUL    ${ASSIGN_OP}[id]    timeout=${OP_TIMEOUT}
    ${output}=    Execute Shell Command    cat /etc/tedge/plugins/modbus/modbus.toml
    Should Contain    ${output}    ${FB_CHILD}
    Should Contain    ${output}    temperature

Child External Identity Links The UI-Created Managed Object
    [Documentation]    The shim registers <gateway id>:device:<child name> (type c8y_Serial)
    ...                against the placeholder MO the UI created, so the thin-edge child
    ...                registration adopts it instead of creating a duplicate.
    ${mo}=    External Identity Should Exist    ${DEVICE_ID}:device:${FB_CHILD}
    Should Be Equal As Strings    ${mo}[id]    ${FB_CHILD_MO}[id]

Imported Points Produce Mapped Measurements
    [Documentation]    Samples from the imported points surface as the measurement type/series
    ...                declared in the device type's measurementMapping (carried through the
    ...                point's meta.measurement, honoured by ot-measurement).
    Set Device    ${DEVICE_ID}:device:${FB_CHILD}
    Device Should Have Measurements
    ...    minimum=1    type=modbus    timeout=${MEAS_TIMEOUT}


*** Keywords ***
Setup Gateway Context
    Set Managed Object    ${DEVICE_ID}
