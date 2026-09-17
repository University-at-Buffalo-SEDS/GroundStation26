import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("users_tool", Path(__file__).resolve().parents[1] / "backend/tools/users_config_gui.py")
users = importlib.util.module_from_spec(spec)
spec.loader.exec_module(users)


class ClockPermissionTests(unittest.TestCase):
    def test_default_off_and_editor_preserves_or_revokes_explicitly(self):
        self.assertFalse(users.normalize_permissions({"send_commands": True})["set_system_time"])
        cfg = {"users": []}
        kwargs = dict(password="test-only", view_data=True, send_commands=False,
                      calibration_view=False, calibration_edit=False, disabled=False,
                      allowed_commands=[])
        users.upsert_user(cfg, "operator", set_system_time=True, **kwargs)
        self.assertTrue(cfg["users"][0]["permissions"]["set_system_time"])
        kwargs["password"] = None
        users.upsert_user(cfg, "operator", **kwargs)
        self.assertTrue(cfg["users"][0]["permissions"]["set_system_time"])
        users.upsert_user(cfg, "operator", set_system_time=False, **kwargs)
        self.assertFalse(cfg["users"][0]["permissions"]["set_system_time"])


if __name__ == "__main__":
    unittest.main()
