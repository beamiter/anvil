#!/usr/bin/env python3
"""Owned portal frontend. Only run by the hosted suite, on its private bus."""
import os, sys
from pathlib import Path
assert os.environ.get('ANVIL_HOSTED_FIXTURE') == '1'
import gi
gi.require_version('Gio', '2.0')
from gi.repository import Gio, GLib
case = sys.argv[1]
xml = '''<node><interface name="org.freedesktop.portal.OpenURI">
<property name="version" type="u" access="read"/>
<method name="OpenURI"><arg type="s" direction="in"/><arg type="s" direction="in"/>
<arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
</interface><interface name="org.freedesktop.portal.Request">
<method name="Close"/><signal name="Response"><arg type="u"/><arg type="a{sv}"/></signal>
</interface></node>'''
info = Gio.DBusNodeInfo.new_for_xml(xml)
loop = GLib.MainLoop()
registrations = []

def call(conn, sender, path, iface, method, parameters, invocation):
    if method == 'Close':
        Path('portal.close').write_text('closed\n')
        invocation.return_value(GLib.Variant('()', ()))
        return
    assert method == 'OpenURI'
    parent, uri, opts = parameters.unpack()
    assert parent == ''
    received = uri.encode('utf-8')
    # Deliberate private artifact; never print the synthetic secret-bearing URI.
    Path('portal-uri.bin').write_bytes(received)
    assert received == Path('expected.bin').read_bytes(), 'portal changed original bytes'
    token = opts['handle_token']
    handle = '/org/freedesktop/portal/desktop/request/' + sender[1:].replace('.', '_') + '/' + token
    registrations.append(conn.register_object(handle, info.interfaces[1], call, None, None))
    invocation.return_value(GLib.Variant('(o)', (handle,)))
    Path('portal.received').write_text('received\n')
    def respond():
        response = {'chooser-cancel': 1, 'deny': 2, 'unknown': 77,
                    'stale': 2, 'client-cancel': 2, 'portal-success': 0}[case]
        conn.emit_signal(sender, handle, 'org.freedesktop.portal.Request', 'Response',
                         GLib.Variant('(ua{sv})', (response, {})))
        Path('portal.responded').write_text(str(response))
        return GLib.SOURCE_REMOVE
    GLib.timeout_add(600, respond)

def acquired(conn, name):
    registrations.append(conn.register_object('/org/freedesktop/portal/desktop', info.interfaces[0],
        call, lambda *args: GLib.Variant('u', 4), None))
    Path('portal.ready').write_text('ready\n')

def lost(*args):
    sys.exit('private portal name ownership failed')
owner = Gio.bus_own_name(Gio.BusType.SESSION, 'org.freedesktop.portal.Desktop',
    Gio.BusNameOwnerFlags.DO_NOT_QUEUE, None, acquired, lost)
loop.run()
