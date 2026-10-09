#!/usr/bin/env python3
"""Exercise only the named, nonexecuting PTY fixture in an isolated Xvfb.

This helper intentionally changes that test display's autorepeat rate. Never
run it against a normal desktop display or a terminal with a live shell.
"""
import ctypes as c
import importlib.util
import os
import pathlib
import sys
import time
sys.dont_write_bytecode = True
helper = pathlib.Path(__file__).with_name('qa-cross-selection-pointer.py')
spec = importlib.util.spec_from_file_location('pointer_qa', helper)
qa = importlib.util.module_from_spec(spec)
spec.loader.exec_module(qa)
x, t = qa.x11, qa.xtest
x.XKeysymToKeycode.argtypes = [qa.Pointer, c.c_ulong]
x.XKeysymToKeycode.restype = c.c_ubyte
x.XSetInputFocus.argtypes = [qa.Pointer, qa.Window, c.c_int, c.c_ulong]
x.XkbSetAutoRepeatRate.argtypes = [qa.Pointer, c.c_uint, c.c_uint, c.c_uint]
x.XCreateSimpleWindow.argtypes = [qa.Pointer, qa.Window, c.c_int, c.c_int, c.c_uint, c.c_uint, c.c_uint, c.c_ulong, c.c_ulong]
x.XCreateSimpleWindow.restype = qa.Window
x.XMapWindow.argtypes = [qa.Pointer, qa.Window]
x.XDestroyWindow.argtypes = [qa.Pointer, qa.Window]
t.XTestFakeKeyEvent.argtypes = [qa.Pointer, c.c_uint, c.c_int, c.c_ulong]
d = x.XOpenDisplay(None)
if not d: raise SystemExit('display unavailable')
root = x.XDefaultRootWindow(d)
window = qa.find_window(d, root)
if not window: raise SystemExit('synthetic nonexecuting PTY window missing')
mode = sys.argv[1] if len(sys.argv) > 1 else 'held'
inherited = "inherited" in mode or os.environ.get("ANVIL_QA_INHERITED_ENTER") == "1"
# The mouse-only fixture requires one recovery hint. That also proves the
# warmup release settled and no inherited repeat was tracked before insertion.
prewarm = os.environ.get("ANVIL_QA_PREWARM_ENTER") == "1"
if prewarm and not (inherited and mode.startswith("review-mouse-only")):
    raise RuntimeError("prewarm requires the passive review mouse-only inherited fixture")
outside = root
if inherited or 'blur' in mode or 'missed' in mode:
    outside = x.XCreateSimpleWindow(d, root, 1, 1, 1, 1, 0, 0, 0)
    if not outside:
        raise RuntimeError('cannot create inert outside-focus window')
    x.XMapWindow(d, outside)
key = x.XKeysymToKeycode(d, 0xff8d if 'keypad' in mode else 0xff0d)
x.XkbSetAutoRepeatRate(d, 0x100, 2000 if inherited else (400 if "review" in mode else 120), 40)
x.XSetInputFocus(d, window if prewarm else (outside if inherited else window), 2, 0)
x.XFlush(d)
time.sleep(.1)
modifier = None
primary_down = False
secondary = None
if os.environ.get("ANVIL_QA_DUAL_ENTER") == "1":
    if "queued" not in mode:
        raise RuntimeError("dual-key coverage requires the passive queued fixture")
    secondary = x.XKeysymToKeycode(d, 0xff0d if "keypad" in mode else 0xff8d)
    if not secondary or secondary == key:
        raise RuntimeError("main and keypad Enter must have distinct physical keycodes")
