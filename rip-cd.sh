#!/usr/bin/env bash
# rip-cd.sh - one command to:
#   1. rip the CD in the drive to FLAC with whipper (paranoia + AccurateRip)
#   2. tag it from MusicBrainz and embed the cover art
#   3. make Opus copies in the phone folder (via flac2phone.sh)
#   4. eject the disc
#
# Usage:
#   rip-cd.sh setup        # first time only: analyze the drive and find its offset
#   rip-cd.sh              # rip the disc in the drive
#   rip-cd.sh --unknown    # rip a disc MusicBrainz doesn't know (generic names)
#   rip-cd.sh -R <mbid>    # use this exact MusicBrainz release
#
# Change the folders/device below, or override them when running, e.g.
#   PHONE_DIR=~/Music/phone rip-cd.sh
#   COUNTRY=GB rip-cd.sh   # prefer a UK edition (default US; COUNTRY= for any)
#
# If several MusicBrainz editions match, US ones are preferred; if none is US,
# the rip is retried with any country. A track that can't be read reliably is
# skipped (whipper --keep-going) so the rest of the disc still gets ripped.
# If the TOC read fails, it's retried once; cdrdao's own messages are kept in
# ~/.cache/rip-cd/cdrdao.log (whipper hides them).
#
# Needs: whipper (sudo dnf install whipper; podman container used as a
# fallback), opus-tools, and flac2phone.sh in the
# same folder as this script.

set -euo pipefail

