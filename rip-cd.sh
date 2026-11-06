#!/usr/bin/env bash
# rip-cd.sh - one command to:
#   1. rip the CD in the drive to FLAC with cyanrip (cd-paranoia + AccurateRip)
#   2. tag it from MusicBrainz and embed the cover art
#   3. make Opus copies in the phone folder (via flac2phone.sh)
#   4. eject the disc
#
# Usage:
#   rip-cd.sh setup        # once per drive: find its read offset (needs a
#                          # popular CD that's in the AccurateRip database)
#   rip-cd.sh              # rip the disc in the drive
#   rip-cd.sh --loop       # keep going: wait for the next disc after each one
#   rip-cd.sh --unknown    # rip a disc MusicBrainz doesn't know (generic names)
#   rip-cd.sh -R <mbid>    # use this exact MusicBrainz release
#   (any other options are passed on to cyanrip)
#   rip-cd-gui.py          # the same, in a desktop window (runs --loop)
#
# Override settings when running, e.g.
#   DEVICE=/dev/sr1 rip-cd.sh       # which drive (default: the one with a CD)
#   READS=8 rip-cd.sh               # try harder on a damaged disc (default 4 reads)
#   COUNTRY=GB rip-cd.sh            # prefer a UK edition (default US; COUNTRY= for none)
#   PHONE_DIR=~/Music/phone FLAC_DIR=~/Music/FLAC rip-cd.sh
#   RIPCD_EVENTS=1 rip-cd.sh        # also print "@@event<TAB>..." lines (for the GUI)
#
# How a rip works:
#   - Tracks already in FLAC_DIR are never re-ripped or overwritten, so running
#     it again on the same disc only fills in what's missing.
#   - Tracks are ripped into a staging folder; only tracks that check out are
#     moved into FLAC_DIR:
#       * a read that matches the AccurateRip database is proven correct
#       * otherwise it's re-read until two reads give the same checksum
#         (or one matches AccurateRip), up to READS reads in all (default 4)
#   - Tracks that still fail are kept in ~/.cache/rip-cd/rejected for a listen;
#     a damaged spot rarely reads better with more tries, so it's quicker to
#     clean the disc or use another drive and run the script again.
#   - Each drive is locked while it's ripping, so two copies of this script
#     (e.g. one per drive) never use the same drive.
#
# Needs: cyanrip (built from source, see CLAUDE.md), opus-tools, and
# flac2phone.sh and mb-enrich.py in the same folder as this script.

set -euo pipefail

