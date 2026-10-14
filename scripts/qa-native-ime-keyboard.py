#!/usr/bin/env python3
"""Fixed simple-IM sequences for the nonexecuting native-ime QA window only.

Uses the same official libX11/libXtst CI dependencies as the pointer fixture.
Never accepts arbitrary text, key names, commands, or a target window argument.
"""
import ctypes
import sys

TITLE = b"anvil-native-ime-qa"
ACTIONS = {"compose": ("4", "e", "2", "d"), "commit": ("Return",),
           "cancel": ("Escape",), "ascii-a": ("a",), "ascii-b": ("b",)}


def main():
    if len(sys.argv) != 2 or sys.argv[1] not in ACTIONS:
        raise SystemExit("Expected compose, commit, cancel, ascii-a, or ascii-b")
    x = ctypes.CDLL("libX11.so.6")
    xt = ctypes.CDLL("libXtst.so.6")
    pointer, window = ctypes.c_void_p, ctypes.c_ulong
    x.XOpenDisplay.argtypes, x.XOpenDisplay.restype = [ctypes.c_char_p], pointer
    x.XDefaultRootWindow.argtypes, x.XDefaultRootWindow.restype = [pointer], window
    x.XFetchName.argtypes = [pointer, window, ctypes.POINTER(pointer)]
    x.XQueryTree.argtypes = [pointer, window, ctypes.POINTER(window), ctypes.POINTER(window),
                            ctypes.POINTER(ctypes.POINTER(window)), ctypes.POINTER(ctypes.c_uint)]
    x.XFree.argtypes = [pointer]
    x.XStringToKeysym.argtypes, x.XStringToKeysym.restype = [ctypes.c_char_p], ctypes.c_ulong
    x.XKeysymToKeycode.argtypes, x.XKeysymToKeycode.restype = [pointer, ctypes.c_ulong], ctypes.c_ubyte
    x.XSetInputFocus.argtypes = [pointer, window, ctypes.c_int, ctypes.c_ulong]
    x.XGetInputFocus.argtypes = [pointer, ctypes.POINTER(window), ctypes.POINTER(ctypes.c_int)]
    x.XSync.argtypes = [pointer, ctypes.c_int]
    x.XCloseDisplay.argtypes = [pointer]
    xt.XTestFakeKeyEvent.argtypes = [pointer, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
    display = x.XOpenDisplay(None)
    if not display:
        raise SystemExit("XOpenDisplay failed")
    held = set()
    try:
        def find(w, depth=0):
            name = pointer()
            if x.XFetchName(display, w, ctypes.byref(name)) and name.value:
                title = ctypes.string_at(name.value)
                x.XFree(name)
                if title == TITLE:
                    return w
            if depth >= 4:
                return None
            root, parent, children, count = window(), window(), ctypes.POINTER(window)(), ctypes.c_uint()
            if not x.XQueryTree(display, w, ctypes.byref(root), ctypes.byref(parent),
                                ctypes.byref(children), ctypes.byref(count)):
                return None
            ids = [children[i] for i in range(count.value)]
            if children:
                x.XFree(ctypes.cast(children, pointer))
            for child in ids:
                found = find(child, depth + 1)
                if found:
                    return found
            return None

        target = find(x.XDefaultRootWindow(display))
        if not target:
            raise RuntimeError("Dedicated nonexecuting native IME QA window is missing")
        x.XSetInputFocus(display, target, 1, 0)
        x.XSync(display, 0)
        focused, revert = window(), ctypes.c_int()
        x.XGetInputFocus(display, ctypes.byref(focused), ctypes.byref(revert))
        if focused.value != target:
            raise RuntimeError("QA window did not acquire X input focus")

        def key(name, down):
            code = x.XKeysymToKeycode(display, x.XStringToKeysym(name.encode("ascii")))
            if not code or not xt.XTestFakeKeyEvent(display, code, int(down), 0):
                raise RuntimeError("XTest key event was refused")
            (held.add if down else held.discard)(code)
            x.XSync(display, 0)

        def tap(name):
            key(name, True)
            key(name, False)

        if sys.argv[1] == "compose":
            key("Control_L", True)
            key("Shift_L", True)
            tap("u")
            key("Shift_L", False)
            key("Control_L", False)
        for name in ACTIONS[sys.argv[1]]:
            tap(name)
    finally:
        for code in held:
            xt.XTestFakeKeyEvent(display, code, 0, 0)
        x.XSync(display, 0)
        x.XCloseDisplay(display)
    print("Native IME fixed sequence completed")


if __name__ == "__main__":
    main()
