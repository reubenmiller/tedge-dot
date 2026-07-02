"""Cumulocity Cloud Fieldbus keywords for the tedge-dot cloud e2e suites.

Creates and manages the inventory entries the Cloud Fieldbus UI would create:
device-type managed objects (c8y_ModbusDeviceType with c8y_Registers/c8y_Coils),
UI-style child device placeholders, and the c8y_ModbusDevice assignment operation.

The generic inventory CRUD half of this library (Create/Update/Get Managed Object,
Add Child Device Reference) is proposed for upstreaming to robotframework-c8y
(branch feat/inventory-crud-keywords); once released there, this library shrinks
to the fieldbus-specific conveniences.

Auth comes from the same environment variables as robotframework-c8y
(C8Y_BASEURL, C8Y_USER, C8Y_PASSWORD, C8Y_TENANT), via c8y_test_core.
"""

import json
import logging
import time
from typing import Any, Dict, List, Optional, Union

from c8y_api.model import ManagedObject
from c8y_test_core.c8y import CustomCumulocityApp
from dotenv import load_dotenv
from robot.api.deco import keyword, library

logger = logging.getLogger(__name__)

Fragments = Union[str, Dict[str, Any], None]


def _to_dict(fragments: Fragments) -> Dict[str, Any]:
    if fragments is None:
        return {}
    if isinstance(fragments, str):
        return json.loads(fragments)
    return dict(fragments)


