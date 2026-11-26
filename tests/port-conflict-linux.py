#!/usr/bin/env python3
"""Check the real GUI port-conflict notification before exit on an isolated bus.

Requires python3-dbus, python3-gi, dbus-run-session and Xvfb. Build aw-tauri,
then run with /usr/bin/python3 tests/port-conflict-linux.py /path/to/aw-tauri.
The script starts its own dbus-run-session and Xvfb; it never uses the live
session bus. The fake loopback server and isolated child HOME leave user data alone.
"""

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib


def main():
    binary = Path(sys.argv[1]).resolve(strict=True)
    if os.environ.get("AW_PORT_CONFLICT_ISOLATED") != "1":
        env = dict(os.environ)
        env["AW_PORT_CONFLICT_ISOLATED"] = "1"
        env["AW_PORT_CONFLICT_PARENT_BUS"] = env.get("DBUS_SESSION_BUS_ADDRESS", "")
        return subprocess.call(
            [
                "dbus-run-session",
                "--",
                "xvfb-run",
                "-a",
                sys.executable,
                str(Path(__file__).resolve()),
                str(binary),
            ],
            env=env,
        )
    assert os.environ.get("DBUS_SESSION_BUS_ADDRESS"), "No isolated session bus"
    assert os.environ["DBUS_SESSION_BUS_ADDRESS"] != os.environ.get(
        "AW_PORT_CONFLICT_PARENT_BUS"
    ), "Refusing to use the parent session bus"

    requests = []

    class InfoServer(BaseHTTPRequestHandler):
        def do_GET(self):
            requests.append(self.path)
            if self.path != "/api/0/info":
                self.send_error(404)
                return
            body = json.dumps(
                {"hostname": "smoke", "version": "v0.14.0", "testing": False}
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, format, *args):
            pass

    DBusGMainLoop(set_as_default=True)
    bus = dbus.SessionBus()
    notification_name = dbus.service.BusName(
        "org.freedesktop.Notifications", bus, do_not_queue=True
    )
    seen = []
    proc = None

    class Notifications(dbus.service.Object):
        @dbus.service.method(
            "org.freedesktop.Notifications",
            in_signature="susssasa{sv}i",
            out_signature="u",
        )
        def Notify(self, app, replaces, icon, summary, body, actions, hints, timeout):
            seen.append(
                {
                    "summary": str(summary),
                    "body": str(body),
                    "before_exit": proc is not None and proc.poll() is None,
                }
            )
            return 1

    notification = Notifications(notification_name, "/org/freedesktop/Notifications")
    with ThreadingHTTPServer(("127.0.0.1", 0), InfoServer) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix="aw-port-conflict-") as home:
                env = {
                    key: os.environ[key]
                    for key in (
                        "PATH",
                        "DISPLAY",
                        "XAUTHORITY",
                        "DBUS_SESSION_BUS_ADDRESS",
                        "LANG",
                        "LC_ALL",
                        "LD_LIBRARY_PATH",
                    )
                    if key in os.environ
                }
                env["HOME"] = home
                for key, subdir in (
                    ("XDG_CONFIG_HOME", "config"),
                    ("XDG_DATA_HOME", "data"),
                    ("XDG_CACHE_HOME", "cache"),
                    ("XDG_RUNTIME_DIR", "runtime"),
                ):
                    path = Path(home) / subdir
                    path.mkdir(mode=0o700)
                    env[key] = str(path)
                port = server.server_port
                with tempfile.TemporaryFile(mode="w+") as output:
                    proc = subprocess.Popen(
                        [
                            str(binary),
                            "--port",
                            str(port),
                            "--profile",
                            f"port-conflict-smoke-{os.getpid()}",
                        ],
                        env=env,
                        stdout=output,
                        stderr=subprocess.STDOUT,
                        start_new_session=True,
                    )
                    loop = GLib.MainLoop()
                    timed_out = False

                    def poll():
                        if proc.poll() is not None:
                            loop.quit()
                            return False
                        return True

                    def timeout():
                        nonlocal timed_out
                        timed_out = True
                        os.killpg(proc.pid, signal.SIGKILL)
                        loop.quit()
                        return False

                    GLib.timeout_add(50, poll)
                    GLib.timeout_add_seconds(15, timeout)
                    try:
                        loop.run()
                        proc.wait(timeout=5)
                    finally:
                        if proc.poll() is None:
                            os.killpg(proc.pid, signal.SIGKILL)
                            proc.wait()
                    output.seek(0)
                    logs = output.read()
                assert not timed_out, logs
                assert proc.returncode == 1, (proc.returncode, logs)
                assert "/api/0/info" in requests, (requests, logs)
                matches = [item for item in seen if "aw-qt" in item["body"]]
                assert len(matches) == 1, (seen, logs)
                assert matches[0]["before_exit"], (seen, logs)
                assert str(port) in matches[0]["body"], seen
                assert "startup entries" in matches[0]["body"], seen
                print(json.dumps(seen, indent=2))
                print(
                    "PASS: ActivityWatch conflict notification delivered before exit 1"
                )
        finally:
            server.shutdown()
            thread.join(timeout=5)
    del notification
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
