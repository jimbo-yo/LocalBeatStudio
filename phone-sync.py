#!/usr/bin/env python3
"""phone-sync.py: choose which albums go on the phone, within its free space.

Shows every album in the phone library on this computer (~/Music/phone, the
Opus copies made by flac2phone.sh) with a checkbox, plus a bar showing the
phone's storage: other apps, music, what's being added/removed, and a reserve
that's kept free for other apps. Sync makes the phone's music folder match the
checked albums: missing files are copied, unchecked albums are removed (after
a confirmation listing them). Only the phone's music folder is ever touched.

Connections (pick in the window, remembered in ~/.config/rip-cd/phone-sync.json):
  USB (adb)            phone plugged in with USB debugging on; needs
                       `sudo dnf install android-tools`
  Wi-Fi (KDE Connect)  phone paired in KDE Connect, with its storage shared

Albums already on the phone under an older folder name ("Artist - Album" vs
"Artist/Album", straight vs curly quotes) are recognised, not copied twice.

Testing: PHONE_SYNC_TEST_DIR=/some/dir PHONE_SYNC_TEST_SIZE=<bytes> adds a
"Test folder" connection that treats that folder as the phone.
"""

import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

from PySide6.QtCore import QRectF, Qt, QThread, QTimer, Signal
from PySide6.QtGui import QBrush, QColor, QIcon, QPainter, QPainterPath, QPen
from PySide6.QtWidgets import (
    QApplication, QComboBox, QDialog, QDialogButtonBox, QDoubleSpinBox,
    QFormLayout, QHBoxLayout, QHeaderView, QLabel, QLineEdit, QMainWindow,
    QMessageBox, QPlainTextEdit, QProgressDialog, QPushButton, QTreeWidget,
    QTreeWidgetItem, QVBoxLayout, QWidget,
)

CONFIG_FILE = Path.home() / ".config/rip-cd/phone-sync.json"
DEFAULTS = {
    "backend": "adb",
    "library": str(Path.home() / "Music/phone"),
    "adb_folder": "/sdcard/Music",
    "kdeconnect_device": "",
    "kdeconnect_folder": "Music",      # inside the phone's shared storage
    "reserve_gb": 5.0,
    "music_limit_gb": 0.0,             # used when the phone can't report free space
    "last_space": None,                # last real numbers (from USB), for Wi-Fi estimates
}
AUDIO = {".opus", ".m4a", ".mp3", ".flac", ".ogg", ".aac", ".wav"}
GB = 1e9

GREEN, RED, BLUE, GREY = "#2e9e44", "#d03838", "#3584e4", "#9a9996"


class SyncError(Exception):
    pass


def human(n):
    n = float(n)
    if abs(n) >= GB:
        return f"{n / GB:.1f} GB"
    if abs(n) >= 1e6:
        return f"{n / 1e6:.0f} MB"
    return f"{n / 1e3:.0f} KB"


def norm_key(rel):
    """Folder name -> matching key: 'Guns N’ Roses/X' == "Guns N' Roses - X"."""
    s = rel.lower()
    for a, b in (("’", "'"), ("‘", "'"), ("“", '"'), ("”", '"'), ("？", "?")):
        s = s.replace(a, b)
    s = re.sub(r"\s*/\s*|\s+-\s+", " - ", s)
    return re.sub(r"\s+", " ", s).strip()


def split_album(rel):
    """'Artist/Album' or 'Artist - Album' -> (artist, album)."""
    if "/" in rel:
        artist, album = rel.split("/", 1)
    elif " - " in rel:
        artist, album = rel.split(" - ", 1)
    else:
        artist, album = "(no artist folder)", rel
    return artist, album


def albums_from_files(files):
    """{relpath: size} -> {album folder: {filename: size}} (folders with audio)."""
    dirs = {}
    for rel, size in files.items():
        d, _, name = rel.rpartition("/")
        dirs.setdefault(d, {})[name] = size
    return {d: f for d, f in dirs.items()
            if d and any(os.path.splitext(n)[1].lower() in AUDIO for n in f)}


def scan_local(root):
    files = {}
    for dirpath, _, names in os.walk(root):
        for n in names:
            p = os.path.join(dirpath, n)
            if n.endswith(".part"):
                continue
            files[os.path.relpath(p, root)] = os.path.getsize(p)
    return files


