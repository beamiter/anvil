#!/usr/bin/python3
"""Native GTK paint/allocation fixture, not full Anvil or physical input acceptance."""
import argparse
import json
import pathlib
import time
import gi
gi.require_version('Gtk', '4.0')
from gi.repository import Gtk, Gdk, GLib

p = argparse.ArgumentParser()
p.add_argument('--baseline', required=True)
p.add_argument('--candidate', required=True)
p.add_argument('--artifacts', required=True)
a = p.parse_args()
Gtk.init()
out = pathlib.Path(a.artifacts)
out.mkdir(parents=True, exist_ok=True)
display = Gdk.Display.get_default()
assert display is not None, 'A real GTK display is required'
settings = Gtk.Settings.get_default()
settings.set_property('gtk-enable-animations', False)

def settle():
    until = time.monotonic() + .3
    while time.monotonic() < until:
        while GLib.MainContext.default().iteration(False):
            pass
        time.sleep(.005)

def provider(path):
    css = Gtk.CssProvider()
    errors = []
    css.connect('parsing-error', lambda _, section, error: errors.append(str(error)))
    css.load_from_data(pathlib.Path(path).read_bytes())
    assert not errors, errors
    return css

base = provider(a.baseline)
patch = provider(a.candidate)
Gtk.StyleContext.add_provider_for_display(display, base, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION + 20)
window = Gtk.Window(title='Anvil scoped controls verification')
window.set_default_size(800, 540)
window.add_css_class('anvil-font-sync')
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
box.add_css_class('workspace-dialog')
box.set_margin_start(20)
box.set_margin_end(20)
box.set_margin_top(20)
box.set_margin_bottom(20)
window.set_child(box)
check = Gtk.CheckButton(label='Check option')
radio = Gtk.CheckButton(label='Radio option')
radio_peer = Gtk.CheckButton(label="Radio group peer")
radio.set_group(radio_peer)
switch = Gtk.Switch()
switch.set_halign(Gtk.Align.START)
scale = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 100, 1)
scale.set_value(45)
spin = Gtk.SpinButton.new_with_range(0, 100, 1)
spin.set_value(10)
entry = Gtk.Entry(text='Native input')
controls = {'check': check, 'radio': radio, 'radio_peer': radio_peer, 'switch': switch, 'scale': scale, 'spin': spin, 'entry': entry}
for control in controls.values():
    box.append(control)
window.present()
settle()

def geometry():
    return {name: [w.get_width(), w.get_height(), list(w.measure(Gtk.Orientation.HORIZONTAL, -1)), list(w.measure(Gtk.Orientation.VERTICAL, -1))] for name, w in controls.items()}

def state_values():
    return (check.get_active(), radio.get_active(), radio_peer.get_active(),
            switch.get_active(), scale.get_value(), spin.get_value(), entry.get_text())

results = []
# Reuse the same widgets for a return transition, not two isolated launches.
for transition, dark in enumerate((False, True, False)):
    settings.set_property('gtk-application-prefer-dark-theme', dark)
    settle()
    assert settings.get_property('gtk-application-prefer-dark-theme') == dark
    for checked in (False, True):
        for state in ('normal', 'hover', 'focus', 'disabled'):
            for w in controls.values():
                w.set_sensitive(state != 'disabled')
                w.unset_state_flags(Gtk.StateFlags.PRELIGHT | Gtk.StateFlags.FOCUSED | Gtk.StateFlags.FOCUS_VISIBLE)
                if state == 'hover':
                    w.set_state_flags(Gtk.StateFlags.PRELIGHT, False)
                if state == 'focus':
                    w.set_state_flags(Gtk.StateFlags.FOCUSED | Gtk.StateFlags.FOCUS_VISIBLE, False)
            check.set_active(checked)
            # Both members are mapped; check true exclusive selection, not just
            # the unchecked radio CSS node as the previous fixture did.
            (radio if checked else radio_peer).set_active(True)
            switch.set_active(checked)
            assert radio.get_active() == checked
            assert radio_peer.get_active() != checked
            settle()
            before = geometry()
            values = state_values()
            assert all(w.get_mapped() for w in controls.values())
            Gtk.StyleContext.add_provider_for_display(display, patch, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION + 21)
            settle()
            after = geometry()
            assert before == after, (dark, state, checked, before, after)
            assert state_values() == values, 'Applying paint must not mutate input values'
            Gtk.StyleContext.remove_provider_for_display(display, patch)
            settle()
            assert geometry() == before, 'Removing candidate must restore the baseline allocation'
            assert state_values() == values
            results.append({'transition': transition, 'dark_preference': dark,
                            'state': state, 'checked': checked, 'geometry_equal': True,
                            'values_retained': True, 'geometry': after})
(out / 'controls-geometry.json').write_text(json.dumps(results, indent=2))
window.destroy()
print('PASS: GTK CSS parses and 24 style-state allocations match baseline; synthetic GTK states/preferences, not full App/OS input, terminal-palette or visual contrast acceptance')
