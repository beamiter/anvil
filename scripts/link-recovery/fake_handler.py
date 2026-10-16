#!/usr/bin/env python3
"""Owned D-Bus application endpoint, no processes or network launched."""
import os
from pathlib import Path
assert os.environ.get('ANVIL_HOSTED_FIXTURE') == '1'
import gi
gi.require_version('Gio', '2.0')
from gi.repository import Gio, GLib
xml = '''<node><interface name="org.freedesktop.Application"><method name="Open">
<arg type="as" direction="in"/><arg type="a{sv}" direction="in"/>
</method></interface></node>'''
info = Gio.DBusNodeInfo.new_for_xml(xml)
loop = GLib.MainLoop()
registrations = []
def call(conn, sender, path, iface, method, params, invocation):
    assert method == 'Open'
    uris, platform = params.unpack()
    assert len(uris) == 1
    Path('handler-uri.bin').write_bytes(uris[0].encode())
    assert Path('handler-uri.bin').read_bytes() == Path('expected.bin').read_bytes()
    def deny():
        invocation.return_dbus_error('org.freedesktop.DBus.Error.AccessDenied',
                                     'owned synthetic access denial')
        Path('handler.responded').write_text('denied\n')
        return GLib.SOURCE_REMOVE
    GLib.timeout_add(600, deny)
def acquired(conn, name):
    registrations.append(conn.register_object('/org/example/AnvilRecorder', info.interfaces[0], call, None, None))
    Path('handler.ready').write_text('ready\n')
def lost(*args):
    raise SystemExit('owned handler name ownership failed')
owner = Gio.bus_own_name(Gio.BusType.SESSION, 'org.example.AnvilRecorder',
    Gio.BusNameOwnerFlags.DO_NOT_QUEUE, None, acquired, lost)
loop.run()
