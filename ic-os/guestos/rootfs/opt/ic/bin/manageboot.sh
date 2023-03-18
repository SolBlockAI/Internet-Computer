#!/bin/bash

set -e

# Reads properties "boot_alternative" and "boot_cycle" from the grubenv
# file. The properties are stored as global variables.
#
# Arguments:
# $1 - name of grubenv file
function read_grubenv() {
    local GRUBENV_FILE="$1"

    while IFS="=" read -r key value; do
        case "$key" in
            '#'*) ;;
            'boot_alternative') ;&
            'boot_cycle')
                eval "$key=\"$value\""
                ;;
            *) ;;

        esac
    done <"$GRUBENV_FILE"
}

# Writes "boot_alternative" and "boot_cycle" global variables to grubenv file
#
# Arguments:
# $1 - name of grubenv file
function write_grubenv() {
    local GRUBENV_FILE="$1"

    TMP_FILE=$(mktemp /tmp/grubenv-XXXXXXXXXXXX)
    (
        echo "# GRUB Environment Block"
        echo boot_alternative="$boot_alternative"
        echo boot_cycle="$boot_cycle"
        # Fill to make sure we will have 1024 bytes
        echo -n "################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################################"
    ) >"${TMP_FILE}"
    # Truncate to arrive at precisely 1024 bytes
    truncate --size=1024 "${TMP_FILE}"
    cat "${TMP_FILE}" >"${GRUBENV_FILE}"
    rm "${TMP_FILE}"
}

# Convert A -> B and B -> A
function swap_alternative() {
    if [ "$1" == B ]; then
        echo A
    else
        echo B
    fi
}

function get_boot_partition() {
    if [ "$1" == B ]; then
        echo /dev/vda8
    else
        echo /dev/vda4
    fi
}
function get_root_partition() {
    if [ "$1" == B ]; then
        echo /dev/vda9
    else
        echo /dev/vda5
    fi
}
function get_var_partition() {
    if [ "$1" == B ]; then
        echo /dev/vda10
    else
        echo /dev/vda6
    fi
}

function usage() {
    cat <<EOF
Usage:
  manageboot.sh [ -f grubenvfile] action

  -f specify alternative grubenv file (defaults to /boot/grub/grubenv).
     Primarily useful for testing

  Action is one of

    upgrade-install
      Installs a new system image into the spare partition and changes
      bootloader to use it on next boot. (Caller will still have to
      reboot on next opportunity).
      The new system image can be given in two different ways:
      - as a singe .tar (or .tar.gz) file containing two files
        named "boot.img" and "root.img"
      - as two filenames: first the "boot" partition
        image, then the "root" partition image.
      The update is written to the partitions, but the bootloader is
      not changed yet, see next command.

    upgrade-commit
      Commits a previously installed upgrade by writing instructions to the
      bootloader to switch to new system after reboot, and also triggers
      reboot immediately.
      This must be called after upgrade-install command above finished
      successfully. Calling it under any other circumstances is illegal and
      will result in a wrong (possibly failing) boot.

    confirm
      Confirm that the current system booted fine (required after first
      boot of newly installed upgrade to prevent bootloader falling back
      to previous installation). This simply does nothing if the system
      has been confirmed previously already (safe to call as many times
      as you like, will not iniate I/O if nothing to be written).

    current
      Output currently booted system (A or B) on stdout and exit.

    target
      Output target system for incoming upgrade (A or B) on stdout and exit.

    next
      Output system (A or B) to be booted next. This may either be the
      same one as running now (if present system is "stable"), or it may
      be the other system (if newly upgraded system has not "confirmed"
      yet).

    current_root
      Output root partition of currently running system.

    target_root
      Output target root partition for incoming upgrade on stdout and exit.
EOF
}

# Parsing options first
GRUBENV_FILE=/boot/grub/grubenv
while getopts ":f" OPT; do
    case "${OPT}" in
        f)
            GRUBENV_FILE="${OPTARG}"
            ;;
        *)
            usage >&2
            exit 1
            ;;
    esac