# ---------------------------------------------------------------- connections
class FolderBackend:
    """A phone (or test folder) reachable as a normal folder."""
    label = "Folder"

    def __init__(self, root, fake_total=None):
        self.root = root
        self.fake_total = fake_total

    def connect(self):
        os.makedirs(self.root, exist_ok=True)
        return f"Test folder {self.root}"

    def space(self):
        if self.fake_total:      # test mode: pretend the disk is this size
            used = sum(scan_local(self.root).values())
            other = self.fake_total * 0.4
            return self.fake_total, self.fake_total - other - used
        st = os.statvfs(self.root)
        total, free = st.f_blocks * st.f_frsize, st.f_bavail * st.f_frsize
        # KDE Connect's SFTP doesn't report space, and sshfs then shows a
        # made-up "1000 GiB, all free"
        if total == 0 or total > 2 ** 50 or (free == total and total == 1000 * 2 ** 30):
            return None
        return total, free

    def list_files(self):
        return scan_local(self.root)

    def push(self, local, rel):
        dest = os.path.join(self.root, rel)
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        shutil.copyfile(local, dest + ".part")
        os.replace(dest + ".part", dest)

    def remove_album(self, rel):
        safe_rel(rel)
        shutil.rmtree(os.path.join(self.root, rel))
        # tidy an empty artist folder
        parent = os.path.dirname(os.path.join(self.root, rel))
        if parent != self.root.rstrip("/") and not os.listdir(parent):
            os.rmdir(parent)


def safe_rel(rel):
    if not rel or rel.startswith("/") or ".." in rel.split("/"):
        raise SyncError(f"Refusing to touch an odd path: {rel!r}")


class AdbBackend:
    label = "USB (adb)"

    def __init__(self, root):
        self.root = root.rstrip("/")
        self.serial = None

    def adb(self, *args, timeout=120):
        try:
            r = subprocess.run(["adb", *(["-s", self.serial] if self.serial else []), *args],
                               capture_output=True, text=True, timeout=timeout)
        except FileNotFoundError:
            raise SyncError("adb isn't installed. Run: sudo dnf install android-tools")
        except subprocess.TimeoutExpired:
            raise SyncError("The phone stopped responding over USB.")
        if r.returncode:
            raise SyncError((r.stderr or r.stdout).strip() or "adb failed")
        return r.stdout

    def shell(self, cmd):
        return self.adb("shell", cmd)

    def connect(self):
        self.serial = None
        lines = self.adb("devices").splitlines()[1:]
        devs = [l.split()[:2] for l in lines if l.strip()]
        ready = [d for d in devs if d[1] == "device"]
        if not ready:
            if any(d[1] == "unauthorized" for d in devs):
                raise SyncError("Unlock the phone and tap Allow on the 'Allow USB debugging?' prompt.")
            raise SyncError("No phone found over USB. Plug it in, and turn on USB debugging "
                            "(Settings > System > Developer options).")
        self.serial = ready[0][0]
        self.shell(f"mkdir -p {shlex.quote(self.root)}")
        model = self.shell("getprop ro.product.model").strip()
        return f"{model or self.serial} over USB, folder {self.root}"

    def space(self):
        out = self.shell(f"df -k {shlex.quote(self.root)}").strip().splitlines()
        f = out[-1].split()
        try:
            return int(f[1]) * 1024, int(f[3]) * 1024
        except (IndexError, ValueError):
            raise SyncError(f"Couldn't read the phone's free space: {out[-1]!r}")

    def list_files(self):
        q = shlex.quote(self.root)
        out = self.shell(f"find {q} -type f -exec stat -c '%s|%n' {{}} +")
        files = {}
        prefix = self.root + "/"
        for line in out.splitlines():
            size, _, path = line.partition("|")
            if path.startswith(prefix) and size.isdigit() and not path.endswith(".part"):
                files[path[len(prefix):]] = int(size)
        return files

    def push(self, local, rel):
        # adb push sends the path itself (no shell), so odd characters are fine;
        # it creates the folders it needs
        self.adb("push", local, f"{self.root}/{rel}", timeout=900)

    def remove_album(self, rel):
        safe_rel(rel)
        path = shlex.quote(f"{self.root}/{rel}")
        parent = shlex.quote(os.path.dirname(f"{self.root}/{rel}"))
        self.shell(f"rm -rf {path}; [ {parent} != {shlex.quote(self.root)} ] && "
                   f"rmdir {parent} 2>/dev/null; true")