try:
    if prewarm:
        # Settle a local release first, so only the later real focus loss can
        # rearm uncertainty. The fixture focuses a passive Entry at this point.
        t.XTestFakeKeyEvent(d, key, 1, 0); x.XFlush(d); time.sleep(.03)
        t.XTestFakeKeyEvent(d, key, 0, 0); x.XFlush(d); time.sleep(.15)
        x.XSetInputFocus(d, outside, 2, 0); x.XFlush(d); time.sleep(.1)
        print('Sent local Enter release before the inert outside-focus press', flush=True)
    if "mouse" not in mode or prewarm:
        t.XTestFakeKeyEvent(d, key, 1, 0); x.XFlush(d)
        primary_down = True
    elif "known" in mode:
        t.XTestFakeKeyEvent(d, key, 1, 0); x.XFlush(d); time.sleep(.03)
        t.XTestFakeKeyEvent(d, key, 0, 0); x.XFlush(d); time.sleep(.15)
    if secondary is not None:
        t.XTestFakeKeyEvent(d, secondary, 1, 0); x.XFlush(d)
    if inherited:
        time.sleep(.06)
        x.XSetInputFocus(d, window, 2, 0); x.XFlush(d)
        time.sleep(.02)
    if 'queued' in mode:
        time.sleep(.06)
        pathlib.Path(sys.argv[2]).write_text('physical Enter held')
        if sys.stdin.readline().strip() != 'go':
            raise RuntimeError('missing queued-write handshake')
        if secondary is not None:
            time.sleep(.1)
            t.XTestFakeKeyEvent(d, key, 0, 0); x.XFlush(d)
            print('Released physical key', key, 'while', secondary, 'remains held')
            time.sleep(2.15 if inherited else .55)
        else:
            time.sleep(2.25 if inherited else .65)
    elif 'review' in mode:
        time.sleep(.06)
        pathlib.Path(sys.argv[2]).write_text('physical Enter held')
        px, py = (int(float(value)) for value in sys.stdin.readline().split())
        ox, oy, child = c.c_int(), c.c_int(), qa.Window()
        if not x.XTranslateCoordinates(d, window, root, 0, 0, c.byref(ox), c.byref(oy), c.byref(child)):
            raise RuntimeError('cannot locate synthetic review window')
        t.XTestFakeMotionEvent(d, -1, px + ox.value, py + oy.value, 0)
        x.XFlush(d); time.sleep(.04)
        t.XTestFakeButtonEvent(d, 1, 1, 0); x.XFlush(d); time.sleep(.04)
        t.XTestFakeButtonEvent(d, 1, 0, 0); x.XFlush(d)
        time.sleep(2.25 if inherited else .65)
    elif 'missed' in mode:
        time.sleep(.06)
        x.XSetInputFocus(d, outside, 2, 0); x.XFlush(d)
        time.sleep(.1)
        t.XTestFakeKeyEvent(d, key, 0, 0); x.XFlush(d)
        time.sleep(.1)
        x.XSetInputFocus(d, window, 2, 0); x.XFlush(d)
        time.sleep(.1)
        t.XTestFakeKeyEvent(d, key, 1, 0); x.XFlush(d)
        time.sleep(.1)
    elif 'modifier' in mode:
        time.sleep(.06)
        modifier = x.XKeysymToKeycode(d, 0xffe3)
        t.XTestFakeKeyEvent(d, modifier, 1, 0); x.XFlush(d)
        time.sleep(.3)
        t.XTestFakeKeyEvent(d, modifier, 0, 0); x.XFlush(d)
        time.sleep(.2)
    elif 'blur' in mode:
        time.sleep(.06)
        x.XSetInputFocus(d, outside, 2, 0); x.XFlush(d)
        time.sleep(.16)
        x.XSetInputFocus(d, window, 2, 0); x.XFlush(d)
        time.sleep(.35)
    else:
        time.sleep(.03 if mode.startswith('tap') else .6)
finally:
    if secondary is not None:
        t.XTestFakeKeyEvent(d, secondary, 0, 0)
    if modifier is not None:
        t.XTestFakeKeyEvent(d, modifier, 0, 0)
    if primary_down:
        t.XTestFakeKeyEvent(d, key, 0, 0); x.XFlush(d)
    time.sleep(.15)
    if "mouse-only" in mode and not prewarm:
        t.XTestFakeKeyEvent(d, key, 1, 0); x.XFlush(d); time.sleep(.03)
        t.XTestFakeKeyEvent(d, key, 0, 0); x.XFlush(d); time.sleep(.15)
    if 'fresh' in mode:
        t.XTestFakeKeyEvent(d, key, 1, 0); x.XFlush(d)
        time.sleep(.03)
        t.XTestFakeKeyEvent(d, key, 0, 0); x.XFlush(d)
        time.sleep(.1)
    if outside != root:
        x.XDestroyWindow(d, outside)
    x.XCloseDisplay(d)
print('Synthetic Enter action completed:', mode)