# Everything is inside main(), which bash reads completely before running it.
# That way, updating this file while a rip is running can't break that rip.
main() {

FLAC_DIR=${FLAC_DIR:-$HOME/Music/FLAC}
PHONE_DIR=${PHONE_DIR:-$HOME/Music/phone}
OPUS_BITRATE=${OPUS_BITRATE:-160}
COUNTRY=${COUNTRY-US}
READS=${READS:-4}       # most reads of one track before giving up on it

CONFIG_DIR=$HOME/.config/rip-cd
OFFSETS=$CONFIG_DIR/offsets          # lines of: <drive serial> <offset>
CACHE_DIR=$HOME/.cache/rip-cd
REJECT_DIR=$CACHE_DIR/rejected
FLAC2PHONE="$(dirname "$(realpath "$0")")/flac2phone.sh"
MB_ENRICH="$(dirname "$(realpath "$0")")/mb-enrich.py"

# Machine-readable progress for the GUI: "@@event<TAB>field<TAB>field..."
ev() {
    [[ -n ${RIPCD_EVENTS:-} ]] || return 0
    local IFS=$'\t'
    printf '@@%s\n' "$*"
}
die() {  # die LINE... : print the message, tell the GUI, exit 1
    local m
    for m in "$@"; do echo "$m" >&2; done
    ev error "$1"
    exit 1
}

mkdir -p "$FLAC_DIR" "$PHONE_DIR" "$CONFIG_DIR" "$CACHE_DIR"

command -v cyanrip >/dev/null || die "cyanrip not found (it's built from source; see CLAUDE.md)"

udev_prop() {  # udev_prop DEVICE KEY
    { udevadm info --query=property --name="$1" 2>/dev/null || true; } | sed -n "s/^$2=//p"
}
has_audio_cd() { [[ $(udev_prop "$1" ID_CDROM_MEDIA_TRACK_COUNT_AUDIO) -gt 0 ]] 2>/dev/null; }
lock_file() { echo "$CACHE_DIR/lock-${1##*/}"; }
is_busy() { ! flock -n "$(lock_file "$1")" true 2>/dev/null; }   # another rip has it

# --loop: rip disc after disc. Each disc is a normal one-disc run of this
# script; a failed disc is ejected and the loop carries on. Ctrl+C to stop.
if [[ " $* " == *" --loop "* ]]; then
    local args=() a d dev self rc
    for a in "$@"; do [[ $a == --loop ]] || args+=("$a"); done
    self=$(realpath "$0")
    while :; do
        echo
        echo "== Waiting for an audio CD (Ctrl+C to stop) =="
        ev waiting
        dev=""
        while [[ -z $dev ]]; do
            for d in ${DEVICE:-/dev/sr[0-9]*}; do
                [[ -b $d ]] && has_audio_cd "$d" && ! is_busy "$d" && { dev=$d; break; }
            done
            [[ -n $dev ]] || sleep 3
        done
        ev inserted "$dev"
        sleep 5   # let the drive and the desktop finish looking at the new disc
        rc=0
        DEVICE=$dev "$self" "${args[@]}" || rc=$?
        ev discend "$dev" "$rc"
        if (( rc != 0 )); then
            echo
            echo "== That disc didn't finish cleanly (see above); moving on =="
            eject "$dev" 2>/dev/null || true
        fi
        # wait for the disc to be gone, in case it couldn't be ejected
        if has_audio_cd "$dev"; then ev remove "$dev"; fi
        while has_audio_cd "$dev"; do
            echo "(Remove the disc from $dev to continue.)"
            sleep 5
        done
    done
fi

# Pick the drive: $DEVICE if set, else the one free drive that has an audio CD
if [[ -z ${DEVICE:-} ]]; then
    found=()
    for dev in /dev/sr[0-9]*; do
        [[ -b $dev ]] && has_audio_cd "$dev" && ! is_busy "$dev" && found+=("$dev")
    done
    (( ${#found[@]} )) ||
        die "No audio CD found in any drive. Insert one (wait for it to spin up)," \
            "or set DEVICE=/dev/srN."
    (( ${#found[@]} == 1 )) ||
        die "Audio CDs in more than one drive (${found[*]}); pick one with DEVICE=/dev/srN"
    DEVICE=${found[0]}
fi
exec 9>"$(lock_file "$DEVICE")"
flock -n 9 || die "Another rip is already using $DEVICE."
DRIVE_ID=$(udev_prop "$DEVICE" ID_SERIAL)
DRIVE_ID=${DRIVE_ID:-unknown-drive}
# The serial number is what identifies the drive: the SuperDrive sometimes
# reports its own name and sometimes its inner LG mechanism's (same serial)
DRIVE_SN=$(udev_prop "$DEVICE" ID_SERIAL_SHORT)
echo "Using drive: $DEVICE ($DRIVE_ID)"
ev drive "$DEVICE" "$DRIVE_ID"

saved_offset() {  # saved offset for this drive: by full ID, else by serial number
    local o
    o=$(awk -v id="$DRIVE_ID" '$1 == id {print $2}' "$OFFSETS" 2>/dev/null | tail -1)
    if [[ -z $o && -n $DRIVE_SN ]]; then
        o=$(awk -v sn="$DRIVE_SN" '$1 == sn || index($1, "_" sn "-") {print $2}' \
            "$OFFSETS" 2>/dev/null | tail -1)
    fi
    echo "$o"
}

if [[ ${1:-} == setup ]]; then
    echo "== Finding the read offset (use a popular, commonly owned CD) =="
    out=$(cyanrip -d "$DEVICE" -f 2>&1 | tee /dev/stderr) || true
    offset=$(grep -oP 'Drive offset of \K[+-]?[0-9]+(?= found)' <<<"$out" | tail -1)
    offset=${offset#+}
    [[ -n $offset ]] || die "Couldn't find the offset with this disc; try a different popular CD."
    old=$(saved_offset)
    if [[ -n $old && $old != "$offset" ]]; then
        # a wrong offset makes every track fail AccurateRip, so don't replace
        # a known-good one by accident (2026-11-06: setup found +396 on a
        # SuperDrive whose real offset is +6)
        echo
        echo "WARNING: this drive already has offset $old saved, and this disc gave $offset."
        echo "If $old verified fine before, keep it: offset finding can be fooled by some discs."
        read -rp "Replace $old with $offset? [y/N] " yn
        [[ $yn == [yY]* ]] || { echo "Kept $old."; exit 0; }
    fi
    touch "$OFFSETS"
    grep -v "^$DRIVE_ID " "$OFFSETS" > "$OFFSETS.new" || true
    echo "$DRIVE_ID $offset" >> "$OFFSETS.new"
    mv "$OFFSETS.new" "$OFFSETS"
    echo
    echo "Saved read offset $offset for this drive in $OFFSETS."
    echo "Setup done. Run rip-cd.sh to rip a disc."
    exit 0
fi

OFFSET=$(saved_offset)
[[ -n $OFFSET ]] || die "No read offset saved for this drive ($DRIVE_ID) yet. Run: rip-cd.sh setup"
echo "Read offset: $OFFSET"
ev offset "$OFFSET"

[[ -x $FLAC2PHONE ]] || die "flac2phone.sh not found next to this script: $FLAC2PHONE"
command -v opusenc >/dev/null || die "opusenc not found: sudo dnf install opus-tools"

UNKNOWN=false
ARGS=()
for a in "$@"; do
    if [[ $a == --unknown || $a == -U ]]; then UNKNOWN=true; else ARGS+=("$a"); fi
done

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
STAGE=$CACHE_DIR/staging
rm -rf "$STAGE"
RUN_TAG=$(date '+%Y-%m-%d %H%M%S')

# Folder layout: FLAC_DIR/Artist/Album/NN. Artist - Title.flac
# (track numbers padded to 2 digits; multi-disc albums get a "disc." prefix)
dir_scheme() { echo "$1/{album_artist}/{album}"; }
TRACK_SCHEME='{if #totaldiscs# > #1#|disc|.}{if #tracktotal# < #10#0}{track}. {artist} - {title}'
# Multi-disc albums share a folder, so each disc gets its own CUE sheet/log name
DISC_SUFFIX='{if #totaldiscs# > #1# CD|disc|}'

# Options used for every cyanrip run on this disc. -X: skip data tracks.
COMMON=(-d "$DEVICE" -s "$OFFSET" -o flac -X -F "$TRACK_SCHEME")
META=()

# Parse cyanrip output into lines of "<track> <status> <crc> <file>", where
# crc is the EAC CRC32 of the read ("-" if none) and status is
#   verified      matches AccurateRip
#   unverified    no AccurateRip match, read without errors
#   inconsistent  -Z repeat reads never agreed
#   error         cd-paranoia reported read errors
classify() {
    tr '\r' '\n' < "$1" | awk '
        /^Track [0-9]+ info:/              { cur = $2 + 0 }
        /Done; \(no matches found/         { nomatch = 1 }
        /^Track [0-9]+ ripped and encoded/ { t = $2 + 0; seen[t] = 1; cur = t
                                             if (/with errors/) err[t] = 1
                                             if (nomatch) nm[t] = 1
                                             nomatch = 0 }
        /^  EAC CRC32:/                    { if (cur) crc[cur] = $3 }
        /Accurip v[12]:.*\(accurately ripped/ { if (cur) ok[cur] = 1 }
        /^  File\(s\):/                    { grab = 1; next }
        grab && /^    /                    { path[cur] = substr($0, 5); grab = 0 }
        END { for (t in seen)
                  print t, (err[t] ? "error" : ok[t] ? "verified" :
                            nm[t] ? "inconsistent" : "unverified"),
                        (crc[t] ? crc[t] : "-"), path[t] }'
}

# Info pass: pick the MusicBrainz release, and find out which files exist
info() {
    local rc=0
    cyanrip -I "${COMMON[@]}" -D "$(dir_scheme "$FLAC_DIR")" "${META[@]}" \
        "${ARGS[@]}" > "$WORK/info.out" 2>&1 || rc=$?
    return $rc
}

has_arg() {
    local a o
    for a in "${ARGS[@]}"; do
        for o in "$@"; do [[ $a == "$o" || $a == "$o="* ]] && return 0; done
    done
    return 1
}

echo "== Reading the disc and looking it up =="
ev stage reading
if $UNKNOWN; then
    # dated, so two unknown discs never land in the same folder
    META=(-N -a "album=Unknown Album $(date '+%Y-%m-%d %H%M'):album_artist=Unknown Artist")
fi
rc=0; info || rc=$?
if grep -q 'Multiple releases found' "$WORK/info.out" && ! has_arg -R --release; then
    grep -E '^ +[0-9]+ \(ID: ' "$WORK/info.out"
    pick=""
    [[ -n $COUNTRY ]] &&
        pick=$(grep -E '^ +[0-9]+ \(ID: ' "$WORK/info.out" | grep -F "($COUNTRY)" | head -1)
    if [[ -n $pick ]]; then
        echo "-> using the $COUNTRY edition"
    else
        pick=$(grep -E '^ +[0-9]+ \(ID: ' "$WORK/info.out" | head -1)
        echo "-> no ${COUNTRY:-preferred} edition; using the first one (pick another with -R <ID>)"
    fi
    ev release "$(sed -E 's/^ *[0-9]+ \(ID: [^)]*\): //' <<<"$pick")"
    META+=(-R "$(grep -oP '\(ID: \K[^)]+' <<<"$pick")")
    rc=0; info || rc=$?
fi
if grep -q 'MusicBrainz lookup failed' "$WORK/info.out" && ! $UNKNOWN; then
    grep 'MusicBrainz' "$WORK/info.out" | head -3
    die "MusicBrainz doesn't know this disc; try: rip-cd.sh --unknown"
fi
if (( rc != 0 )) && ! grep -q '^Track [0-9]* info:' "$WORK/info.out"; then
    tail -20 "$WORK/info.out"
    die "Couldn't read the disc info (see above)."
fi
grep -E '^(Album|Album artist|AccurateRip|Total time|Offset): ' "$WORK/info.out" || true

# Which tracks aren't in the library yet? (fields split by \037: titles and
# lengths can't be split on tabs, which bash would merge when one is empty)
declare -A DEST
MISSING=()
TRACK_EV=()
while IFS=$'\037' read -r num dur title path; do
    DEST[$num]=$path
    if [[ -e $path ]]; then st=have; else st=todo; MISSING+=("$num"); fi
    TRACK_EV+=("$num"$'\t'"$dur"$'\t'"$title"$'\t'"$st")
done < <(tr '\r' '\n' < "$WORK/info.out" | awk '
    /^Track [0-9]+ info:/ { cur = $2 + 0; dur = ""; title = "" }
    /^    Duration:/      { dur = $2 }
    /^    title:/         { sub(/^    title: */, ""); title = $0 }
    /^  File\(s\):/       { grab = 1; next }
    grab && /^    /       { print cur "\037" dur "\037" title "\037" substr($0, 5); grab = 0 }' |
    sort -n)
(( ${#DEST[@]} )) || { tail -20 "$WORK/info.out"; die "No tracks found on the disc."; }
for k in "${!DEST[@]}"; do ALBUM_DIR=$(dirname "${DEST[$k]}"); break; done
rel=${ALBUM_DIR#"$FLAC_DIR"/}
ALL_TRACKS=${#DEST[@]}
mapfile -t MISSING < <(printf '%s\n' "${MISSING[@]}" | sort -n | sed '/^$/d')

infofield() { tr '\r' '\n' < "$WORK/info.out" | sed -n "s/^$1: *//p" | head -1; }
DISC_NO=$(infofield 'Disc number')
DISCS=$(infofield 'Total discs')
ev disc "$(infofield 'Album artist')" "$(infofield 'Album')" "${DISC_NO:-1}" "${DISCS:-1}" \
    "$ALBUM_DIR" "$DEVICE"
for t in "${TRACK_EV[@]}"; do ev track "$t"; done
for c in "$ALBUM_DIR"/cover.{jpg,png}; do
    [[ -s $c ]] && { ev cover "$c"; break; }
done
ev stagedir "$STAGE/$rel"

if (( ${#MISSING[@]} == 0 )); then
    echo
    echo "All $ALL_TRACKS tracks are already in $ALBUM_DIR; nothing to rip."
    NEED=()
else
    if (( ${#MISSING[@]} < ALL_TRACKS )); then
        echo "Already in the library: $(( ALL_TRACKS - ${#MISSING[@]} )) tracks. Ripping: ${MISSING[*]}"
    fi
    ev stage ripping

    # Use the existing cover rather than downloading it again
    COVER=()
    for c in "$ALBUM_DIR"/cover.{jpg,png}; do
        [[ -s $c ]] && { COVER=(-C "$c"); break; }
    done

    PASS=0
    # rip_pass LABEL TRACKS... [-- extra cyanrip options]: one cyanrip run into
    # STAGE, then its results are in $WORK/result (see classify)
    rip_pass() {
        local label=$1; shift
        local tracks=() extra=()
        while (( $# )) && [[ $1 != -- ]]; do tracks+=("$1"); shift; done
        [[ ${1:-} == -- ]] && { shift; extra=("$@"); }
        PASS=$((PASS + 1))
        ev pass "$PASS" "$label" "${tracks[*]}"
        local list; list=$(IFS=,; echo "${tracks[*]}")
        local rc=0
        cyanrip "${COMMON[@]}" -D "$(dir_scheme "$STAGE")" "${META[@]}" "${COVER[@]}" -l "$list" \
            -L "{album_artist} - {album}$DISC_SUFFIX (rip-cd $RUN_TAG pass $PASS)" \
            -M "{album_artist} - {album}$DISC_SUFFIX" "${extra[@]}" "${ARGS[@]}" 2>&1 |
            tee "$WORK/pass$PASS.out" || rc=${PIPESTATUS[0]}
        # pick up the cover cyanrip downloaded, for flac2phone and later passes
        local d front
        d=$STAGE/$rel
        for front in "$d"/Front.*; do
            [[ -e $front ]] || continue
            mv "$front" "$d/cover.${front##*.}"
            [[ -e $ALBUM_DIR/cover.${front##*.} ]] || {
                mkdir -p "$ALBUM_DIR"; cp "$d/cover.${front##*.}" "$ALBUM_DIR/"; }
            COVER=(-C "$ALBUM_DIR/cover.${front##*.}")
            ev cover "$ALBUM_DIR/cover.${front##*.}"
        done
        classify "$WORK/pass$PASS.out" > "$WORK/result"
        local num status _
        while read -r num status _; do ev result "$num" "$status"; done < "$WORK/result"
        return $rc
    }

    # accept TRACKS_FILE STATUS...: move tracks with an OK status into the library
    accept() {
        local file=$1; shift
        local num status crc path dest ok
        while read -r num status crc path; do
            ok=false
            for s in "$@"; do [[ $status == "$s" ]] && ok=true; done
            $ok || continue
            dest=${DEST[$num]}
            [[ -e $path && ! -e $dest ]] || continue
            mkdir -p "$(dirname "$dest")"
            mv "$path" "$dest"
            echo "  track $num: $status -> library"
            ev accepted "$num" "$status"
            local rest=() n
            for n in "${NEED[@]}"; do [[ $n == "$num" ]] || rest+=("$n"); done
            NEED=("${rest[@]}")
        done < "$file"
    }

    # repeats: accept clean reads whose checksum matches an earlier read of the
    # same track (two identical reads, the same test as cyanrip -Z 1)
    declare -A CRCS
    repeats() {
        local num status crc path
        while read -r num status crc path; do
            [[ $status == unverified && $crc != - ]] || continue
            [[ " ${NEED[*]} " == *" $num "* ]] || continue
            [[ " ${CRCS[$num]:-} " == *" $crc "* ]] && echo "$num repeat $crc $path"
            CRCS[$num]+=" $crc"
        done < "$WORK/result" > "$WORK/repeats"
        accept "$WORK/repeats" repeat
    }

    NEED=("${MISSING[@]}")

    echo
    echo "== Ripping track(s) ${NEED[*]} =="
    rip_pass "Ripping" "${NEED[@]}" || true
    AR_KNOWN=false
    grep -q 'Accurip: *disc found in database' "$WORK/pass$PASS.out" && AR_KNOWN=true
    ev accuraterip "$AR_KNOWN"
    echo
    echo "== Results =="
    sort -n "$WORK/result" | awk '{print "  track " $1 ": " $2}'
    accept "$WORK/result" verified
    repeats
    if ! $AR_KNOWN; then
        echo "(This disc isn't in the AccurateRip database, so tracks are checked by reading twice.)"
    fi

    # Re-read what's left: accept an AccurateRip match, or a read with the
    # same checksum as an earlier one; give up after READS reads in all
    read_no=2
    while (( ${#NEED[@]} && read_no <= READS )); do
        echo
        echo "== Re-reading track(s) ${NEED[*]} (read $read_no of $READS) =="
        rip_pass "Re-reading (read $read_no of $READS)" "${NEED[@]}" || true
        accept "$WORK/result" verified
        repeats
        read_no=$((read_no + 1))
    done

    # Keep the logs; keep the CUE sheet only from a full-disc rip
    mkdir -p "$ALBUM_DIR"
    for f in "$STAGE/$rel"/*.log; do
        [[ -e $f ]] && mv "$f" "$ALBUM_DIR/"
    done
    if (( ${#MISSING[@]} == ALL_TRACKS )); then
        for f in "$STAGE/$rel"/*.cue; do
            [[ -e $f && ! -e $ALBUM_DIR/$(basename "$f") ]] && mv "$f" "$ALBUM_DIR/"
        done
    fi

    # Anything still failing goes to the rejects folder, not the library
    if (( ${#NEED[@]} )); then
        mkdir -p "$REJECT_DIR/$rel"
        for num in "${NEED[@]}"; do
            src=${DEST[$num]/#"$FLAC_DIR"/$STAGE}
            [[ -e $src ]] && mv "$src" "$REJECT_DIR/$rel/"
            ev rejected "$num"
        done
    fi
fi
rm -rf "$STAGE"

if [[ ! -e $ALBUM_DIR/cover.jpg && ! -e $ALBUM_DIR/cover.png ]]; then
    echo "(No cover art found for this album.)"
fi

# cyanrip doesn't write the album's original release date or genres; add them
# from MusicBrainz before the Opus copies are made. Not fatal: a network
# hiccup just leaves them for a later `mb-enrich.py` run.
echo
echo "== Adding original release date and genres from MusicBrainz =="
ev stage tagging
"$MB_ENRICH" "$ALBUM_DIR" || echo "(Couldn't add them now; run mb-enrich.py later.)"

echo
echo "== Making Opus copies in $PHONE_DIR =="
ev stage phone
# locked, so rips on two drives don't convert the same files at once
flock "$CACHE_DIR/flac2phone.lock" "$FLAC2PHONE" "$FLAC_DIR" "$PHONE_DIR" opus "$OPUS_BITRATE"

echo
echo "== Ejecting =="
ev stage eject
eject "$DEVICE" || echo "Couldn't eject automatically; use the Disks & Devices popup."

echo
if (( ${#NEED[@]} )); then
    echo "Done, BUT these tracks couldn't be read reliably and are missing: ${NEED[*]}" >&2
    echo "The failed reads are in $REJECT_DIR/$rel for a listen." >&2
    echo "Clean the disc or try another drive, then run rip-cd.sh again: it only" >&2
    echo "rips what's missing." >&2
    ev finished missing "${NEED[*]}"
    exit 5
fi
ev finished ok
echo "All done. Insert the next disc and run rip-cd.sh again."
}

main "$@"; exit
