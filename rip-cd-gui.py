#!/usr/bin/env python3
"""rip-cd-gui.py: a desktop window for rip-cd.sh.

Runs `rip-cd.sh --loop` (with RIPCD_EVENTS=1) and shows what it's doing: the
album and cover, every track's status, the current read with an ETA, and the
log. All the ripping logic stays in rip-cd.sh; this window only starts and
stops it and displays the "@@event" lines it prints (plus cyanrip's progress
lines). The full output is also saved to ~/rip.log, as with `| tee ~/rip.log`.

Usage: rip-cd-gui.py [--no-start] [rip-cd.sh options, e.g. -R <mbid>]
Only one window runs at a time; starting it again brings it to the front.
"""

import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

from PySide6.QtCore import QProcess, QProcessEnvironment, Qt, QTimer, QUrl
from PySide6.QtGui import QColor, QDesktopServices, QFont, QFontDatabase, QIcon, QPixmap
from PySide6.QtNetwork import QLocalServer, QLocalSocket
from PySide6.QtWidgets import (
    QApplication, QCheckBox, QFrame, QHBoxLayout, QHeaderView, QLabel,
    QListWidget, QMainWindow, QMessageBox, QPlainTextEdit, QProgressBar,
    QPushButton, QSplitter, QTableWidget, QTableWidgetItem, QVBoxLayout, QWidget,
)

HERE = Path(__file__).resolve().parent
SCRIPT = Path(os.environ.get("RIPCD_SCRIPT", HERE / "rip-cd.sh"))   # override for testing
LOG_FILE = Path.home() / "rip.log"
INSTANCE = f"rip-cd-gui-{os.getuid()}"

PROGRESS_RE = re.compile(r"^Ripping(?: and encoding)? track (\d+), progress - ([\d.]+)%")
RIPPED_RE = re.compile(r"^Track (\d+) ripped and encoded")
ACCURIP_RE = re.compile(r"^\s+Accurip v[12]:\s+\S+ \((accurately ripped|not found)")

GREEN, ORANGE, RED, BLUE = "#2e9e44", "#d4831a", "#d03838", "#3584e4"

# track state -> (status text, colour or None for the normal text colour)
STATES = {
    "have":     ("✓ Already in library", None),
    "todo":     ("Waiting", None),
    "queued":   ("Waiting to read", None),
    "reading":  ("Reading…", BLUE),
    "ar-match": ("AccurateRip match ✓ (moves to library after this pass)", GREEN),
    "ar-miss":  ("No AccurateRip match: will re-read to compare", ORANGE),
    "readerr":  ("Read errors on this read", RED),
    "read":     ("Read, checking…", None),
    "retry":    ("Will re-read", ORANGE),
    "ok-ar":    ("✔ AccurateRip verified", GREEN),
    "ok-two":   ("✔ Two identical reads (no AccurateRip match)", GREEN),
    "rejected": ("✘ Couldn't read reliably (see rejected folder)", RED),
}
RESULT_STATE = {"verified": "ar-match", "unverified": "retry",
                "inconsistent": "retry", "error": "readerr"}


def seconds(dur):
    """'05:34.50' -> 334.5"""
    try:
        m, s = dur.split(":")
        return int(m) * 60 + float(s)
    except ValueError:
        return 0.0


def mmss(secs):
    secs = int(secs)
    h, rem = divmod(secs, 3600)
    return f"{h}:{rem // 60:02d}:{rem % 60:02d}" if h else f"{rem // 60}:{rem % 60:02d}"


class Track:
    def __init__(self, num, dur, title, state):
        self.num, self.dur, self.title, self.state = num, seconds(dur), title, state
        self.dur_text = dur.split(".")[0]
        self.reads = 0
        self.pct = 0.0


