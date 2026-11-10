#!/bin/bash
# Full eMMC backup of the U60 Pro over ADB: mmcblk0 (~7.6 GB) plus the two 4 MB boot areas.
# Also saves the GPT partition listing and key UCI config so the firmware state is on record.
set -u
OUT="${1:-./backup-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT"
adb devices | grep -qw device || { echo "no adb device; run ./01_enable_adb.sh first" >&2; exit 1; }

echo "== device identity"
adb shell 'uci get zwrt_common_info.common_config.integrate_version; uci get zwrt_common_info.common_config.hardware_version; cat /etc/adb_devid 2>/dev/null' | tee "$OUT/identity.txt"

echo "== partition table and mounts"
adb shell 'cat /proc/partitions; echo; ls -l /dev/block/bootdevice/by-name/ 2>/dev/null; echo; mount; echo; df -h' > "$OUT/partitions.txt"
adb shell 'uci show zwrt_common_info; uci show zwrt_product; uci show zwrt_fota_vd; uci show zwrt_tr069; uci show zwrt_mqtt; uci show zwrt_smart_mng' > "$OUT/uci-key.txt" 2>&1
adb shell 'uci export' > "$OUT/uci-export-all.txt" 2>&1

echo "== device-side checksums (takes a few minutes)"
adb shell 'md5sum /dev/mmcblk0 /dev/mmcblk0boot0 /dev/mmcblk0boot1' | tee "$OUT/device-md5.txt"

for dev in mmcblk0boot0 mmcblk0boot1 mmcblk0; do
  echo "== dumping /dev/$dev -> $OUT/$dev.img"
  adb exec-out "dd if=/dev/$dev bs=4M 2>/dev/null" > "$OUT/$dev.img"
  ls -l "$OUT/$dev.img"
done

echo "== verifying local checksums against device"
( cd "$OUT" && md5 -q mmcblk0.img mmcblk0boot0.img mmcblk0boot1.img ) > "$OUT/local-md5.txt"
echo "device: "; cat "$OUT/device-md5.txt"
echo "local:  "; paste "$OUT/local-md5.txt" <(printf 'mmcblk0.img\nmmcblk0boot0.img\nmmcblk0boot1.img\n')
file "$OUT/mmcblk0.img"
echo "Backup in: $OUT"
