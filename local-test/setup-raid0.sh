#!/usr/bin/env bash
# Stripe the two EC2 NVMe instance-store disks into a single ~13.6TB RAID0
# volume mounted at /data. Instance storage is ephemeral: everything on
# /data is lost when the instance stops or terminates.
set -euxo pipefail

DISKS=(/dev/nvme1n1 /dev/nvme2n1)

# Abort if either disk already has a filesystem/signature
for d in "${DISKS[@]}"; do
    if blkid "$d" >/dev/null 2>&1; then
        echo "ERROR: $d already has a signature, refusing to wipe" >&2
        exit 1
    fi
done

apt-get install -y mdadm >/dev/null 2>&1 || true

mdadm --create /dev/md0 --level=0 --raid-devices=2 --run "${DISKS[@]}"
mkfs.ext4 -E nodiscard -m 0 /dev/md0
mkdir -p /data
mount -o noatime /dev/md0 /data
chown "$SUDO_USER:$SUDO_USER" /data

df -h /data