class KdeConnectBackend(FolderBackend):
    """The phone's storage, mounted by KDE Connect (SFTP) as a local folder."""
    label = "Wi-Fi (KDE Connect)"

    def __init__(self, device, subfolder):
        super().__init__(root="")
        self.device, self.subfolder = device, subfolder

    @staticmethod
    def qdbus(path, method):
        exe = shutil.which("qdbus-qt6") or shutil.which("qdbus6") or shutil.which("qdbus")
        if not exe:
            raise SyncError("qdbus not found (package qt6-qttools)")
        r = subprocess.run([exe, "org.kde.kdeconnect", path, method],
                           capture_output=True, text=True, timeout=60)
        if r.returncode:
            raise SyncError(r.stderr.strip() or f"KDE Connect: {method} failed")
        return r.stdout.strip()

    def connect(self):
        r = subprocess.run(["kdeconnect-cli", "-a", "--id-name-only"],
                           capture_output=True, text=True, timeout=30)
        devs = [l.split(" ", 1) for l in r.stdout.splitlines() if l.strip()]
        if not devs:
            raise SyncError("No paired phone is reachable over Wi-Fi. Open KDE Connect on the "
                            "phone, check it's on the same Wi-Fi and paired with this computer.")
        dev = next((d for d in devs if d[0] == self.device), devs[0])
        self.device = dev[0]
        sftp = f"/modules/kdeconnect/devices/{dev[0]}/sftp"
        if self.qdbus(sftp, "org.kde.kdeconnect.device.sftp.mountAndWait") != "true":
            why = self.qdbus(sftp, "org.kde.kdeconnect.device.sftp.getMountError")
            hint = ""
            if "ermission" in why:
                hint = (" On the phone: open KDE Connect, tap this computer, and allow "
                        "Filesystem expose (or Settings > Apps > KDE Connect > Permissions > "
                        "Files: allow management of all files).")
            raise SyncError(f"KDE Connect couldn't open the phone's storage ({why or 'no reason given'})."
                            + hint)
        mount = self.qdbus(sftp, "org.kde.kdeconnect.device.sftp.mountPoint")
        # the shared storage is a folder inside the mount (e.g. .../storage/emulated/0)
        dirs = self.qdbus(sftp, "org.kde.kdeconnect.device.sftp.getDirectories")
        paths = [l.split(":", 1)[0].strip() for l in dirs.splitlines() if l.startswith("/")]
        base = paths[0] if paths else mount
        self.root = os.path.join(base, self.subfolder)
        os.makedirs(self.root, exist_ok=True)
        return f"{dev[1] if len(dev) > 1 else dev[0]} over Wi-Fi, folder {self.subfolder}"


# ---------------------------------------------------------------- background work
class Worker(QThread):
    """Runs fn(progress) in the background; progress(done, total, text)."""
    progressed = Signal(float, float, str)
    succeeded = Signal(object)
    failed = Signal(str)

    def __init__(self, fn, parent=None):
        super().__init__(parent)
        self.fn = fn
        self.cancelled = False

    def run(self):
        try:
            self.succeeded.emit(self.fn(self.progressed.emit))
        except SyncError as e:
            self.failed.emit(str(e))
        except Exception as e:          # show anything unexpected in the window
            self.failed.emit(f"{type(e).__name__}: {e}")


