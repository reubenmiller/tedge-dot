*** Settings ***
Documentation       Device parameters round-trip (RFC 0003): the connector's writable points are
...                 declared in the tenant as Digital Twin Manager property definitions, show up on
...                 the child device as a twin fragment kept current by the ot-parameter-state
...                 flow, and an edit from the Cumulocity "Parameters" tab (a c8y_ParameterUpdate
...                 operation) is written to the PLC through one connector write-batch, completes
...                 the operation, and is reflected in the fragment, a c8y_ParameterUpdate event
...                 and the next measurement.
...
...                 Requires C8Y_BASEURL / C8Y_USER / C8Y_PASSWORD / C8Y_TENANT and DEVICE_ID, a
...                 tenant with the dtm + device-parameter microservices, and a running stack
...                 (see `just test-cloud modbus`).

Resource            ../../_shared/device.resource
Library             Collections
Library             ../../_shared/ParameterLibrary.py

Suite Setup         Setup Child Context
Suite Teardown      Teardown Cloud Device


*** Variables ***
${CHILD_NAME}           plc1
# ${CHILD_EXTERNAL_ID} is built in the suite setup: it embeds the per-run device id.
# The parameter set is named after the device *type* the config declares (§5.2), not after the
# protocol: a DTM identifier is tenant-wide, and two Modbus device types must not share one.
${SET}                  modbus_plc_sim_control_parameters
${OP_TIMEOUT}           60
${MEAS_TIMEOUT}         90
${NEW_VALUE}            4343


*** Test Cases ***
Parameter Definitions Are Rendered From The Device Manifest
    [Documentation]    `tedge-dot manifest --format c8y-dtm` renders one DTM property definition
    ...                per parameter set, with the writable points as properties (an admin
    ...                registers it once). It renders them from the device MANIFESTS the
    ...                connector publishes, not from the configuration file: the same document,
    ...                the same sets, whatever produced it.
    ${output}=    Execute Shell Command And Get Output
    ...    tedge-dot manifest -c /etc/tedge/plugins/ot/modbus.toml --format c8y-dtm    timeout=${OP_TIMEOUT}
    # The first JSON line, not the first line: a warning on stderr (§5.2) can be interleaved.
    ${definition}=    Evaluate    json.loads([l for l in $output.splitlines() if l.startswith("{")][0])    modules=json
    Should Be Equal    ${definition}[identifier]    ${SET}
    Dictionary Should Contain Key    ${definition}[jsonSchema][properties]    temp_u16
    Dictionary Should Contain Key    ${definition}[jsonSchema][properties]    coil_rw
    Dictionary Should Not Contain Key    ${definition}[jsonSchema][properties]    level_f32
    Should Be Equal    ${definition}[jsonSchema][properties][temp_u16][title]    Temperature setpoint
    Set Suite Variable    ${DEFINITION}    ${definition}

Parameter Definition Is Registered In The Tenant
    [Documentation]    The rendered definition is posted to the DTM service — the tenant admin's
    ...                one-off step (the device never calls the DTM service). An existing
    ...                definition is reconciled rather than trusted: one left over from an
    ...                earlier connector config (a renamed or removed point) is re-created, so
    ...                the Parameters tab never shows stale properties.
    Ensure DTM Property Definition    ${DEFINITION}
    DTM Property Definitions Should Contain    ${SET}
    DTM Property Definition Should Match    ${DEFINITION}

Child Device Carries The Parameter Set Fragment
    [Documentation]    ot-parameter-state publishes the set as a twin fragment; the c8y mapper
    ...                mirrors it into the child's managed object where the Parameters tab reads it.
    Cumulocity.Device Should Exist    ${CHILD_EXTERNAL_ID}
    ${mo}=    Managed Object Should Have Fragments    ${SET}    timeout=${MEAS_TIMEOUT}
    Dictionary Should Contain Key    ${mo}[${SET}]    temp_u16
    Dictionary Should Contain Key    ${mo}[${SET}]    coil_rw

Parameter Update Operation Writes The Points
    [Documentation]    The operation the Parameters tab sends is executed as one connector
    ...                write-batch and completes SUCCESSFUL.
    Cumulocity.Device Should Exist    ${CHILD_EXTERNAL_ID}
    ${operation}=    Cumulocity.Create Operation
    ...    fragments={"c8y_ParameterUpdate":{},"c8y_ParameterUpdate_${SET}":{},"${SET}":{"temp_u16":${NEW_VALUE},"coil_rw":true}}
    ...    description=Update ${SET}
    Cumulocity.Operation Should Be SUCCESSFUL    ${operation}    timeout=${OP_TIMEOUT}

Parameter Fragment Reflects The Update
    Cumulocity.Device Should Exist    ${CHILD_EXTERNAL_ID}
    Cumulocity.Managed Object Should Have Fragment Values    ${SET}.temp_u16\=${NEW_VALUE}    ${SET}.coil_rw\=true    timeout=${MEAS_TIMEOUT}

Measurements Confirm The Written Value
    Cumulocity.Device Should Exist    ${CHILD_EXTERNAL_ID}
    ${measurements}=    Cumulocity.Device Should Have Measurements
    ...    minimum=1    type=modbus    value=modbus    series=temp_u16
    ...    sort_newest=${True}    timeout=${MEAS_TIMEOUT}
    Should Be Equal As Integers    ${measurements[0]["modbus"]["temp_u16"]["value"]}    ${NEW_VALUE}

Parameter Update With An Unknown Key Fails
    Cumulocity.Device Should Exist    ${CHILD_EXTERNAL_ID}
    ${operation}=    Cumulocity.Create Operation
    ...    fragments={"c8y_ParameterUpdate":{},"c8y_ParameterUpdate_${SET}":{},"${SET}":{"bogus":1}}
    ...    description=Update ${SET} with an unknown key
    ${op}=    Cumulocity.Operation Should Be FAILED    ${operation}    timeout=${OP_TIMEOUT}
    Should Contain    ${op}[failureReason]    bogus


*** Keywords ***
Setup Child Context
    Setup Cloud Device
    Set Suite Variable    $CHILD_EXTERNAL_ID    ${DEVICE_ID}:device:${CHILD_NAME}
    Cumulocity.Set Device    ${DEVICE_ID}