@library(scope="SUITE", auto_keywords=False)
class FieldbusLibrary:
    """Robot keywords for Cumulocity Cloud Fieldbus inventory entries."""

    ROBOT_LISTENER_API_VERSION = 3

    def __init__(self):
        load_dotenv()
        try:
            self.c8y = CustomCumulocityApp()
        except Exception as ex:  # allow --dryrun / import without tenant credentials
            logger.warning("Could not load Cumulocity API client: %s", ex)
            self.c8y = None
        self._cleanup: List[Any] = []
        # pylint: disable=invalid-name
        self.ROBOT_LIBRARY_LISTENER = self

    def end_suite(self, _data: Any, _result: Any):
        for func in reversed(self._cleanup):
            try:
                func()
            except Exception as ex:  # cleanup is best effort
                logger.warning("cleanup failed: %s", ex)
        self._cleanup.clear()

    # ── Generic inventory CRUD (upstream candidates) ────────────────────────

    @keyword("Create Managed Object")
    def create_managed_object(
        self, fragments: Fragments = None, cleanup: bool = True, **kwargs
    ) -> Dict[str, Any]:
        """Create a managed object from arbitrary fragments; returns its json."""
        body = _to_dict(fragments)
        body.update(kwargs)
        mo = ManagedObject.from_json(body)
        mo.c8y = self.c8y
        created = mo.create()
        if cleanup:
            self._cleanup.append(created.delete)
        data = created.to_json()
        data["id"] = created.id
        return data

    @keyword("Update Managed Object")
    def update_managed_object(self, mo_id: str, fragments: Fragments) -> Dict[str, Any]:
        """Merge the given fragments into an existing managed object."""
        return self.c8y.put(
            f"/inventory/managedObjects/{mo_id}",
            json=_to_dict(fragments),
            accept="application/vnd.com.nsn.cumulocity.managedobject+json",
        )

    @keyword("Get Managed Object")
    def get_managed_object(self, mo_id: str) -> Dict[str, Any]:
        """Fetch a managed object by id."""
        mo = self.c8y.inventory.get(mo_id)
        data = mo.to_json()
        data["id"] = mo.id
        return data

    @keyword("Delete Managed Object By Id")
    def delete_managed_object_by_id(self, mo_id: str):
        """Delete a managed object by id."""
        self.c8y.inventory.delete(mo_id)

    @keyword("Add Child Device Reference")
    def add_child_device_reference(self, parent_id: str, child_id: str):
        """Attach an existing managed object as child device of a parent."""
        self.c8y.post(
            f"/inventory/managedObjects/{parent_id}/childDevices",
            json={"managedObject": {"id": str(child_id)}},
        )

    # ── Cloud Fieldbus conveniences ──────────────────────────────────────────

    @keyword("Create Modbus Device Type")
    def create_modbus_device_type(
        self,
        name: str,
        registers: Fragments = None,
        coils: Fragments = None,
        protocol: str = "TCP",
        cleanup: bool = True,
    ) -> Dict[str, Any]:
        """Create a Cloud Fieldbus modbus device type managed object.

        `registers`/`coils` take the Cloud Fieldbus definition shape, e.g.::

            [{"number": 3, "startBit": 0, "noBits": 16, "signed": false,
              "multiplier": 1, "divisor": 1, "offset": 0, "input": false,
              "name": "temperature", "unit": "°C",
              "measurementMapping": {"type": "modbus", "series": "temperature"}}]
        """
        regs = json.loads(registers) if isinstance(registers, str) else registers
        cls = json.loads(coils) if isinstance(coils, str) else coils
        body = {
            "name": name,
            "type": "c8y_ModbusDeviceType",
            "c8y_IsDeviceType": {},
            "c8y_ModbusDeviceType": {"protocol": protocol},
            "c8y_Registers": regs or [],
            "c8y_Coils": cls or [],
        }
        return self.create_managed_object(body, cleanup=cleanup)

    @keyword("Create Fieldbus Child")
    def create_fieldbus_child(
        self, gateway_mo_id: str, child_name: str, cleanup: bool = True
    ) -> Dict[str, Any]:
        """Create the UI-style child placeholder MO and reference it under the gateway.

        Mirrors what the Cloud Fieldbus UI does before it sends the c8y_ModbusDevice
        assignment operation. Idempotent: an existing placeholder with the same name is
        reused, so repeated suite runs keep one child (and its external identity) stable.
        Returns the child managed object json.
        """
        existing = self.c8y.get(
            "/inventory/managedObjects",
            params={
                "query": f"$filter=(name eq '{child_name}' and type eq 'c8y_ModbusDevice')",
                "pageSize": "1",
            },
        ).get("managedObjects", [])
        if existing:
            return existing[0]
        child = self.create_managed_object(
            {"name": child_name, "type": "c8y_ModbusDevice", "c8y_IsDevice": {}},
            cleanup=cleanup,
        )
        self.add_child_device_reference(gateway_mo_id, child["id"])
        return child

    @keyword("Build Modbus Device Assignment")
    def build_modbus_device_assignment(
        self,
        child_mo_id: str,
        child_name: str,
        type_mo_id: str,
        address: int = 1,
        ip_address: str = "127.0.0.1",
        protocol: str = "TCP",
    ) -> str:
        """Return the UI-shaped c8y_ModbusDevice operation fragment as a JSON string.

        Pass it to the Cumulocity library's `Create Operation` (against the gateway
        context) so the standard operation assertion keywords apply.
        """
        return json.dumps(
            {
                "c8y_ModbusDevice": {
                    "protocol": protocol,
                    "address": int(address),
                    "ipAddress": ip_address,
                    "id": str(child_mo_id),
                    "name": child_name,
                    "type": f"/inventory/managedObjects/{type_mo_id}",
                }
            }
        )

    @keyword("Remove Modbus Device Type")
    def remove_modbus_device_type(self, type_mo_id: str):
        """Delete a fieldbus device type managed object."""
        self.c8y.inventory.delete(type_mo_id)

    @keyword("Operation Should Eventually Be Successful")
    def operation_should_eventually_be_successful(
        self, operation_id: str, timeout: float = 60, interval: float = 2
    ) -> Dict[str, Any]:
        """Poll an operation until SUCCESSFUL; fail fast on FAILED.

        Plain REST polling — avoids the c8y_test_core retry stack, which currently
        raises a TypeError under Python 3.14 when an operation stays EXECUTING
        across retry attempts.
        """
        deadline = time.time() + float(timeout)
        status = "UNKNOWN"
        while time.time() < deadline:
            op = self.c8y.get(f"/devicecontrol/operations/{operation_id}")
            status = op.get("status", "UNKNOWN")
            if status == "SUCCESSFUL":
                return op
            if status == "FAILED":
                raise AssertionError(
                    f"operation {operation_id} FAILED: {op.get('failureReason', '')}"
                )
            time.sleep(float(interval))
        raise AssertionError(
            f"operation {operation_id} still {status} after {timeout}s"
        )