# ---------------------------------------------------------------- capacity bar
class CapacityBar(QWidget):
    def __init__(self):
        super().__init__()
        self.setMinimumHeight(30)
        self.total = 0
        self.segments = []          # (bytes, colour, pattern)

    def set(self, total, segments):
        self.total, self.segments = total, segments
        self.update()

    def paintEvent(self, _):
        p = QPainter(self)
        p.setRenderHint(QPainter.Antialiasing)
        r = QRectF(self.rect()).adjusted(1, 1, -1, -1)
        clip = QPainterPath()
        clip.addRoundedRect(r, 6, 6)
        p.setClipPath(clip)
        p.fillRect(r, self.palette().base())
        if self.total > 0:
            x = r.left()
            for size, colour, pattern in self.segments:
                w = r.width() * max(size, 0) / self.total
                seg = QRectF(x, r.top(), w, r.height())
                p.fillRect(seg, QColor(colour))
                if pattern is not None:
                    p.fillRect(seg, QBrush(QColor(255, 255, 255, 140), pattern))
                x += w
        p.setClipping(False)
        p.setPen(QPen(self.palette().mid().color()))
        p.drawRoundedRect(r, 6, 6)


# ---------------------------------------------------------------- window
class SettingsDialog(QDialog):
    def __init__(self, cfg, parent):
        super().__init__(parent)
        self.setWindowTitle("Phone Sync settings")
        form = QFormLayout(self)
        self.library = QLineEdit(cfg["library"])
        self.adb_folder = QLineEdit(cfg["adb_folder"])
        self.kdc_folder = QLineEdit(cfg["kdeconnect_folder"])
        self.reserve = QDoubleSpinBox(suffix=" GB", decimals=1, maximum=500, value=cfg["reserve_gb"])
        self.limit = QDoubleSpinBox(suffix=" GB", decimals=0, maximum=2000,
                                    value=cfg.get("music_limit_gb", 0), specialValueText="none")
        form.addRow("Music library on this computer:", self.library)
        form.addRow("Music folder on the phone (USB):", self.adb_folder)
        form.addRow("Music folder on the phone (Wi-Fi):", self.kdc_folder)
        form.addRow("Always leave free on the phone:", self.reserve)
        form.addRow("Music limit, if free space is unknown:", self.limit)
        form.addRow(QLabel("The phone folder is the one your music player (Musicolet) reads.\n"
                           "Phone Sync only ever adds and removes albums inside it."))
        bb = QDialogButtonBox(QDialogButtonBox.Ok | QDialogButtonBox.Cancel)
        bb.accepted.connect(self.accept)
        bb.rejected.connect(self.reject)
        form.addRow(bb)

    def values(self):
        return {"library": self.library.text().strip(),
                "adb_folder": self.adb_folder.text().strip() or DEFAULTS["adb_folder"],
                "kdeconnect_folder": self.kdc_folder.text().strip().strip("/") or "Music",
                "reserve_gb": self.reserve.value(),
                "music_limit_gb": self.limit.value()}


class Album:
    def __init__(self, key):
        self.key = key
        self.local_rel = self.phone_rel = None
        self.local_files = {}      # name -> size
        self.phone_files = {}

    @property
    def rel(self):
        return self.local_rel or self.phone_rel

    def local_size(self):
        return sum(self.local_files.values())

    def phone_size(self):
        return sum(self.phone_files.values())

    def missing_on_phone(self):
        return {n: s for n, s in self.local_files.items() if self.phone_files.get(n) != s}


class SyncWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self.cfg = dict(DEFAULTS)
        try:
            self.cfg.update(json.loads(CONFIG_FILE.read_text()))
        except (OSError, ValueError):
            pass
        self.backend = None
        self.space = None          # (total, free); None when the phone won't say
        self.is_connected = False
        self.space_note = ""
        self.albums = {}
        self.worker = None
        self.building = False

        self.setWindowTitle("Phone Sync")
        self.setWindowIcon(QIcon.fromTheme("smartphone"))
        self.resize(900, 760)
        self.build_ui()
        self.load_library()
        QTimer.singleShot(100, self.connect_phone)

    def save_cfg(self):
        CONFIG_FILE.parent.mkdir(parents=True, exist_ok=True)
        CONFIG_FILE.write_text(json.dumps(self.cfg, indent=2))

    # ---------- layout
    def build_ui(self):
        top = QHBoxLayout()
        self.conn = QComboBox()
        self.conn.addItem("USB (adb)", "adb")
        self.conn.addItem("Wi-Fi (KDE Connect)", "kdeconnect")
        if os.environ.get("PHONE_SYNC_TEST_DIR"):
            self.conn.addItem("Test folder", "test")
        i = self.conn.findData(self.cfg["backend"])
        self.conn.setCurrentIndex(max(i, 0))
        self.conn.currentIndexChanged.connect(self.connection_changed)
        self.status = QLabel()
        self.status.setWordWrap(True)
        refresh = QPushButton(QIcon.fromTheme("view-refresh"), "Connect / refresh")
        refresh.clicked.connect(self.connect_phone)
        settings = QPushButton(QIcon.fromTheme("configure"), "Settings")
        settings.clicked.connect(self.edit_settings)
        top.addWidget(QLabel("Phone:"))
        top.addWidget(self.conn)
        top.addWidget(self.status, 1)
        top.addWidget(refresh)
        top.addWidget(settings)

        self.bar = CapacityBar()
        self.legend = QLabel()
        self.legend.setTextFormat(Qt.RichText)
        self.summary = QLabel()
        f = self.summary.font(); f.setBold(True); self.summary.setFont(f)

        self.search = QLineEdit(placeholderText="Search artists and albums…", clearButtonEnabled=True)
        self.search.textChanged.connect(self.apply_filter)

        self.tree = QTreeWidget()
        self.tree.setHeaderLabels(["Album", "Size", "Status"])
        self.tree.setUniformRowHeights(True)
        h = self.tree.header()
        h.setSectionResizeMode(0, QHeaderView.Stretch)
        h.setSectionResizeMode(1, QHeaderView.ResizeToContents)
        h.setSectionResizeMode(2, QHeaderView.ResizeToContents)
        self.tree.itemChanged.connect(self.item_changed)

        buttons = QHBoxLayout()
        reset = QPushButton("Undo changes")
        reset.setToolTip("Set the checkboxes back to what's on the phone now")
        reset.clicked.connect(self.check_phone_state)
        expand = QPushButton("Expand all")
        expand.clicked.connect(self.tree.expandAll)
        collapse = QPushButton("Collapse all")
        collapse.clicked.connect(self.tree.collapseAll)
        self.sync_btn = QPushButton(QIcon.fromTheme("folder-sync"), "Sync to phone")
        self.sync_btn.clicked.connect(self.sync)
        for w in (reset, expand, collapse):
            buttons.addWidget(w)
        buttons.addStretch()
        buttons.addWidget(self.sync_btn)

        root = QVBoxLayout()
        root.addLayout(top)
        root.addWidget(self.bar)
        root.addWidget(self.legend)
        root.addWidget(self.summary)
        root.addWidget(self.search)
        root.addWidget(self.tree, 1)
        root.addLayout(buttons)
        w = QWidget(); w.setLayout(root)
        self.setCentralWidget(w)

    # ---------- library + phone contents
    def load_library(self):
        lib = self.cfg["library"]
        self.albums = {}
        for rel, files in albums_from_files(scan_local(lib) if os.path.isdir(lib) else {}).items():
            a = self.albums.setdefault(norm_key(rel), Album(norm_key(rel)))
            a.local_rel, a.local_files = rel, files
        self.rebuild_tree()

    def merge_phone(self, files):
        for a in list(self.albums.values()):
            a.phone_rel, a.phone_files = None, {}
            if not a.local_rel:
                del self.albums[a.key]
        for rel, f in albums_from_files(files).items():
            a = self.albums.setdefault(norm_key(rel), Album(norm_key(rel)))
            a.phone_rel, a.phone_files = rel, f

    def make_backend(self):
        kind = self.conn.currentData()
        if kind == "adb":
            return AdbBackend(self.cfg["adb_folder"])
        if kind == "kdeconnect":
            return KdeConnectBackend(self.cfg["kdeconnect_device"], self.cfg["kdeconnect_folder"])
        return FolderBackend(os.environ["PHONE_SYNC_TEST_DIR"],
                             float(os.environ.get("PHONE_SYNC_TEST_SIZE", 64 * GB)))

    def connection_changed(self):
        self.cfg["backend"] = self.conn.currentData()
        self.save_cfg()
        self.connect_phone()

    def connect_phone(self):
        if self.worker and self.worker.isRunning():
            return
        self.backend = None
        self.space = None
        self.is_connected = False
        self.status.setText("Connecting…")
        self.status.setStyleSheet("")
        backend = self.make_backend()

        def job(_progress):
            desc = backend.connect()
            return backend, desc, backend.space(), backend.list_files()
        self.run(job, self.connected, self.connect_failed)

    def connected(self, result):
        self.backend, desc, self.space, files = result
        if isinstance(self.backend, KdeConnectBackend):
            self.cfg["kdeconnect_device"] = self.backend.device
            self.save_cfg()
        self.status.setText(f"Connected: {desc}")
        self.is_connected = True
        self.load_library()
        self.merge_phone(files)
        music = sum(a.phone_size() for a in self.albums.values() if a.phone_rel)
        if self.space and isinstance(self.backend, AdbBackend):
            self.cfg["last_space"] = {"total": self.space[0], "free": self.space[1],
                                      "music": music, "when": time.strftime("%Y-%m-%d")}
            self.save_cfg()
        self.space_note = ""
        if self.space is None:
            last = self.cfg.get("last_space")
            limit = self.cfg.get("music_limit_gb", 0) * GB
            if last:
                # other apps' use is assumed unchanged since that USB check
                self.space = (last["total"], last["free"] + last["music"] - music)
                self.space_note = (f"Free space estimated from the USB check on {last['when']} "
                                   "(the phone doesn't report it over Wi-Fi).")
            elif limit:
                self.space = (limit, limit - music)
                self.space_note = ("The phone doesn't report free space over Wi-Fi, so the bar "
                                   f"shows your {human(limit)} music limit (Settings).")
            else:
                self.space_note = ("The phone doesn't report free space over Wi-Fi. Connect once "
                                   "by USB, or set a music limit in Settings.")
        self.rebuild_tree()

    def connect_failed(self, msg):
        self.status.setText(msg)
        self.status.setStyleSheet(f"color: {RED}")
        self.load_library()

    def run(self, fn, on_ok, on_fail, progress=None):
        self.worker = Worker(fn, self)
        self.worker.succeeded.connect(on_ok)
        self.worker.failed.connect(on_fail)
        if progress:
            self.worker.progressed.connect(progress)
        self.worker.start()

    # ---------- tree
    def rebuild_tree(self):
        self.building = True
        self.tree.clear()
        by_artist = {}
        phone_only = []
        names = {}                 # one group per artist, whatever quote style
        for a in self.albums.values():
            if a.local_rel:
                artist = split_album(a.local_rel)[0]
                names.setdefault(norm_key(artist), artist)
                by_artist.setdefault(norm_key(artist), []).append(a)
            else:
                phone_only.append(a)
        groups = sorted(((names[k], v) for k, v in by_artist.items()),
                        key=lambda kv: kv[0].lower().removeprefix("the "))
        if phone_only:
            groups.append(("On the phone, not in your library", phone_only))
        for artist, albums in groups:
            top = QTreeWidgetItem(self.tree, [artist])
            top.setFlags(top.flags() | Qt.ItemIsAutoTristate | Qt.ItemIsUserCheckable)
            for a in sorted(albums, key=lambda a: a.rel.lower()):
                name = split_album(a.rel)[1] if a.local_rel else a.phone_rel
                it = QTreeWidgetItem(top, [name])
                it.setData(0, Qt.UserRole, a.key)
                it.setFlags(it.flags() | Qt.ItemIsUserCheckable)
                it.setCheckState(0, Qt.Checked if a.phone_rel else Qt.Unchecked)
                if a.phone_rel and a.local_rel and norm_key(a.phone_rel) == a.key \
                        and a.phone_rel != a.local_rel:
                    it.setToolTip(0, f"On the phone as “{a.phone_rel}”")
        self.building = False
        self.apply_filter()
        self.refresh()

    def album_items(self):
        for i in range(self.tree.topLevelItemCount()):
            top = self.tree.topLevelItem(i)
            for j in range(top.childCount()):
                yield top, top.child(j)

    def check_phone_state(self):
        self.building = True
        for _, it in self.album_items():
            a = self.albums[it.data(0, Qt.UserRole)]
            it.setCheckState(0, Qt.Checked if a.phone_rel else Qt.Unchecked)
        self.building = False
        self.refresh()

    def item_changed(self, *_):
        if not self.building:
            self.building = True       # status/size updates below change items too
            QTimer.singleShot(0, self.refresh)

    def apply_filter(self):
        text = self.search.text().strip().lower()
        for i in range(self.tree.topLevelItemCount()):
            top = self.tree.topLevelItem(i)
            artist_hit = text in top.text(0).lower()
            any_shown = False
            for j in range(top.childCount()):
                c = top.child(j)
                show = not text or artist_hit or text in c.text(0).lower()
                c.setHidden(not show)
                any_shown |= show
            top.setHidden(not any_shown)
            if text and any_shown:
                top.setExpanded(True)

    # ---------- the plan: what Sync would do
    def plan(self):
        pushes, removes = [], []        # (local path, phone rel, size), (album, size)
        lib = self.cfg["library"]
        for _, it in self.album_items():
            a = self.albums[it.data(0, Qt.UserRole)]
            want = it.checkState(0) == Qt.Checked
            if want and a.local_rel:
                dest = a.phone_rel or a.local_rel
                for name, size in a.missing_on_phone().items():
                    pushes.append((os.path.join(lib, a.local_rel, name), f"{dest}/{name}", size))
            elif not want and a.phone_rel:
                removes.append((a, a.phone_size()))
        return pushes, removes

    def refresh(self):
        self.building = True
        pushes, removes = self.plan()
        add_by_album = {}
        for local, rel, size in pushes:
            add_by_album[rel.rpartition("/")[0]] = add_by_album.get(rel.rpartition("/")[0], 0) + size
        removing = {a.key for a, _ in removes}
        for top, it in self.album_items():
            a = self.albums[it.data(0, Qt.UserRole)]
            want = it.checkState(0) == Qt.Checked
            dest = a.phone_rel or a.local_rel
            if not self.is_connected:
                status, colour = ("on phone" if a.phone_rel else ""), None
            elif a.key in removing:
                status, colour = "will be removed", RED
            elif want and not a.phone_rel:
                status, colour = "will be added", GREEN
            elif want and dest in add_by_album:
                status, colour = f"will update ({human(add_by_album[dest])})", BLUE
            elif a.phone_rel:
                status, colour = "on phone", None
            else:
                status, colour = "", None
            it.setText(1, human(a.local_size() or a.phone_size()))
            it.setText(2, status)
            it.setForeground(2, QColor(colour) if colour else self.palette().text().color())
        for i in range(self.tree.topLevelItemCount()):
            top = self.tree.topLevelItem(i)
            kids = [top.child(j) for j in range(top.childCount())]
            total = sum(self.albums[c.data(0, Qt.UserRole)].local_size()
                        or self.albums[c.data(0, Qt.UserRole)].phone_size() for c in kids)
            on = sum(c.checkState(0) == Qt.Checked for c in kids)
            top.setText(1, human(total))
            top.setText(2, f"{on} of {len(kids)} albums" if len(kids) > 1 else "")
        self.building = False
        self.update_capacity(pushes, removes)

    def update_capacity(self, pushes, removes):
        add = sum(p[2] for p in pushes)
        rem = sum(r[1] for r in removes)
        reserve = self.cfg["reserve_gb"] * GB
        self.bar.setVisible(bool(self.space))
        if not self.space:
            self.bar.set(0, [])
            self.legend.setText(self.space_note)
            self.summary.setText(f"Selected changes: +{human(add)} / −{human(rem)}"
                                 + ("" if self.is_connected else
                                    " (connect the phone to see its space)"))
            self.summary.setStyleSheet("")
            self.sync_btn.setEnabled(self.is_connected and bool(pushes or removes))
            return
        total, free = self.space
        music = sum(a.phone_size() for a in self.albums.values() if a.phone_rel)
        other = max(total - free - music, 0)
        free_after = free - add + rem
        room = free_after - reserve
        stay = music - rem
        segs = [(other, GREY, None), (stay, BLUE, None), (rem, RED, Qt.BDiagPattern),
                (add, GREEN, None)]
        res_seg = min(reserve, max(free_after, 0))
        segs.append((res_seg, "#c0bfbc", Qt.Dense5Pattern))
        self.bar.set(total, segs)

        def sw(c, pat=False):
            return (f"<span style='background:{c};color:{c}'>{'░' if pat else '█'}█</span>")
        self.legend.setText(
            f"{sw(GREY)} other apps {human(other)} &nbsp; {sw(BLUE)} music {human(stay)} &nbsp; "
            f"{sw(GREEN)} adding {human(add)} &nbsp; {sw(RED)} removing {human(rem)} &nbsp; "
            f"{sw('#c0bfbc')} reserve {human(reserve)} &nbsp; of {human(total)}"
            + (f"<br><i>{self.space_note}</i>" if self.space_note else ""))
        if room < 0 and add > 0:
            self.summary.setText(f"Too much selected: {human(-room)} over "
                                 f"(keeping {human(reserve)} free for other apps).")
            self.summary.setStyleSheet(f"color: {RED}")
        else:
            self.summary.setText(f"After sync: {human(free_after)} free on the phone, "
                                 f"room for about {human(max(room, 0))} more music.")
            self.summary.setStyleSheet("")
        changes = bool(pushes or removes)
        self.sync_btn.setEnabled(changes and not (room < 0 and add > 0))

    # ---------- settings
    def edit_settings(self):
        d = SettingsDialog(self.cfg, self)
        if d.exec() == QDialog.Accepted:
            self.cfg.update(d.values())
            self.save_cfg()
            self.connect_phone()

    # ---------- sync
    def sync(self):
        pushes, removes = self.plan()
        if not (pushes or removes):
            return
        add_albums = sorted({p[1].rpartition("/")[0] for p in pushes})
        lines = []
        if removes:
            lines.append(f"Remove {len(removes)} album(s) from the phone:")
            lines += [f"   − {a.phone_rel}" for a, _ in sorted(removes, key=lambda r: r[0].phone_rel)]
        if add_albums:
            lines.append(f"Copy {human(sum(p[2] for p in pushes))} to the phone "
                         f"({len(add_albums)} album(s)):")
            lines += [f"   + {r}" for r in add_albums]
        box = QDialog(self)
        box.setWindowTitle("Sync to phone?")
        lay = QVBoxLayout(box)
        text = QPlainTextEdit("\n".join(lines), readOnly=True)
        lay.addWidget(text)
        bb = QDialogButtonBox()
        bb.addButton("Sync", QDialogButtonBox.AcceptRole)
        bb.addButton(QDialogButtonBox.Cancel)
        bb.accepted.connect(box.accept)
        bb.rejected.connect(box.reject)
        lay.addWidget(bb)
        box.resize(640, 420)
        if box.exec() != QDialog.Accepted:
            return

        backend = self.backend
        total = float(sum(p[2] for p in pushes) + 1)
        dlg = QProgressDialog("Syncing…", "Stop", 0, 1000, self)
        dlg.setWindowTitle("Phone Sync")
        dlg.setWindowModality(Qt.WindowModal)
        dlg.setMinimumDuration(0)
        dlg.setMinimumWidth(520)

        def job(progress):
            for a, _ in removes:
                if self.worker.cancelled:
                    return "stopped"
                progress(0, total, f"Removing {a.phone_rel}")
                backend.remove_album(a.phone_rel)
            done = 0
            for local, rel, size in pushes:
                if self.worker.cancelled:
                    return "stopped"
                progress(done, total, f"Copying {rel}")
                backend.push(local, rel)
                done += size
            progress(total, total, "Checking the phone…")
            return "done"

        def on_progress(done, tot, text):
            dlg.setLabelText(text)
            dlg.setValue(int(1000 * done / tot))

        def finished(result):
            dlg.close()
            self.connect_phone()
            if result == "stopped":
                self.status.setText("Sync stopped; what was copied so far is on the phone.")

        def failed(msg):
            dlg.close()
            QMessageBox.warning(self, "Phone Sync", f"Sync stopped with an error:\n\n{msg}")
            self.connect_phone()

        self.run(job, finished, failed, on_progress)
        dlg.canceled.connect(lambda: setattr(self.worker, "cancelled", True))


def main():
    app = QApplication(sys.argv)
    app.setApplicationName("Phone Sync")
    app.setDesktopFileName("phone-sync")
    w = SyncWindow()
    w.show()
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