class RipWindow(QMainWindow):
    def __init__(self, extra_args):
        super().__init__()
        self.extra_args = extra_args
        self.proc = None
        self.buf = ""
        self.logf = None
        self.reset_disc()
        self.stopping = False
        self.discs_done = 0

        self.setWindowTitle("CD Ripper")
        self.setWindowIcon(QIcon.fromTheme("media-optical-audio"))
        self.resize(880, 860)
        self.build_ui()

        self.clock = QTimer(self, interval=1000, timeout=self.tick)
        self.clock.start()
        self.cover_poll = QTimer(self, interval=3000, timeout=self.look_for_cover)

    # ---------- layout ----------
    def build_ui(self):
        top = QHBoxLayout()
        self.cover = QLabel(alignment=Qt.AlignCenter)
        self.cover.setFixedSize(180, 180)
        self.cover.setFrameShape(QFrame.StyledPanel)
        top.addWidget(self.cover)

        info = QVBoxLayout()
        self.artist = QLabel()
        f = self.artist.font(); f.setPointSizeF(f.pointSizeF() * 1.7); f.setBold(True)
        self.artist.setFont(f)
        self.album = QLabel()
        f = self.album.font(); f.setPointSizeF(f.pointSizeF() * 1.35)
        self.album.setFont(f)
        self.detail = QLabel(); self.detail.setWordWrap(True)
        self.stage = QLabel(); self.stage.setWordWrap(True)
        self.pressing_note = QLabel(
            "No track has matched AccurateRip so far. This is most likely a different "
            "pressing from the ones in its database (common with box sets and reissues), "
            "not a bad rip. Each track is read again instead, and kept when two reads match.")
        self.pressing_note.setWordWrap(True)
        self.pressing_note.setStyleSheet(f"color: {ORANGE}")
        self.pressing_note.hide()
        f = self.stage.font(); f.setPointSizeF(f.pointSizeF() * 1.15); f.setBold(True)
        self.stage.setFont(f)
        for w in (self.artist, self.album, self.detail):
            w.setTextInteractionFlags(Qt.TextSelectableByMouse)
            info.addWidget(w)
        info.addStretch()
        info.addWidget(self.pressing_note)
        info.addWidget(self.stage)
        top.addLayout(info, 1)

        self.track_bar = QProgressBar(format="Current track: %p%")
        self.pass_bar = QProgressBar(format="")
        self.pass_bar.setTextVisible(True)

        self.table = QTableWidget(0, 5)
        self.table.setHorizontalHeaderLabels(["#", "Title", "Length", "Status", "Reads"])
        self.table.verticalHeader().hide()
        self.table.verticalHeader().setDefaultSectionSize(self.fontMetrics().height() + 8)
        self.table.setEditTriggers(QTableWidget.NoEditTriggers)
        self.table.setSelectionMode(QTableWidget.NoSelection)
        self.table.setFocusPolicy(Qt.NoFocus)
        h = self.table.horizontalHeader()
        for col, mode in enumerate([QHeaderView.ResizeToContents, QHeaderView.Stretch,
                                    QHeaderView.ResizeToContents, QHeaderView.Stretch,
                                    QHeaderView.ResizeToContents]):
            h.setSectionResizeMode(col, mode)

        self.session = QListWidget()
        self.log = QPlainTextEdit(readOnly=True)
        self.log.setMaximumBlockCount(5000)
        self.log.setFont(QFontDatabase.systemFont(QFontDatabase.FixedFont))
        self.log.hide()

        buttons = QHBoxLayout()
        self.start_btn = QPushButton(QIcon.fromTheme("media-playback-start"), "Start")
        self.start_btn.clicked.connect(self.start_stop)
        self.eject_btn = QPushButton(QIcon.fromTheme("media-eject"), "Eject")
        self.eject_btn.clicked.connect(self.eject)
        self.folder_btn = QPushButton(QIcon.fromTheme("folder-music"), "Open album folder")
        self.folder_btn.clicked.connect(
            lambda: QDesktopServices.openUrl(QUrl.fromLocalFile(self.album_dir)))
        self.unknown = QCheckBox("Disc isn't in MusicBrainz (generic names)")
        self.unknown.setToolTip("Rips with --unknown. Takes effect when you press Start.")
        self.log_btn = QPushButton("Show log", checkable=True)
        self.log_btn.toggled.connect(self.toggle_log)
        for w in (self.start_btn, self.eject_btn, self.folder_btn, self.unknown):
            buttons.addWidget(w)
        buttons.addStretch()
        buttons.addWidget(self.log_btn)

        lower = QSplitter(Qt.Vertical)
        lower.addWidget(self.table)
        self.sess = QWidget(); sl = QVBoxLayout(self.sess); sl.setContentsMargins(0, 0, 0, 0)
        sl.addWidget(QLabel("Discs this session:")); sl.addWidget(self.session)
        self.sess.setMaximumHeight(130)
        self.sess.hide()                  # until the first disc is done
        lower.addWidget(self.log)
        lower.setStretchFactor(0, 3)
        lower.setStretchFactor(1, 2)

        root = QVBoxLayout()
        root.addLayout(top)
        root.addWidget(self.track_bar)
        root.addWidget(self.pass_bar)
        root.addWidget(lower, 1)
        root.addWidget(self.sess)
        root.addLayout(buttons)
        central = QWidget(); central.setLayout(root)
        self.setCentralWidget(central)
        self.refresh_header()
        self.set_stage("Press Start, then insert a disc.")
        self.update_buttons()

    def toggle_log(self, on):
        self.log.setVisible(on)
        self.log_btn.setText("Hide log" if on else "Show log")

    # ---------- per-disc state ----------
    def reset_disc(self):
        self.tracks = {}
        self.artist_name = self.album_name = self.release = ""
        self.disc_no = self.discs = 1
        self.album_dir = self.stage_dir = ""
        self.device = getattr(self, "device", "")
        self.cover_path = ""
        self.ar_note = ""
        self.offset = ""
        self.ar_hits = self.ar_misses = 0   # AccurateRip results on the first pass
        self.disc_start = None
        self.pass_label = ""
        self.pass_tracks = []
        self.pass_start = None
        self.cur = None            # track cyanrip is reading / reporting on
        self.finished = None       # ("ok"|"missing"|"error", detail)
        self.finish_msg = ("", None)  # stage text + colour to keep showing after the disc
        self.in_disc = False

    def refresh_header(self):
        self.artist.setText(self.artist_name or "No disc")
        self.album.setText(self.album_name)
        bits = []
        if self.album_name:
            if self.discs > 1:
                bits.append(f"Disc {self.disc_no} of {self.discs}")
            if self.tracks:
                n = len(self.tracks)
                have = sum(t.state in ("have", "ok-ar", "ok-two") for t in self.tracks.values())
                bits.append(f"{have} of {n} tracks in library")
        if self.release:
            bits.append(self.release)
        if self.device:
            bits.append(self.device + (f" (offset {int(self.offset):+d})" if self.offset else ""))
        if self.ar_note:
            bits.append(self.ar_note)
        self.pressing_note.setVisible(self.ar_misses >= 3 and self.ar_hits == 0)
        if self.disc_start:
            bits.append(f"elapsed {mmss(time.time() - self.disc_start)}")
        self.detail.setText(" · ".join(bits))
        if not self.cover_path:
            self.cover.setPixmap(QIcon.fromTheme("media-optical-audio").pixmap(96, 96))

    def set_cover(self, path):
        pm = QPixmap(path)
        if pm.isNull():
            return
        self.cover_path = path
        self.cover_poll.stop()
        self.cover.setPixmap(pm.scaled(self.cover.size(), Qt.KeepAspectRatio,
                                       Qt.SmoothTransformation))

    def look_for_cover(self):
        for d in (self.stage_dir, self.album_dir):
            for name in ("Front.jpg", "Front.png", "cover.jpg", "cover.png"):
                if d and os.path.isfile(os.path.join(d, name)):
                    self.set_cover(os.path.join(d, name))
                    return

    def set_stage(self, text, colour=None):
        self.stage.setText(text)
        self.stage.setStyleSheet(f"color: {colour}" if colour else "")

    # ---------- track table ----------
    def rebuild_table(self):
        self.table.setRowCount(len(self.tracks))
        for row, t in enumerate(sorted(self.tracks.values(), key=lambda t: t.num)):
            t.row = row
            for col, text in enumerate([str(t.num), t.title, t.dur_text, "", ""]):
                item = QTableWidgetItem(text)
                if col in (0, 2, 4):
                    item.setTextAlignment(Qt.AlignRight | Qt.AlignVCenter)
                self.table.setItem(row, col, item)
            self.show_track(t)

    def show_track(self, t):
        if not hasattr(t, "row"):
            return
        text, colour = STATES.get(t.state, (t.state, None))
        if t.state == "reading":
            text = f"Reading… {t.pct:.0f}%"
        item = self.table.item(t.row, 3)
        item.setText(text)
        item.setForeground(QColor(colour) if colour else self.palette().text().color())
        f = item.font(); f.setBold(t.state in ("ok-ar", "ok-two", "rejected", "reading"))
        item.setFont(f)
        self.table.item(t.row, 4).setText(str(t.reads) if t.reads else "")

    def set_state(self, num, state):
        t = self.tracks.get(num)
        if t:
            t.state = state
            self.show_track(t)

    # ---------- progress ----------
    def update_pass_bar(self):
        if not self.pass_tracks:
            return
        total = sum(self.tracks[n].dur for n in self.pass_tracks if n in self.tracks) or 1
        done = 0.0
        finished = 0
        for n in self.pass_tracks:
            t = self.tracks.get(n)
            if not t:
                continue
            if t.state == "reading":
                done += t.dur * t.pct / 100
            elif t.state not in ("queued", "retry", "todo"):
                done += t.dur
                finished += 1
        frac = min(done / total, 1.0)
        self.pass_bar.setRange(0, 1000)
        self.pass_bar.setValue(int(frac * 1000))
        text = f"{self.pass_label}: {finished} of {len(self.pass_tracks)} tracks read"
        if self.pass_start and frac > 0.02:
            elapsed = time.time() - self.pass_start
            text += f" · about {mmss(elapsed / frac - elapsed)} left in this pass"
        self.pass_bar.setFormat(text)

    def tick(self):
        if self.in_disc:
            self.refresh_header()
            self.update_pass_bar()

    # ---------- the rip-cd.sh process ----------
    def start_stop(self):
        if self.proc:
            self.stop()
        else:
            self.start()

    def start(self):
        others = subprocess.run(["pgrep", "-x", "cyanrip"], capture_output=True, text=True)
        if others.stdout.strip():
            r = QMessageBox.question(
                self, "CD Ripper",
                "cyanrip is already running (a rip in a terminal?).\n\n"
                "The window will only use drives that rip isn't using; an older rip-cd.sh "
                "doesn't lock its drive, though. Start anyway?")
            if r != QMessageBox.Yes:
                return
        args = ["--loop"] + (["--unknown"] if self.unknown.isChecked() else []) + self.extra_args
        self.proc = QProcess(self)
        env = QProcessEnvironment.systemEnvironment()
        env.insert("RIPCD_EVENTS", "1")
        env.insert("PATH", f"{Path.home()}/.local/bin:{env.value('PATH')}")
        self.proc.setProcessEnvironment(env)
        self.proc.setProcessChannelMode(QProcess.MergedChannels)
        # own session/process group, so Stop also stops cyanrip
        params = QProcess.UnixProcessParameters()
        params.flags = QProcess.UnixProcessFlag.CreateNewSession
        self.proc.setUnixProcessParameters(params)
        self.proc.readyReadStandardOutput.connect(self.read_output)
        self.proc.finished.connect(self.proc_finished)
        self.logf = open(LOG_FILE, "wb")
        self.buf = ""
        self.stopping = False
        self.proc.start(str(SCRIPT), args)
        if not self.proc.waitForStarted(5000):
            self.set_stage(f"Couldn't start {SCRIPT}", RED)
            self.proc = None
        self.update_buttons()

    def stop(self):
        if self.in_disc:
            r = QMessageBox.question(
                self, "CD Ripper",
                "Stop the rip in progress? Tracks already moved to the library are kept; "
                "the rest of this disc is not ripped.")
            if r != QMessageBox.Yes:
                return False
        self.signal_rip(signal.SIGTERM)
        self.stopping = True
        QTimer.singleShot(6000, lambda: self.signal_rip(signal.SIGKILL))
        return True

    def signal_rip(self, sig):
        if self.proc and self.proc.processId():
            try:
                os.killpg(self.proc.processId(), sig)
            except (ProcessLookupError, PermissionError):
                pass

    def proc_finished(self, code, _status):
        self.read_output()
        if self.logf:
            self.logf.close()
            self.logf = None
        self.proc = None
        self.in_disc = False
        self.track_bar.setValue(0)
        self.set_stage("Stopped." if self.stopping else f"rip-cd.sh exited (code {code}).",
                       None if self.stopping else RED)
        self.update_buttons()

    def update_buttons(self):
        running = self.proc is not None
        self.start_btn.setText("Stop" if running else "Start")
        self.start_btn.setIcon(QIcon.fromTheme(
            "media-playback-stop" if running else "media-playback-start"))
        self.eject_btn.setEnabled(bool(self.device) and not self.in_disc)
        self.folder_btn.setEnabled(bool(self.album_dir) and os.path.isdir(self.album_dir))
        self.unknown.setEnabled(not running)

    def eject(self):
        if self.device:
            QProcess.startDetached("eject", [self.device])

    def notify(self, title, body):
        QProcess.startDetached("notify-send", ["-a", "CD Ripper", "-i",
                                               "media-optical-audio", title, body])

    def read_output(self):
        if not self.proc:
            return
        self.feed(bytes(self.proc.readAllStandardOutput()))

    def feed(self, data):
        if not data:
            return
        if self.logf:
            self.logf.write(data)
            self.logf.flush()
        self.buf += data.decode("utf-8", "replace")
        *lines, self.buf = re.split(r"[\r\n]", self.buf)
        shown = []
        for line in lines:
            if line.startswith("@@"):
                self.rip_event(line[2:].split("\t"))
            elif PROGRESS_RE.match(line):
                self.progress(line)
            else:
                self.output_line(line)
                if line.strip():
                    shown.append(line)
        if shown:
            self.log.appendPlainText("\n".join(shown))

    # ---------- cyanrip's own output ----------
    def progress(self, line):
        m = PROGRESS_RE.match(line)
        num, pct = int(m.group(1)), float(m.group(2))
        t = self.tracks.get(num)
        if not t:
            return
        if t.state != "reading":
            t.state = "reading"
            self.cur = num
        t.pct = pct
        self.show_track(t)
        self.track_bar.setValue(int(pct))
        self.track_bar.setFormat(f"Track {num}: {t.title} · %p%")
        self.update_pass_bar()

    def output_line(self, line):
        m = RIPPED_RE.match(line)
        if m:
            num = int(m.group(1))
            self.cur = num
            t = self.tracks.get(num)
            if t:
                t.reads += 1
                t.state = "readerr" if "with errors" in line else "read"
                self.show_track(t)
            return
        m = ACCURIP_RE.match(line)
        if m and self.cur in self.tracks:
            t = self.tracks[self.cur]
            first_pass = self.pass_label == "Ripping"
            if m.group(1) == "accurately ripped":
                if t.state != "ar-match" and first_pass:
                    self.ar_hits += 1
                t.state = "ar-match"
            elif t.state == "read":
                t.state = "ar-miss"
                if first_pass:
                    self.ar_misses += 1
                    self.refresh_header()
            self.show_track(t)

    # ---------- rip-cd.sh events ----------
    def rip_event(self, f):
        kind, args = f[0], f[1:]
        if kind == "waiting":
            self.in_disc = False
            self.track_bar.setValue(0)
            self.pass_bar.setValue(0)
            self.pass_bar.setFormat("")
            if self.finished:
                text, colour = self.finish_msg
                self.set_stage(f"{text}\nInsert the next disc.", colour)
            else:
                self.set_stage("Waiting for an audio CD… insert one.")
        elif kind == "inserted":
            self.reset_disc()
            self.device = args[0]
            self.in_disc = True
            self.disc_start = time.time()
            self.cover_path = ""
            self.tracks = {}
            self.rebuild_table()
            self.set_stage(f"Disc found in {self.device}; letting it spin up…")
        elif kind == "drive":
            self.device = args[0]
        elif kind == "offset":
            self.offset = args[0]
        elif kind == "stage":
            self.stage_event(args[0])
        elif kind == "release":
            self.release = args[0]
        elif kind == "disc":
            self.artist_name, self.album_name = args[0], args[1]
            self.disc_no, self.discs = int(args[2] or 1), int(args[3] or 1)
            self.album_dir = args[4]
            self.tracks = {}
            self.look_for_cover()
        elif kind == "track":
            num = int(args[0])
            self.tracks[num] = Track(num, args[1], args[2], args[3])
            self.rebuild_table()
        elif kind == "cover":
            self.set_cover(args[0])
        elif kind == "stagedir":
            self.stage_dir = args[0]
            if not self.cover_path:
                self.cover_poll.start()
        elif kind == "accuraterip":
            self.ar_note = ("in AccurateRip" if args[0] == "true"
                            else "not in AccurateRip: tracks are checked by reading twice")
        elif kind == "pass":
            self.pass_label = args[1]
            self.pass_tracks = [int(n) for n in args[2].split()]
            self.pass_start = time.time()
            for n in self.pass_tracks:
                self.set_state(n, "queued")
            n = len(self.pass_tracks)
            if n == len(self.tracks) and n > 1:
                which = f"all {n} tracks"
            else:
                which = f"track{'s' if n > 1 else ''} {', '.join(map(str, self.pass_tracks))}"
            self.set_stage(f"{self.pass_label}: {which}")
        elif kind == "result":
            num = int(args[0])
            t = self.tracks.get(num)
            if t and t.state not in ("ok-ar", "ok-two"):
                self.set_state(num, RESULT_STATE.get(args[1], "retry"))
        elif kind == "accepted":
            self.set_state(int(args[0]), "ok-ar" if args[1] == "verified" else "ok-two")
        elif kind == "rejected":
            self.set_state(int(args[0]), "rejected")
        elif kind == "finished":
            self.disc_finished(args[0], args[1] if len(args) > 1 else "")
        elif kind == "error":
            self.finished = ("error", args[0])
            self.finish_msg = (f"✘ {args[0]}", RED)
            self.set_stage(args[0], RED)
            self.add_session(f"✘ {self.disc_name() or self.device}: {args[0]}")
            self.notify("Rip failed", args[0])
        elif kind == "discend":
            self.in_disc = False
            if not self.finished and args[1] != "0":
                self.finished = ("error", f"exit code {args[1]}")
                self.add_session(f"✘ {self.disc_name() or args[0]}: stopped (code {args[1]})")
        elif kind == "remove":
            self.set_stage(f"Remove the disc from {args[0]} to continue.", ORANGE)
        self.refresh_header()
        self.update_pass_bar()
        self.update_buttons()

    def stage_event(self, what):
        if what == "reading":
            self.set_stage("Reading the disc and looking it up on MusicBrainz…")
        elif what == "ripping":
            self.set_stage("Ripping…")
        elif what == "phone":
            self.set_stage("Making Opus copies for the phone…")
            self.track_bar.setValue(0)
            self.track_bar.setFormat("")
            self.pass_tracks = []
            self.pass_bar.setRange(0, 0)       # busy indicator
            self.pass_bar.setFormat("")
        elif what == "eject":
            self.set_stage("Ejecting…")
            self.pass_bar.setRange(0, 1000)
            self.pass_bar.setValue(1000)

    def disc_name(self):
        if not self.album_name:
            return ""
        name = f"{self.artist_name} – {self.album_name}"
        return name + (f", disc {self.disc_no} of {self.discs}" if self.discs > 1 else "")

    def disc_finished(self, how, missing):
        self.discs_done += 1
        n = len(self.tracks)
        took = f" ({mmss(time.time() - self.disc_start)})" if self.disc_start else ""
        if how == "missing":
            self.finished = ("missing", missing)
            msg = f"{n - len(missing.split())} of {n} tracks; couldn't read: {missing}"
            self.finish_msg = (f"⚠ Done, but track(s) {missing} couldn't be read reliably. "
                               "Clean the disc and rip it again: only those are re-read.", ORANGE)
            self.set_stage(*self.finish_msg)
            self.add_session(f"⚠ {self.disc_name()}: {msg}{took}")
            self.notify("Disc done, with missing tracks", f"{self.disc_name()}\n{msg}")
        else:
            self.finished = ("ok", "")
            self.finish_msg = (f"✔ Done: all {n} tracks are in the library.", GREEN)
            self.set_stage(*self.finish_msg)
            self.add_session(f"✔ {self.disc_name()}: all {n} tracks{took}")
            self.notify("Disc done", f"{self.disc_name()}\nAll {n} tracks ripped. "
                                     "Insert the next disc.")

    def add_session(self, text):
        self.sess.show()
        self.session.addItem(text)
        self.session.scrollToBottom()

    def closeEvent(self, e):
        if self.proc:
            if not self.stop():
                e.ignore()
                return
            self.proc.waitForFinished(8000)
        e.accept()


def main():
    app = QApplication(sys.argv)
    app.setApplicationName("CD Ripper")
    app.setDesktopFileName("rip-cd")

    # one window only: a second launch (e.g. the disc-inserted popup) raises it
    sock = QLocalSocket()
    sock.connectToServer(INSTANCE)
    if sock.waitForConnected(500):
        sock.write(b"raise")
        sock.waitForBytesWritten(500)
        return 0
    QLocalServer.removeServer(INSTANCE)
    server = QLocalServer()
    server.listen(INSTANCE)

    args = sys.argv[1:]
    autostart = "--no-start" not in args
    args = [a for a in args if a != "--no-start"]
    win = RipWindow(args)

    def raise_window():
        conn = server.nextPendingConnection()
        if conn:
            conn.close()
        win.showNormal()
        win.raise_()
        win.activateWindow()
    server.newConnection.connect(raise_window)

    win.show()
    if autostart:
        QTimer.singleShot(200, win.start)
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
