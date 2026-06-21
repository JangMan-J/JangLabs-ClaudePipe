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
The match is RE-SCANNED on a timer, not resolved once at startup: at boot the
keyboard exposes several /dev/input/eventN nodes that enumerate at different times,
and the node that actually emits Right Shift can appear AFTER this watcher starts
(observed: two "N-KEY Device" nodes; only the late one carries Right Shift). A
one-shot scan would lock onto the wrong/early node and miss every keypress. So we
attach every matching node we can find now, and keep re-scanning to pick up nodes
that appear later (late boot enumeration, hotplug, replug).
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
# How often to re-scan for matching nodes that weren't present at startup.
RESCAN_SECS = float(os.environ.get("CMD_WATCH_RESCAN_SECS", "3"))


def matching_paths():
    """Paths of input devices whose name matches and that advertise Right Shift."""
    out = []
    for path in evdev.list_devices():
        try:
            d = evdev.InputDevice(path)
        except OSError:
            continue
        try:
            if DEVICE_NAME_SUBSTR in (d.name or "") and \
               KEYCODE in d.capabilities().get(ecodes.EV_KEY, []):
                out.append(path)
        finally:
            d.close()
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
    sel = selectors.DefaultSelector()
    attached = {}  # path -> InputDevice

    def detach(path):
        d = attached.pop(path, None)
        if d is None:
            return
        try:
            sel.unregister(d.fileno())
        except (KeyError, ValueError):
            pass
        try:
            d.close()
        except OSError:
            pass

    def rescan():
        """Attach any newly-appeared matching nodes; drop any that vanished."""
        try:
            want = set(matching_paths())
        except OSError:
            return
        for path in want - set(attached):
            try:
                d = evdev.InputDevice(path)
                sel.register(d.fileno(), selectors.EVENT_READ, d)
                attached[path] = d
                sys.stderr.write(f"cmd-key-watcher: attached {path} ({d.name!r})\n")
                sys.stderr.flush()
            except OSError as e:
                if DEBUG:
                    sys.stderr.write(f"cmd-key-watcher: attach {path} failed: {e}\n")
                    sys.stderr.flush()
        for path in set(attached) - want:
            detach(path)
            if DEBUG:
                sys.stderr.write(f"cmd-key-watcher: detached {path} (gone)\n")
                sys.stderr.flush()

    rescan()
    if not attached:
        # Not fatal: the node we need may enumerate shortly after boot. Keep
        # re-scanning instead of exiting — exiting here just churns the service
        # until the late node appears, and risks the start-limit tripping.
        sys.stderr.write(
            f"cmd-key-watcher: no device matching '{DEVICE_NAME_SUBSTR}' yet; "
            f"re-scanning every {RESCAN_SECS:g}s\n")
        sys.stderr.flush()
    else:
        sys.stderr.write(
            "cmd-key-watcher: watching " + ", ".join(sorted(attached)) + "\n")
        sys.stderr.flush()

    while True:
        # select() wakes on input OR the rescan timeout — whichever comes first,
        # so a node that appears between events still gets picked up promptly.
        for key, _ in sel.select(timeout=RESCAN_SECS):
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
                # device unplugged/replugged: drop it; rescan re-attaches on return
                if e.errno in (errno.ENODEV, errno.EBADF):
                    detach(d.path)
        rescan()


if __name__ == "__main__":
    main()
