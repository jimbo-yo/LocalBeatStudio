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
#   rip-cd.sh --unknown    # rip a disc MusicBrainz doesn't know (generic names)
#   rip-cd.sh -R <mbid>    # use this exact MusicBrainz release
#   (any other options are passed on to cyanrip)
#
# Override settings when running, e.g.
#   DEVICE=/dev/sr1 rip-cd.sh       # which drive (default: the one with a CD)
#   COUNTRY=GB rip-cd.sh            # prefer a UK edition (default US; COUNTRY= for none)
#   PHONE_DIR=~/Music/phone FLAC_DIR=~/Music/FLAC rip-cd.sh
#
# How a rip works:
#   - Tracks already in FLAC_DIR are never re-ripped or overwritten, so running
#     it again on the same disc only fills in what's missing.
#   - Tracks are ripped into a staging folder; only tracks that check out are
#     moved into FLAC_DIR:
#       * a read that matches the AccurateRip database is proven correct
#       * otherwise it's re-read up to AR_TRIES more times, hoping for a match
#       * otherwise it needs two identical reads (cyanrip -Z 1), like whipper
#   - Tracks that still fail are kept in ~/.cache/rip-cd/rejected for a listen.
#
# Needs: cyanrip (built from source, see CLAUDE.md), opus-tools, and
# flac2phone.sh in the same folder as this script.

set -euo pipefail

