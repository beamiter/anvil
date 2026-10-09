#!/usr/bin/env python3
"""Drive a synthetic selection test window on its isolated Xvfb display.

Requires libX11 and libXtst. The GTK regression supplies widget coordinates;
this helper refuses to act unless its specifically named QA window exists.
No keystrokes or terminal input are generated.
"""

import ctypes
import sys
import time


x11 = ctypes.CDLL("libX11.so.6")
xtest = ctypes.CDLL("libXtst.so.6")
Window = ctypes.c_ulong
Pointer = ctypes.c_void_p
x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
x11.XOpenDisplay.restype = Pointer
x11.XDefaultRootWindow.argtypes = [Pointer]
x11.XDefaultRootWindow.restype = Window
x11.XQueryTree.argtypes = [
    Pointer, Window, ctypes.POINTER(Window), ctypes.POINTER(Window),
    ctypes.POINTER(ctypes.POINTER(Window)), ctypes.POINTER(ctypes.c_uint),
]
x11.XFetchName.argtypes = [Pointer, Window, ctypes.POINTER(Pointer)]
x11.XFree.argtypes = [Pointer]
x11.XTranslateCoordinates.argtypes = [
    Pointer, Window, Window, ctypes.c_int, ctypes.c_int,
    ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int),
    ctypes.POINTER(Window),
]
x11.XFlush.argtypes = [Pointer]
x11.XCloseDisplay.argtypes = [Pointer]
xtest.XTestFakeMotionEvent.argtypes = [
    Pointer, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_ulong,
]
xtest.XTestFakeButtonEvent.argtypes = [
    Pointer, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong,
]


def find_window(display, window, depth=0):
    name = Pointer()
    if x11.XFetchName(display, window, ctypes.byref(name)) and name.value:
        title = ctypes.string_at(name.value).decode(errors="replace")
        x11.XFree(name)
        if title == "anvil-cross-selection-qa":
            return window
    if depth > 3:
        return None
    root, parent = Window(), Window()
    children = ctypes.POINTER(Window)()
    count = ctypes.c_uint()
    if not x11.XQueryTree(
        display, window, ctypes.byref(root), ctypes.byref(parent),
        ctypes.byref(children), ctypes.byref(count),
    ):
        return None
    child_ids = [children[index] for index in range(count.value)]
    if children:
        x11.XFree(ctypes.cast(children, Pointer))
    for child in child_ids:
        found = find_window(display, child, depth + 1)
        if found:
            return found
    return None


def main():
    if len(sys.argv) != 5:
        raise SystemExit("Usage: qa-cross-selection-pointer.py START_X START_Y END_X END_Y")
    display = x11.XOpenDisplay(None)
    if not display:
        raise SystemExit("XOpenDisplay failed")
    pressed = False
    try:
        root = x11.XDefaultRootWindow(display)
        window = find_window(display, root)
        if not window:
            raise SystemExit("Synthetic selection QA window is missing")
        origin_x, origin_y, child = ctypes.c_int(), ctypes.c_int(), Window()
        if not x11.XTranslateCoordinates(
            display, window, root, 0, 0, ctypes.byref(origin_x),
            ctypes.byref(origin_y), ctypes.byref(child),
        ):
            raise SystemExit("Cannot locate QA window coordinates")
        start_x, start_y, end_x, end_y = (int(float(value)) for value in sys.argv[1:])
        start_x += origin_x.value
        end_x += origin_x.value
        start_y += origin_y.value
        end_y += origin_y.value

        def move(x, y):
            if not xtest.XTestFakeMotionEvent(display, -1, x, y, 0):
                raise RuntimeError("XTest motion was refused")
            x11.XFlush(display)
            time.sleep(0.04)

        move(start_x, start_y)
        if not xtest.XTestFakeButtonEvent(display, 1, 1, 0):
            raise RuntimeError("XTest press was refused")
        pressed = True
        x11.XFlush(display)
        time.sleep(0.08)
        for step in range(1, 9):
            move(
                round(start_x + (end_x - start_x) * step / 8),
                round(start_y + (end_y - start_y) * step / 8),
            )
    finally:
        if pressed:
            xtest.XTestFakeButtonEvent(display, 1, 0, 0)
            x11.XFlush(display)
            time.sleep(0.08)
        x11.XCloseDisplay(display)
    print("Pointer drag completed through XTest")


if __name__ == "__main__":
    main()