done

# Read current state
read_grubenv "${GRUBENV_FILE}"

CURRENT_ALTERNATIVE="${boot_alternative}"
NEXT_BOOT="${CURRENT_ALTERNATIVE}"
IS_STABLE=1
if [ "${boot_cycle}" == "first_boot" ]; then
    # If the next system to be booted according to bootloader has never been
    # booted yet, then we must still be in the other system.
    CURRENT_ALTERNATIVE=$(swap_alternative "${CURRENT_ALTERNATIVE}")
    IS_STABLE=0
fi

if [ "${boot_cycle}" == "failsafe_check" ]; then
    # If the system booted is marked as "failsafe_check" then bootloader
    # will revert to the other system on next boot.
    NEXT_BOOT=$(swap_alternative "${NEXT_BOOT}")
fi

TARGET_ALTERNATIVE=$(swap_alternative "${CURRENT_ALTERNATIVE}")
CURRENT_ROOT=$(get_root_partition "${CURRENT_ALTERNATIVE}")
TARGET_ROOT=$(get_root_partition "${TARGET_ALTERNATIVE}")
CURRENT_BOOT=$(get_boot_partition "${CURRENT_ALTERNATIVE}")
TARGET_BOOT=$(get_boot_partition "${TARGET_ALTERNATIVE}")
CURRENT_VAR=$(get_var_partition "${CURRENT_ALTERNATIVE}")
TARGET_VAR=$(get_var_partition "${TARGET_ALTERNATIVE}")

# Execute subsequent action
ACTION="$1"
case "${ACTION}" in
    upgrade-install) ;&

    upgrade-commit) ;&

    confirm)
        # Re-execute script as root (unless root already) for operations that
        # require privilege.
        if [ $(id -u) != 0 ]; then
            exec sudo "$0" "$@"
        fi
        ;;
    *) ;;

esac

shift

case "${ACTION}" in
    upgrade-install)
        if [ "${IS_STABLE}" != 1 ]; then
            echo "Cannot install an upgrade before present system is committed as stable." >&2
            exit 1
        fi

        if [ "$#" == 2 ]; then
            BOOT_IMG="$1"
            ROOT_IMG="$2"
        elif [ "$#" == 1 ]; then
            TMPDIR=$(mktemp -d -t upgrade-image-XXXXXXXXXXXX)
            trap "rm -rf $TMPDIR" exit
            tar -xaf "$1" -C "${TMPDIR}"
            BOOT_IMG="${TMPDIR}"/boot.img
            ROOT_IMG="${TMPDIR}"/root.img
        else
            usage >&2
            exit 1
        fi

        # Write to target partitions, and "wipe" header of var partition
        # (to ensure that new target starts from a pristine state).
        dd if="${BOOT_IMG}" of="${TARGET_BOOT}" bs=1M status=progress
        dd if="${ROOT_IMG}" of="${TARGET_ROOT}" bs=1M status=progress
        dd if=/dev/zero of="${TARGET_VAR}" bs=1M count=16 status=progress
        ;;
    upgrade-commit)
        if [ "${IS_STABLE}" != 1 ]; then
            echo "Cannot install an upgrade before present system is committed as stable." >&2
            exit 1
        fi

        # Tell boot loader to switch partitions on next boot.
        boot_alternative="${TARGET_ALTERNATIVE}"
        boot_cycle=first_boot
        write_grubenv "${GRUBENV_FILE}"
        sync
        reboot
        ;;
    confirm)
        if [ "$boot_cycle" != "stable" ]; then
            boot_cycle=stable
            write_grubenv "${GRUBENV_FILE}"
        fi
        ;;
    current)
        echo "${CURRENT_ALTERNATIVE}"
        ;;
    target)
        echo "${TARGET_ALTERNATIVE}"
        ;;
    next)
        echo "${NEXT_BOOT}"
        ;;
    current_partition)
        echo "${CURRENT_ROOT}"
        ;;
    target_partition)
        echo "${TARGET_ROOT}"
        ;;
    *)
        usage
        ;;
esac
