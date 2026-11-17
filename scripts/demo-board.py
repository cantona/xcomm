#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Su Kang Yin <paradyse@gmail.com>
# SPDX-License-Identifier: GPL-2.0-only
"""demo-board.py LINK: a made-up board on a pty, for the README's pictures.

Makes a pty, links LINK to its slave, and plays a board's console on the
master: a U-Boot countdown, a kernel boot, a login, and a shell that
answers a few commands. Nothing on it is anyone's real hardware.

The README's session picture: run `xcomm -b 115200 LINK` in a terminal (a
herdr pane), drive it as the agent skill's examples do (send '', wait 'Hit
any key', run -n ' ' to '=> ', run boot, root, secret, uname -a, ip -4 addr
show eth0), capture the screen with `herdr pane read --format ansi`, put
/dev/ttyUSB0 for LINK's path, join lines the pane wrapped, and render it with
`scripts/ansi2svg.py --title 'xcomm /dev/ttyUSB0' --cols 92`. The agent
picture is the same commands' real output from a shell, rendered with
`--cols 118`.
"""

import os
import pty
import select
import sys
import time
import tty

GREEN, RED, YEL, BLUE, BOLD, DIM, RST = (
    "\x1b[32m", "\x1b[31m", "\x1b[33m", "\x1b[34m", "\x1b[1m", "\x1b[2m", "\x1b[0m")

BOOT = [
    "",
    "U-Boot 2024.01 (Mar 02 2026 - 10:14:03 +0000)",
    "",
    "CPU:   i.MX8MP rev1.1 1600 MHz",
    "DRAM:  2 GiB",
    "MMC:   FSL_SDHC: 1, FSL_SDHC: 2",
    "Net:   eth0: ethernet@30be0000",
]
KERNEL = [
    "[    0.000000] Booting Linux on physical CPU 0x0000000000 [0x410fd034]",
    "[    0.000000] Linux version 6.6.23 (builder@ci) (aarch64-linux-gcc 13.2.0) #1 SMP PREEMPT",
    "[    0.000000] Machine model: Demo Board i.MX8MP",
    "[    1.204551] mmcblk2: mmc2:0001 DG4016 14.7 GiB",
    "[    1.981123] EXT4-fs (mmcblk2p2): mounted filesystem with ordered data mode",
]
UNITS = [
    "Started Journal Service.",
    "Mounted Kernel Configuration File System.",
    "Started Network Time Synchronization.",
    "Reached target Network.",
    "Started OpenSSH Daemon.",
]
ANSWERS = {
    "uname -a": "Linux demo 6.6.23 #1 SMP PREEMPT aarch64 GNU/Linux",
    "cat /proc/version": "Linux version 6.6.23 (builder@ci) (aarch64-linux-gcc 13.2.0) #1 SMP PREEMPT",
    "ip -4 addr show eth0": (
        "2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc mq state UP\r\n"
        "    inet 192.168.7.2/24 brd 192.168.7.255 scope global eth0"
    ),
    "uptime": " 10:21:07 up 1 min,  1 user,  load average: 0.31, 0.12, 0.04",
    "dmesg | tail -2": (
        "[    3.118404] imx-dwmac 30bf0000.ethernet eth0: Link is Up - 1Gbps/Full\r\n"
        "[    3.130002] IPv6: ADDRCONF(NETDEV_CHANGE): eth0: link becomes ready"
    ),
}
PROMPT = f"{GREEN}root@demo{RST}:{BLUE}~{RST}# "


def main():
    link = sys.argv[1]
    master, slave = pty.openpty()
    tty.setraw(slave)
    try:
        os.unlink(link)
    except FileNotFoundError:
        pass
    os.symlink(os.ttyname(slave), link)
    out = lambda s: os.write(master, s.encode())

    def read_line(timeout=None):
        buf = b""
        while True:
            r, _, _ = select.select([master], [], [], timeout)
            if not r:
                return None
            b = os.read(master, 1024)
            buf += b
            if b"\r" in buf or b"\n" in buf:
                return buf.split(b"\r")[0].split(b"\n")[0].decode(errors="replace")

    # Waits for the session, then boots when anything comes (a "reset").
    read_line()
    for l in BOOT:
        out(l + "\r\n")
        time.sleep(0.02)
    out("Hit any key to stop autoboot:  2 ")
    stopped = bool(select.select([master], [], [], 3)[0])
    if stopped:
        os.read(master, 1024)
    if stopped:
        out("\r\n=> ")
        read_line()
        out("boot\r\n")
    out("\r\nStarting kernel ...\r\n\r\n")
    for l in KERNEL:
        out(l + "\r\n")
        time.sleep(0.03)
    for u in UNITS:
        out(f"[  {GREEN}OK{RST}  ] {u}\r\n")
        time.sleep(0.03)
    out(f"[{RED}FAILED{RST}] Failed to start {BOLD}Bluetooth Service{RST}.\r\n")
    out(f"{DIM}See 'systemctl status bluetooth.service' for details.{RST}\r\n\r\n")
    out("Demo Linux 2026.03 demo ttymxc1\r\n\r\ndemo login: ")
    while read_line() is None:
        pass
    out("root\r\nPassword: ")
    read_line()
    out(f"\r\n{YEL}Last login: Mon Mar  2 10:20:11 2026{RST}\r\n{PROMPT}")
    while True:
        cmd = read_line()
        if cmd is None:
            continue
        out(cmd + "\r\n")
        if cmd in ANSWERS:
            out(ANSWERS[cmd] + "\r\n")
        elif cmd:
            out(f"-sh: {cmd.split()[0]}: not found\r\n")
        out(PROMPT)


if __name__ == "__main__":
    main()