# Everything is inside main(), which bash reads completely before running it.
# That way, updating this file while a rip is running can't break that rip.
main() {

# Find the Apple SuperDrive (USB 05ac:1500) by its USB ID, so we never pick
# the laptop's built-in DVD drive (/dev/sr0) by mistake.
find_superdrive() {
    local dev
    for dev in /dev/sr[0-9]*; do
        [[ -b $dev ]] || continue
        if udevadm info --query=property --name="$dev" 2>/dev/null |
                grep -qx 'ID_VENDOR_ID=05ac'; then
            echo "$dev"; return 0
        fi
    done
    return 1
}

if [[ -z ${DEVICE:-} ]]; then
    DEVICE=$(find_superdrive) || {
        echo "SuperDrive not found. Plug it into a USB 3.0 port on the laptop," >&2
        echo "or set DEVICE=/dev/srN to use another drive." >&2
        exit 1
    }
fi
FLAC_DIR=${FLAC_DIR:-$HOME/Music/FLAC}
PHONE_DIR=${PHONE_DIR:-$HOME/Music/phone}
OPUS_BITRATE=${OPUS_BITRATE:-160}
COUNTRY=${COUNTRY-US}

CONFIG_DIR=$HOME/.config/whipper
CACHE_DIR=$HOME/.cache/rip-cd
CDRDAO_LOG=$CACHE_DIR/cdrdao.log
IMAGE=docker.io/whipperteam/whipper
FLAC2PHONE="$(dirname "$(realpath "$0")")/flac2phone.sh"

mkdir -p "$FLAC_DIR" "$PHONE_DIR" "$CONFIG_DIR" "$CACHE_DIR"

# Scratch space for this run; also marks the start of this rip, so we can
# find the album folder it creates
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
STAMP=$WORK/stamp
touch "$STAMP"

# Use the Fedora whipper package if installed (sudo dnf install whipper);
# otherwise fall back to the official container image via rootless podman.
if command -v whipper >/dev/null; then
    # whipper runs "cdrdao" from PATH and throws away its error messages.
    # Put a wrapper first in PATH that keeps a copy of them in $CDRDAO_LOG.
    if REAL_CDRDAO=$(command -v cdrdao); then
        mkdir -p "$WORK/bin"
        cat > "$WORK/bin/cdrdao" <<EOF
#!/usr/bin/env bash
echo "== \$(date '+%F %T') cdrdao \$*" >> "$CDRDAO_LOG"
"$REAL_CDRDAO" "\$@" 2> >(tee -a "$CDRDAO_LOG" >&2)
rc=\$?
wait \$! 2>/dev/null
echo "== cdrdao exit code \$rc" >> "$CDRDAO_LOG"
exit \$rc
EOF
        chmod +x "$WORK/bin/cdrdao"
        : > "$CDRDAO_LOG"
    fi
    WHIPPER=(env PATH="$WORK/bin:$PATH" whipper)
    OUT_DIR=$FLAC_DIR
else
    echo "(whipper not installed; using container image. 'sudo dnf install whipper' is simpler.)"
    WHIPPER=(podman run --rm -it --userns=keep-id --group-add keep-groups
             --device "$DEVICE"
             -v "$CONFIG_DIR":/home/worker/.config/whipper:z
             -v "$FLAC_DIR":/output:z
             "$IMAGE")
    OUT_DIR=/output
fi
whipper() { "${WHIPPER[@]}" "$@"; }

echo "Using drive: $DEVICE"

if [[ ${1:-} == setup ]]; then
    echo "== Analyzing drive (insert any audio CD first) =="
    whipper drive analyze -d "$DEVICE"
    echo
    echo "== Finding read offset (use a popular, commonly owned CD) =="
    whipper offset find -d "$DEVICE"
    echo
    echo "Setup done. Run rip-cd.sh to rip a disc."
    exit 0
fi

[[ -x $FLAC2PHONE ]] || { echo "flac2phone.sh not found next to this script: $FLAC2PHONE" >&2; exit 1; }
command -v opusenc >/dev/null || { echo "opusenc not found: sudo dnf install opus-tools" >&2; exit 1; }

# Folder layout: FLAC_DIR/Artist/Album/NN. Artist - Title.flac
# (whipper's default puts everything under an extra "album/" folder)
TRACK_TEMPLATE='%A/%d/%t. %a - %n'
DISC_TEMPLATE='%A/%d/%A - %d'

ARGS=("$@")
# true if the user passed any of the given options
has_arg() {
    local a o
    for a in "${ARGS[@]}"; do
        for o in "$@"; do
            [[ $a == "$o" || $a == "$o="* ]] && return 0
        done
    done
    return 1
}

# Prefer editions from $COUNTRY, unless the user already picked a release
COUNTRY_ARGS=()
if [[ -n $COUNTRY ]] && ! has_arg -R --release-id -c --country -U --unknown; then
    COUNTRY_ARGS=(--country "$COUNTRY")
fi

OUT=$WORK/whipper.out
# One whipper run; its output is shown and also saved in $OUT.
# Exit code 5 means it finished but skipped tracks it couldn't read.
rip_once() {
    whipper cd -d "$DEVICE" rip --output-directory "$OUT_DIR" \
        --track-template "$TRACK_TEMPLATE" --disc-template "$DISC_TEMPLATE" \
        --cover-art complete --keep-going "$@" 2>&1 | tee "$OUT"
    return "${PIPESTATUS[0]}"
}

show_cdrdao_log() {
    [[ -s $CDRDAO_LOG ]] || return 0
    echo "-- last cdrdao messages (full copy: $CDRDAO_LOG) --"
    tr '\r' '\n' < "$CDRDAO_LOG" | grep -v '^[[:space:]]*$' | tail -15
}

echo "== Ripping $DEVICE to $FLAC_DIR =="
toc_retried=false
while :; do
    rc=0
    rip_once "${COUNTRY_ARGS[@]}" "${ARGS[@]}" || rc=$?
    (( rc == 0 || rc == 5 )) && break

    if ! $toc_retried && grep -q 'cdrdao\.read-toc' "$OUT"; then
        toc_retried=true
        echo
        echo "== Reading the disc's table of contents failed; trying once more =="
        show_cdrdao_log
        sleep 10
        continue
    fi
    if (( ${#COUNTRY_ARGS[@]} )) && grep -q 'was not released in' "$OUT" &&
            grep -q 'unable to retrieve disc metadata' "$OUT"; then
        echo
        echo "== No $COUNTRY edition of this disc in MusicBrainz; retrying with any country =="
        COUNTRY_ARGS=()
        continue
    fi
    break
done

if (( rc != 0 && rc != 5 )); then
    echo
    grep -q 'cdrdao\.read-toc' "$OUT" && show_cdrdao_log
    echo "Rip failed or was incomplete. Check the messages above and the .log file in $FLAC_DIR." >&2
    if grep -q 'unable to retrieve disc metadata' "$OUT"; then
        echo "MusicBrainz doesn't know this disc; try: rip-cd.sh --unknown" >&2
    fi
    exit 1
fi
SKIPPED=$(grep -oP 'track \K[0-9]+(?= failed to rip)' "$OUT" | paste -sd, - || true)

# whipper silently skips the cover if the Cover Art Archive has a hiccup
# (it happened on the first rip), so check and fetch it ourselves if needed.
LOG=$(find "$FLAC_DIR" -name '*.log' -newer "$STAMP" -print -quit)
if [[ -n $LOG ]]; then
    ALBUM_DIR=$(dirname "$LOG")
    MBID=$(grep -oP 'musicbrainz.org/release/\K[0-9a-f-]{36}' "$LOG" | head -1 || true)
    if [[ ! -s $ALBUM_DIR/cover.jpg && -n $MBID ]]; then
        echo
        echo "== Cover art missing; fetching it again =="
        for try in 1 2 3; do
            curl -fsSL -o "$ALBUM_DIR/cover.jpg" \
                "https://coverartarchive.org/release/$MBID/front-500" && break
            rm -f "$ALBUM_DIR/cover.jpg"; sleep 5
        done
        if [[ -s $ALBUM_DIR/cover.jpg ]]; then
            for f in "$ALBUM_DIR"/*.flac; do
                metaflac --remove --block-type=PICTURE "$f"
                metaflac --import-picture-from="$ALBUM_DIR/cover.jpg" "$f"
            done
            echo "Cover art embedded."
        else
            echo "Couldn't get cover art (this album may have none on the Cover Art Archive)." >&2
        fi
    fi
fi

echo
echo "== Making Opus copies in $PHONE_DIR =="
"$FLAC2PHONE" "$FLAC_DIR" "$PHONE_DIR" opus "$OPUS_BITRATE"

echo
echo "== Ejecting =="
eject "$DEVICE" || echo "Couldn't eject automatically; use the Disks & Devices popup."

echo
if [[ -n $SKIPPED ]]; then
    echo "Done, BUT these tracks couldn't be read reliably and are missing: $SKIPPED" >&2
    echo "Clean the disc and re-rip it; the other tracks were ripped normally." >&2
    exit 5
fi
echo "All done. Insert the next disc and run rip-cd.sh again."
}

main "$@"
exit
