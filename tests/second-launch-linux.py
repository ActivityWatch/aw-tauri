#!/usr/bin/env python3
"""Replay an older primary (no warning callback) on an isolated Linux bus.

Requires python3-dbus, python3-gi, dbus-run-session and Xvfb. Build aw-tauri,
then run:
    xvfb-run -a dbus-run-session -- /usr/bin/python3 tests/second-launch-linux.py /absolute/path/to/aw-tauri

Never run against the live desktop bus. The test supplies an isolated HOME and
fake primary/notification services; no real ActivityWatch server is started.
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib


def main():
    binary = Path(sys.argv[1]).resolve(strict=True)
    DBusGMainLoop(set_as_default=True)
    bus = dbus.SessionBus()
    primary_name = dbus.service.BusName(
        "net.activitywatch.app.SingleInstance", bus, do_not_queue=True
    )
    notification_name = dbus.service.BusName(
        "org.freedesktop.Notifications", bus, do_not_queue=True
    )
    seen = []

    class Primary(dbus.service.Object):
        @dbus.service.method(
            "org.SingleInstance.DBus", in_signature="ass", out_signature=""
        )
        def ExecuteCallback(self, argv, cwd):
            seen.append({"callback": list(argv)})

    class Notifications(dbus.service.Object):
        @dbus.service.method(
            "org.freedesktop.Notifications",
            in_signature="susssasa{sv}i",
            out_signature="u",
        )
        def Notify(self, app, replaces, icon, summary, body, actions, hints, timeout):
            seen.append({"summary": str(summary), "body": str(body)})
            return 1

    # Keep service objects/names alive until the child exits.
    primary = Primary(primary_name, "/net/activitywatch/app/SingleInstance")
    notification = Notifications(notification_name, "/org/freedesktop/Notifications")
    with tempfile.TemporaryDirectory(prefix="aw-second-launch-") as home:
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("BOB_", "CASCADE_", "CONTEXT_HOLDOUT_"))
        }
        env.update(
            HOME=home,
            XDG_CONFIG_HOME=home + "/.config",
            XDG_DATA_HOME=home + "/.local/share",
            XDG_CACHE_HOME=home + "/.cache",
            APPIMAGE=home + "/new-aw.AppImage",
            AW_PROFILE="default",
        )
        # A file, rather than a pipe, cannot fill and deadlock the child.
        with tempfile.TemporaryFile(mode="w+") as output:
            proc = subprocess.Popen(
                [str(binary)], env=env, stdout=output, stderr=subprocess.STDOUT
            )
            loop = GLib.MainLoop()

            def poll():
                if proc.poll() is not None:
                    loop.quit()
                    return False
                return True

            def timeout():
                proc.kill()
                loop.quit()
                return False

            GLib.timeout_add(100, poll)
            GLib.timeout_add_seconds(20, timeout)
            try:
                loop.run()
                proc.wait(timeout=5)
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait()
            output.seek(0)
            logs = output.read()
        assert proc.returncode == 0, logs
        assert any("callback" in item for item in seen), (seen, logs)
        bodies = [item["body"] for item in seen if "body" in item]
        assert len(bodies) == 1, (seen, logs)
        assert "Requested launch: " + env["APPIMAGE"] in bodies[0], seen
        assert "Running from:" not in bodies[0], seen
        print(json.dumps(seen, indent=2))
        print("PASS: new process warns, forwards to old primary, exits successfully")
    del primary, notification


if __name__ == "__main__":
    main()
