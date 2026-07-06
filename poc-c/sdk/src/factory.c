#include "tedge_dot/connector.h"

#include <string.h>

tdot_connector_t *tdot_connector_factory(const char *protocol) {
#ifdef TDOT_FEATURE_MODBUS
    if (strcmp(protocol, "modbus") == 0)
        return tdot_connector_modbus_new();
#endif
#ifdef TDOT_FEATURE_OPCUA
    if (strcmp(protocol, "opcua") == 0)
        return tdot_connector_opcua_new();
#endif
#ifdef TDOT_FEATURE_CANBUS
    if (strcmp(protocol, "canbus") == 0)
        return tdot_connector_canbus_new();
#endif
#ifdef TDOT_FEATURE_CANOPEN
    if (strcmp(protocol, "canopen") == 0)
        return tdot_connector_canopen_new();
#endif
#ifdef TDOT_FEATURE_J1939
    if (strcmp(protocol, "j1939") == 0)
        return tdot_connector_j1939_new();
#endif
#ifdef TDOT_FEATURE_PROFIBUS
    if (strcmp(protocol, "profibus") == 0)
        return tdot_connector_profibus_new();
#endif
    return NULL;
}
