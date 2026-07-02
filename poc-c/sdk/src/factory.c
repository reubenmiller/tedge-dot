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
    return NULL;
}
