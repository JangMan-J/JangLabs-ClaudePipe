#!/usr/bin/env python3
"""cmd-key-watcher — observe Right Shift and latch a one-shot "command mode" flag.

Part of the dictation intent-router (claude-pipe voxtype consumer). The gesture is:
hold Right Ctrl (voxtype PTT, unchanged) AND Right Shift together => that recording
is a COMMAND for the agent rather than text to type. voxtype hands the gate only the
transcript, not the keyboard state, so this watcher bridges the gap:

  Right Shift DOWN -> write the cmd-mode flag (with a fresh timestamp)
  Right Shift UP   -> refresh the timestamp (leave the flag; the gate consumes it)

The flag is a one-shot: llm-gate.sh reads it, routes the transcript to the agent,
and DELETES it. A flag older than STALE_SECS with no transcription is ignored by
the gate (guards against a stray Shift press that never became a command dictation).

READ-ONLY: the device is observed, never grabbed, so Right Shift still works as a
normal modifier for typing. Requires membership in the 'input' group (no root).

Watches the physical keyboard by NAME (event numbers are unstable across reboots).
"""
import os
import sys
import time
import errno
import selectors

import evdev
from evdev import ecodes

# --- config -------------------------------------------------------------------
RUNTIME = os.environ.get("XDG_RUNTIME_DIR", "/tmp")
FLAG = os.path.join(RUNTIME, "voxtype-cmd-mode")
# Match the device that actually emits Right Shift (verified: the physical
# keyboard, not the input-remapper virtuals). Substring match on device name.
DEVICE_NAME_SUBSTR = os.environ.get("CMD_WATCH_DEVICE", "N-KEY Device")
KEYCODE = ecodes.KEY_RIGHTSHIFT
DEBUG = os.environ.get("CMD_WATCH_DEBUG", "") != ""


def find_devices():
    """All input devices whose name matches DEVICE_NAME_SUBSTR and have Right Shift."""
    out = []
    for path in evdev.list_devices():
        try:
            d = evdev.InputDevice(path)
        except OSError:
            continue
        if DEVICE_NAME_SUBSTR in (d.name or "") and \
           KEYCODE in d.capabilities().get(ecodes.EV_KEY, []):
            out.append(d)
    return out


def set_flag():
    # write the current time; presence = "Right Shift engaged for a command"
    try:
        with open(FLAG, "w") as f:
            f.write(str(time.time()))
        if DEBUG:
            sys.stderr.write(f"cmd-key-watcher: wrote flag {FLAG}\n"); sys.stderr.flush()
    except OSError as e:
        if DEBUG:
            sys.stderr.write(f"cmd-key-watcher: FLAG WRITE FAILED {FLAG}: {e}\n"); sys.stderr.flush()


def touch_flag():
    # refresh mtime/contents if the flag exists (keep it fresh across the recording)
    if os.path.exists(FLAG):
        set_flag()


def main():
    devices = find_devices()
    if not devices:
        sys.stderr.write(
            f"cmd-key-watcher: no device matching '{DEVICE_NAME_SUBSTR}' with Right Shift\n")
        sys.exit(1)
    sys.stderr.write(
        "cmd-key-watcher: watching " + ", ".join(d.path for d in devices) + "\n")
    sys.stderr.flush()

    sel = selectors.DefaultSelector()
    for d in devices:
        sel.register(d.fileno(), selectors.EVENT_READ, d)

    while True:
        for key, _ in sel.select():
            d = key.data
            try:
                for ev in d.read():
                    if ev.type == ecodes.EV_KEY and ev.code == KEYCODE:
                        if DEBUG:
                            sys.stderr.write(f"cmd-key-watcher: RIGHTSHIFT {ev.value}\n")
                            sys.stderr.flush()
                        if ev.value == 1:      # DOWN
                            set_flag()
                        elif ev.value == 2:    # autorepeat while held
                            touch_flag()
                        # UP (0): leave the flag for the gate to consume.
            except OSError as e:
                # device unplugged/replugged: drop it; a supervisor restart re-resolves
                if e.errno in (errno.ENODEV, errno.EBADF):
                    try:
                        sel.unregister(d.fileno())
                    except (KeyError, ValueError):
                        pass
                    if not sel.get_map():
                        sys.stderr.write("cmd-key-watcher: all devices gone; exiting for restart\n")
                        sys.exit(1)


if __name__ == "__main__":
    main()