# Everything is inside main(), which bash reads completely before running it.
# That way, updating this file while a rip is running can't break that rip.
main() {

FLAC_DIR=${FLAC_DIR:-$HOME/Music/FLAC}
PHONE_DIR=${PHONE_DIR:-$HOME/Music/phone}
OPUS_BITRATE=${OPUS_BITRATE:-160}
COUNTRY=${COUNTRY-US}
AR_TRIES=${AR_TRIES:-3}

CONFIG_DIR=$HOME/.config/rip-cd
OFFSETS=$CONFIG_DIR/offsets          # lines of: <drive serial> <offset>
CACHE_DIR=$HOME/.cache/rip-cd
REJECT_DIR=$CACHE_DIR/rejected
FLAC2PHONE="$(dirname "$(realpath "$0")")/flac2phone.sh"

mkdir -p "$FLAC_DIR" "$PHONE_DIR" "$CONFIG_DIR" "$CACHE_DIR"

command -v cyanrip >/dev/null || {
    echo "cyanrip not found (it's built from source; see CLAUDE.md)" >&2; exit 1; }

udev_prop() {  # udev_prop DEVICE KEY
    { udevadm info --query=property --name="$1" 2>/dev/null || true; } | sed -n "s/^$2=//p"
}

# Pick the drive: $DEVICE if set, else the one drive that has an audio CD in it
if [[ -z ${DEVICE:-} ]]; then
    found=()
    for dev in /dev/sr[0-9]*; do
        [[ -b $dev ]] || continue
        [[ $(udev_prop "$dev" ID_CDROM_MEDIA_TRACK_COUNT_AUDIO) -gt 0 ]] 2>/dev/null &&
            found+=("$dev")
    done
    if (( ${#found[@]} == 0 )); then
        echo "No audio CD found in any drive. Insert one (wait for it to spin up)," >&2
        echo "or set DEVICE=/dev/srN." >&2
        exit 1
    elif (( ${#found[@]} > 1 )); then
        echo "Audio CDs in more than one drive (${found[*]}); pick one with DEVICE=/dev/srN" >&2
        exit 1
    fi
    DEVICE=${found[0]}
fi
DRIVE_ID=$(udev_prop "$DEVICE" ID_SERIAL)
DRIVE_ID=${DRIVE_ID:-unknown-drive}
echo "Using drive: $DEVICE ($DRIVE_ID)"

if [[ ${1:-} == setup ]]; then
    echo "== Finding the read offset (use a popular, commonly owned CD) =="
    out=$(cyanrip -d "$DEVICE" -f 2>&1 | tee /dev/stderr) || true
    offset=$(grep -oP 'Drive offset of \K[+-]?[0-9]+(?= found)' <<<"$out" | tail -1)
    offset=${offset#+}
    if [[ -z $offset ]]; then
        echo "Couldn't find the offset with this disc; try a different popular CD." >&2
        exit 1
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

OFFSET=$(awk -v id="$DRIVE_ID" '$1 == id {print $2}' "$OFFSETS" 2>/dev/null | tail -1)
if [[ -z $OFFSET ]]; then
    echo "No read offset saved for this drive yet. Run: rip-cd.sh setup" >&2
    exit 1
fi

[[ -x $FLAC2PHONE ]] || { echo "flac2phone.sh not found next to this script: $FLAC2PHONE" >&2; exit 1; }
command -v opusenc >/dev/null || { echo "opusenc not found: sudo dnf install opus-tools" >&2; exit 1; }

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

# Options used for every cyanrip run on this disc. -X: skip data tracks.
COMMON=(-d "$DEVICE" -s "$OFFSET" -o flac -X -F "$TRACK_SCHEME")
META=()

# Parse cyanrip output into lines of "<track> <status> <file>", where status is
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
        /Accurip v[12]:.*\(accurately ripped/ { if (cur) ok[cur] = 1 }
        /^  File\(s\):/                    { grab = 1; next }
        grab && /^    /                    { path[cur] = substr($0, 5); grab = 0 }
        END { for (t in seen)
                  print t, (err[t] ? "error" : ok[t] ? "verified" :
                            nm[t] ? "inconsistent" : "unverified"), path[t] }'
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
    META+=(-R "$(grep -oP '\(ID: \K[^)]+' <<<"$pick")")
    rc=0; info || rc=$?
fi
if grep -q 'MusicBrainz lookup failed' "$WORK/info.out" && ! $UNKNOWN; then
    grep 'MusicBrainz' "$WORK/info.out" | head -3
    echo "MusicBrainz doesn't know this disc; try: rip-cd.sh --unknown" >&2
    exit 1
fi
if (( rc != 0 )) && ! grep -q '^Track [0-9]* info:' "$WORK/info.out"; then
    tail -20 "$WORK/info.out"
    echo "Couldn't read the disc info (see above)." >&2
    exit 1
fi
grep -E '^(Album|Album artist|AccurateRip|Total time|Offset): ' "$WORK/info.out" || true

# Which tracks aren't in the library yet?
declare -A DEST
MISSING=()
while read -r num _ path; do
    DEST[$num]=$path
    [[ -e $path ]] || MISSING+=("$num")
done < <(tr '\r' '\n' < "$WORK/info.out" | awk '
    /^Track [0-9]+ info:/ { cur = $2 + 0 }
    /^  File\(s\):/       { grab = 1; next }
    grab && /^    /       { print cur, "-", substr($0, 5); grab = 0 }' | sort -n)
(( ${#DEST[@]} )) || { tail -20 "$WORK/info.out"; echo "No tracks found on the disc." >&2; exit 1; }
for k in "${!DEST[@]}"; do ALBUM_DIR=$(dirname "${DEST[$k]}"); break; done
ALL_TRACKS=${#DEST[@]}
mapfile -t MISSING < <(printf '%s\n' "${MISSING[@]}" | sort -n | sed '/^$/d')

if (( ${#MISSING[@]} == 0 )); then
    echo
    echo "All $ALL_TRACKS tracks are already in $ALBUM_DIR; nothing to rip."
else
    if (( ${#MISSING[@]} < ALL_TRACKS )); then
        echo "Already in the library: $(( ALL_TRACKS - ${#MISSING[@]} )) tracks. Ripping: ${MISSING[*]}"
    fi

    # Use the existing cover rather than downloading it again
    COVER=()
    for c in "$ALBUM_DIR"/cover.{jpg,png}; do
        [[ -s $c ]] && { COVER=(-C "$c"); break; }
    done

    PASS=0
    # rip_pass TRACKS... [-- extra cyanrip options]: one cyanrip run into STAGE
    rip_pass() {
        local tracks=() extra=()
        while (( $# )) && [[ $1 != -- ]]; do tracks+=("$1"); shift; done
        [[ ${1:-} == -- ]] && { shift; extra=("$@"); }
        PASS=$((PASS + 1))
        local list; list=$(IFS=,; echo "${tracks[*]}")
        local rc=0
        cyanrip "${COMMON[@]}" -D "$(dir_scheme "$STAGE")" "${META[@]}" "${COVER[@]}" -l "$list" \
            -L "{album_artist} - {album} (rip-cd $RUN_TAG pass $PASS)" \
            -M "{album_artist} - {album}" "${extra[@]}" "${ARGS[@]}" 2>&1 |
            tee "$WORK/pass$PASS.out" || rc=${PIPESTATUS[0]}
        # pick up the cover cyanrip downloaded, for flac2phone and later passes
        local d front
        d=$(dirname "${DEST[${tracks[0]}]/#"$FLAC_DIR"/$STAGE}")
        for front in "$d"/Front.*; do
            [[ -e $front ]] || continue
            mv "$front" "$d/cover.${front##*.}"
            [[ -e $ALBUM_DIR/cover.${front##*.} ]] || {
                mkdir -p "$ALBUM_DIR"; cp "$d/cover.${front##*.}" "$ALBUM_DIR/"; }
            COVER=(-C "$ALBUM_DIR/cover.${front##*.}")
        done
        return $rc
    }

    # accept TRACKS_FILE STATUS...: move tracks with an OK status into the library
    accept() {
        local file=$1; shift
        local num status path dest ok
        while read -r num status path; do
            ok=false
            for s in "$@"; do [[ $status == "$s" ]] && ok=true; done
            $ok || continue
            dest=${DEST[$num]}
            [[ -e $path && ! -e $dest ]] || continue
            mkdir -p "$(dirname "$dest")"
            mv "$path" "$dest"
            echo "  track $num: $status -> library"
            local rest=() n
            for n in "${NEED[@]}"; do [[ $n == "$num" ]] || rest+=("$n"); done
            NEED=("${rest[@]}")
        done < "$file"
    }

    NEED=("${MISSING[@]}")

    echo
    echo "== Ripping track(s) ${NEED[*]} =="
    rip_pass "${NEED[@]}" || true
    classify "$WORK/pass$PASS.out" > "$WORK/result"
    AR_KNOWN=false
    grep -q 'Accurip: *disc found in database' "$WORK/pass$PASS.out" && AR_KNOWN=true
    echo
    echo "== Results =="
    sort -n "$WORK/result" | awk '{print "  track " $1 ": " $2}'
    accept "$WORK/result" verified
    if ! $AR_KNOWN; then
        echo "(This disc isn't in the AccurateRip database, so tracks are checked by reading twice.)"
    fi

    # Retry: single reads, hoping for an AccurateRip match
    if $AR_KNOWN; then
        try=1
        while (( ${#NEED[@]} && try <= AR_TRIES )); do
            echo
            echo "== Re-reading track(s) ${NEED[*]} (AccurateRip try $try of $AR_TRIES) =="
            rip_pass "${NEED[@]}" || true
            classify "$WORK/pass$PASS.out" > "$WORK/result"
            accept "$WORK/result" verified
            try=$((try + 1))
        done
    fi

    # Last resort: two identical reads (up to cyanrip's -r limit of tries)
    if (( ${#NEED[@]} )); then
        echo
        echo "== Track(s) ${NEED[*]}: reading until two reads match =="
        rip_pass "${NEED[@]}" -- -Z 1 || true
        classify "$WORK/pass$PASS.out" > "$WORK/result"
        accept "$WORK/result" verified unverified
    fi

    # Keep the logs; keep the CUE sheet only from a full-disc rip
    rel=${ALBUM_DIR#"$FLAC_DIR"/}
    mkdir -p "$ALBUM_DIR"
    for f in "$STAGE/$rel"/*.log; do
        [[ -e $f ]] && mv "$f" "$ALBUM_DIR/"
    done
    if (( ${#MISSING[@]} == ALL_TRACKS )) && [[ ! -e $ALBUM_DIR/$(basename "$rel").cue ]]; then
        for f in "$STAGE/$rel"/*.cue; do [[ -e $f ]] && mv "$f" "$ALBUM_DIR/"; done
    fi

    # Anything still failing goes to the rejects folder, not the library
    if (( ${#NEED[@]} )); then
        mkdir -p "$REJECT_DIR/$rel"
        for num in "${NEED[@]}"; do
            src=${DEST[$num]/#"$FLAC_DIR"/$STAGE}
            [[ -e $src ]] && mv "$src" "$REJECT_DIR/$rel/"
        done
    fi
fi
rm -rf "$STAGE"

if [[ ! -e $ALBUM_DIR/cover.jpg && ! -e $ALBUM_DIR/cover.png ]]; then
    echo "(No cover art found for this album.)"
fi

echo
echo "== Making Opus copies in $PHONE_DIR =="
"$FLAC2PHONE" "$FLAC_DIR" "$PHONE_DIR" opus "$OPUS_BITRATE"

echo
echo "== Ejecting =="
eject "$DEVICE" || echo "Couldn't eject automatically; use the Disks & Devices popup."

echo
if (( ${#MISSING[@]} )) && (( ${#NEED[@]} )); then
    echo "Done, BUT these tracks couldn't be read reliably and are missing: ${NEED[*]}" >&2
    echo "The failed reads are in $REJECT_DIR/$rel for a listen." >&2
    echo "Clean the disc or try another drive, then run rip-cd.sh again: it only" >&2
    echo "rips what's missing." >&2
    exit 5
fi
echo "All done. Insert the next disc and run rip-cd.sh again."
}

main "$@"
exit
