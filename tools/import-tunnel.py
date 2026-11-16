#!/usr/bin/env python3
"""Sets this Linux laptop up with a WireGuard tunnel to the LocalBeat server
(docs/server-library.md, step 3), so Studio plays from the server anywhere.

    tools/import-tunnel.py NAME [--mac musicuser@192.168.1.10]
    tools/import-tunnel.py NAME --from FILE

At home, it runs `sudo localbeat-people add NAME --json` on the Mac over SSH
(asking for the Mac password without showing it), which makes the laptop's
WireGuard peer and a NEW music login for NAME (an old secret stops working).
The answer stays in memory: the private key is never shown or left in a
file. Then it:
  - imports the WireGuard config into NetworkManager as "localbeat"
    (connects automatically; only the server, 10.8.0.1, goes through it);
  - sets Studio's music server to the tunnel address (http://10.8.0.1:4533)
    with that login and secret.
Sending rips is unchanged (SSH to the Mac's home address, home only).

--from FILE: use the output of `localbeat-people add NAME --json` saved
earlier (the file is deleted afterwards).

Close LocalBeat Studio first: it would write its old settings back.
NAME: lowercase letters, digits and -, e.g. my-laptop or my-linux-pc.
"""

import argparse
import getpass
import json
import os
import re
import subprocess
import sys
import tempfile

CONNECTION = "localbeat"


def die(msg):
    sys.exit(f"import-tunnel: {msg}")


def studio_running():
    r = subprocess.run(["pgrep", "-f", r"(^|/)localbeat-studio( |$)"], capture_output=True, text=True)
    return r.returncode == 0


def from_mac(mac, name):
    pw = getpass.getpass(f"Password for sudo on {mac.split('@')[-1]}: ")
    r = subprocess.run(
        ["ssh", mac, f"sudo -S -p '' localbeat-people add {name} --json"],
        input=pw + "\n",
        capture_output=True,
        text=True,
    )
    pw = None
    if r.returncode != 0:
        die("the Mac said: " + (r.stderr.strip() or r.stdout.strip() or f"exit {r.returncode}"))
    return r.stdout


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("name")
    ap.add_argument("--mac", default="musicuser@192.168.1.10", help="SSH login on the server (default: %(default)s)")
    ap.add_argument("--from", dest="src", metavar="FILE", help="saved output of localbeat-people add --json")
    a = ap.parse_args()
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,30}", a.name):
        die("NAME: lowercase letters, digits and -, e.g. my-laptop")
    if studio_running():
        die("close LocalBeat Studio first (it would write its old settings back)")
    config = os.path.join(os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config"), "rip-cd")
    prefs_file = os.path.join(config, "localbeat.json")
    have = subprocess.run(["nmcli", "-t", "-f", "NAME", "connection", "show"], capture_output=True, text=True)
    if have.returncode != 0:
        die("NetworkManager (nmcli) isn't available")
    if CONNECTION in have.stdout.split("\n"):
        die(f'NetworkManager already has a "{CONNECTION}" connection (remove it first: nmcli connection delete {CONNECTION})')

    raw = open(a.src).read() if a.src else from_mac(a.mac, a.name)
    # The JSON object is the line starting with "{" (anything else is noise).
    line = next((l for l in raw.splitlines() if l.strip().startswith("{")), None)
    if line is None:
        die("no answer from localbeat-people (nothing changed here)")
    d = json.loads(line)
    wg, secret, server, name = d["wireguard"], d["secret"], d["server"], d["name"]
    if "[Interface]" not in wg or "PrivateKey" not in wg:
        die("no WireGuard config in the answer")
    field = lambda k: next(l.split("=", 1)[1].strip() for l in wg.splitlines() if l.startswith(k))

    # NetworkManager names the connection and the interface after the file.
    tmpdir = tempfile.mkdtemp(prefix="lbwg-")
    conf = os.path.join(tmpdir, f"{CONNECTION}.conf")
    try:
        fd = os.open(conf, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.write(fd, (wg.rstrip("\n") + "\n").encode())
        os.close(fd)
        r = subprocess.run(["nmcli", "connection", "import", "type", "wireguard", "file", conf], capture_output=True, text=True)
        if r.returncode != 0:
            die("nmcli import failed: " + (r.stderr or r.stdout).strip())
        subprocess.run(["nmcli", "connection", "modify", CONNECTION, "connection.autoconnect", "yes"], check=True)
    finally:
        if os.path.exists(conf):
            os.remove(conf)
        os.rmdir(tmpdir)

    # Studio's music server: through the tunnel, with the new login.
    try:
        p = json.load(open(prefs_file))
    except FileNotFoundError:
        p = {}
    p.setdefault("server", {})
    p["server"].update({"url": server, "login": name, "secret": secret})
    os.makedirs(config, exist_ok=True)
    tmp = prefs_file + ".new"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    os.write(fd, (json.dumps(p, indent=2) + "\n").encode())
    os.close(fd)
    os.replace(tmp, prefs_file)
    if a.src:
        os.remove(a.src)

    up = subprocess.run(["nmcli", "connection", "up", CONNECTION], capture_output=True, text=True)
    print(f'NetworkManager: "{CONNECTION}" added (connects automatically){"" if up.returncode == 0 else ", but not up yet: " + up.stderr.strip()}')
    print(f"Tunnel address {field('Address')}, via {field('Endpoint')}; only {field('AllowedIPs')} goes through it")
    print(f"Studio: music server {server}, login {name}, new secret saved")
    if re.match(r"\d+\.\d+\.\d+\.\d+:", field("Endpoint")):
        print("Note: the endpoint is an IP address; if the home's IP changes, set the router's DNS name in")
        print(f"  /etc/NetworkManager/system-connections/{CONNECTION}.nmconnection (endpoint=), then nmcli connection reload")
    print("Check: localbeat server check")


if __name__ == "__main__":
    main()
