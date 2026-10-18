#!/usr/bin/env python3
"""Narrow mutation tests: these must fail for actual breaking contracts, not cosmetic edits."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("compat", Path(__file__).with_name("check-compatibility.py"))
compat = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compat)


class Compatibility(unittest.TestCase):
    def setUp(self):
        self.old = {"commands": [{"name": "show", "args": [{"name": "id", "required": True, "kind": "positional", "value_type": "string"},
            {"name": "last", "required": False, "kind": "option", "value_type": "integer", "default": "50"}]}],
            "tools": [{"name": "show", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}}],
            "schema": {"message": {"fields": ["role", "content"]}}, "rows": [{"id": "str"}],
            "session": {"messages": [{"role": "=user", "content": []}]},
            "messages": [{"role": "=user", "content": []}], "exits": [0, 1, 2],
            "routes": {"GET /api/sessions": {"status": 200, "pointer": ""}}}

    def check(self, new, old=None, tag="v0.17.0", version="0.18.0"):
        compat.compare(old or self.old, new, tag, version)

    def test_additions_and_help_are_allowed(self):
        new = copy.deepcopy(self.old)
        new["commands"][0]["about"] = "new prose"
        new["commands"][0]["args"].append({"name": "optional", "required": False})
        new["tools"][0]["inputSchema"]["properties"]["optional"] = {"type": "string"}
        new["messages"][0]["new_field"] = "str"
        self.check(new)

    def test_removal_type_default_requiredness_and_routes_fail(self):
        mutations = [lambda n: n["commands"][0]["args"].pop(),
            lambda n: n["commands"][0]["args"][1].pop("default"),
            lambda n: n["commands"][0]["args"][1].update(default="60"),
            lambda n: n["commands"][0]["args"][1].update(value_type="string"),
            lambda n: n["commands"][0]["args"][1].update(required=True),
            lambda n: n["commands"][0]["args"].append({"name": "new", "required": True}),
            lambda n: n["tools"][0]["inputSchema"]["properties"]["id"].update(type="integer"),
            lambda n: n["tools"][0]["inputSchema"].update(required=["id", "new"]),
            lambda n: n["tools"].clear(), lambda n: n["messages"][0].pop("role"),
            lambda n: n["messages"][0].update(role="=assistant"),
            lambda n: n["rows"][0].pop("id"), lambda n: n["routes"].clear(),
            lambda n: n["routes"]["GET /api/sessions"].update(status=404),
            lambda n: n["exits"].remove(2)]
        for mutation in mutations:
            new = copy.deepcopy(self.old)
            mutation(new)
            with self.subTest(mutation=mutation), self.assertRaises(compat.Incompatible):
                self.check(new)

    def test_route_inventory_follows_real_patterns_and_catches_omissions(self):
        routes = compat.routes(Path(__file__).resolve().parents[1])
        self.assertIn("GET /api/session/{}/{}/messages", routes)
        self.assertIn("GET /api/session/{}/{}/export", routes)
        self.assertIn("POST /api/task/{}/claim", routes)
        self.assertIn("POST /api/task/{}/reopen", routes)
        # Every discovered route is collected and compared, not just a selected probe table.
        old = copy.deepcopy(self.old)
        old["routes"] = {r: {"status": 200, "pointer": ""} for r in routes}
        new = copy.deepcopy(old)
        new["routes"].pop(routes[-1])
        with self.assertRaises(compat.Incompatible): self.check(new, old)

    def test_g1_exception_is_exact_and_release_scoped(self):
        old = copy.deepcopy(self.old)
        tool = old["tools"][0]
        tool["name"] = "workflow"
        tool["inputSchema"]["properties"]["follow"] = {"type": "boolean"}
        new = copy.deepcopy(old)
        new["tools"][0]["inputSchema"]["properties"].pop("follow")
        self.check(new, old)
        for tag, version in [("v0.16.1", "0.18.0"), ("v0.17.0", "0.19.0")]:
            with self.assertRaises(compat.Incompatible): self.check(new, old, tag, version)
        new["tools"][0]["inputSchema"]["properties"].pop("id")
        with self.assertRaises(compat.Incompatible): self.check(new, old)


if __name__ == "__main__": unittest.main()
