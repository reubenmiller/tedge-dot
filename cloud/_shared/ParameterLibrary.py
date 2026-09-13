"""Cumulocity device-parameter keywords for the tedge-dot cloud e2e suites.

Manages Digital Twin Manager (DTM) property definitions — the tenant-side declaration of
which parameter sets a device exposes. The definitions themselves are rendered by
`tedge-dot manifest --format c8y-dtm` so the suite posts exactly what a tenant admin would.

Auth comes from the same environment variables as robotframework-c8y
(C8Y_BASEURL, C8Y_USER, C8Y_PASSWORD, C8Y_TENANT), via c8y_test_core.
"""

import json
import logging
from typing import Any, Dict, List, Union

from c8y_test_core.c8y import CustomCumulocityApp
from dotenv import load_dotenv
from robot.api.deco import keyword, library
from robot.utils import is_truthy

logger = logging.getLogger(__name__)

DTM_PROPERTIES = "/service/dtm/definitions/properties"


@library(scope="SUITE", auto_keywords=False)
class ParameterLibrary:
    """Robot keywords for Cumulocity device parameters (DTM definitions)."""

    def __init__(self):
        load_dotenv()
        try:
            self.c8y = CustomCumulocityApp()
        except Exception as ex:  # allow --dryrun / import without tenant credentials
            logger.warning("Could not load Cumulocity API client: %s", ex)
            self.c8y = None

    @keyword("Ensure DTM Property Definition")
    def ensure_dtm_property_definition(
        self, definition: Union[str, Dict[str, Any]], force: bool = False
    ) -> Dict[str, Any]:
        """Make the tenant's DTM property definition match the given one (as rendered by
        `tedge-dot manifest --format c8y-dtm`).

        An identifier that already exists is NOT assumed to be correct: the stored schema is
        compared with the wanted one and re-created when it differs, so a definition left over
        from an earlier connector configuration (a renamed or removed point, changed limits)
        cannot silently keep stale properties in the Parameters tab. Pass force=True to
        re-create even when they match.
        """
        body = json.loads(definition) if isinstance(definition, str) else dict(definition)
        identifier = body["identifier"]
        # The per-identifier GET of the DTM service is context-scoped and does not resolve
        # a definition by identifier alone; the list endpoint is the reliable existence check.
        existing = self._find_definition(identifier)
        if existing is None:
            created = self.c8y.post(DTM_PROPERTIES, json=body)
            logger.info("DTM definition %s created", identifier)
            return created

        drift = self.definition_drift(existing, body)
        if not drift and not is_truthy(force):
            logger.info("DTM definition %s already matches", identifier)
            return existing

        reason = "; ".join(drift) if drift else "forced"
        logger.info("Re-creating DTM definition %s (%s)", identifier, reason)
        # Delete with the contexts the tenant has, which may be a superset of ours.
        contexts = ",".join(existing.get("contexts") or body.get("contexts") or [])
        self.c8y.delete(f"{DTM_PROPERTIES}/{identifier}?contexts={contexts}")
        created = self.c8y.post(DTM_PROPERTIES, json=body)
        logger.info("DTM definition %s re-created", identifier)
        return created

    @keyword("DTM Property Definition Should Match")
    def dtm_property_definition_should_match(
        self, definition: Union[str, Dict[str, Any]]
    ) -> Dict[str, Any]:
        """Assert the tenant's definition matches the given one (same properties, no stale
        ones, same titles/limits/contexts). Returns the stored definition."""
        body = json.loads(definition) if isinstance(definition, str) else dict(definition)
        identifier = body["identifier"]
        existing = self._find_definition(identifier)
        if existing is None:
            raise AssertionError(f"no DTM property definition named {identifier}")
        drift = self.definition_drift(existing, body)
        if drift:
            raise AssertionError(
                f"DTM definition {identifier} does not match the rendered one: "
                + "; ".join(drift)
            )
        return existing

    def definition_drift(
        self, existing: Dict[str, Any], wanted: Dict[str, Any]
    ) -> List[str]:
        """Differences between the stored definition and the wanted one, as readable strings.

        Only the parts we declare are compared: the DTM service adds fields of its own (for
        example `c8y_AvailableActions`, `creationTime`, and extra contexts), and flagging those
        would re-create the definition on every run.
        """
        drift: List[str] = []
        want_schema = wanted.get("jsonSchema") or {}
        have_schema = existing.get("jsonSchema") or {}
        want_props = want_schema.get("properties") or {}
        have_props = have_schema.get("properties") or {}

        for key in sorted(set(have_props) - set(want_props)):
            drift.append(f"stale property '{key}'")
        for key in sorted(set(want_props) - set(have_props)):
            drift.append(f"missing property '{key}'")
        for key in sorted(set(want_props) & set(have_props)):
            want_prop = want_props[key] or {}
            have_prop = have_props[key] or {}
            changed = {
                field: (have_prop.get(field), value)
                for field, value in want_prop.items()
                if have_prop.get(field) != value
            }
            if changed:
                detail = ", ".join(
                    f"{field}: tenant={have!r} wanted={want!r}"
                    for field, (have, want) in sorted(changed.items())
                )
                drift.append(f"property '{key}' differs ({detail})")

        for field in ("title", "type", "description"):
            if field in want_schema and want_schema[field] != have_schema.get(field):
                drift.append(
                    f"schema {field} differs "
                    f"(tenant={have_schema.get(field)!r} wanted={want_schema[field]!r})"
                )

        missing_contexts = set(wanted.get("contexts") or []) - set(
            existing.get("contexts") or []
        )
        if missing_contexts:
            drift.append(f"missing contexts {sorted(missing_contexts)}")
        return drift

    def _find_definition(self, identifier: str):
        """The stored definition with this identifier, or None. The list endpoint is used
        because the per-identifier GET of the DTM service is context-scoped and does not
        resolve a definition by identifier alone."""
        page = self.c8y.get(DTM_PROPERTIES, params={"pageSize": "2000"})
        for definition in page.get("definitions", []):
            if definition.get("identifier") == identifier:
                return definition
        return None

    @keyword("DTM Property Definitions Should Contain")
    def dtm_property_definitions_should_contain(self, identifier: str) -> Dict[str, Any]:
        """Assert a DTM property definition with the identifier is listed."""
        found = self._find_definition(identifier)
        if found is None:
            raise AssertionError(f"no DTM property definition named {identifier}")
        return found
