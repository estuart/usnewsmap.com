#!/usr/bin/env bash
# Create an AlmaLinux 9 VM for an on-prem OCR node (#296) on a Proxmox host.
# Run on the Proxmox host as root (copy it there first); then run
# scripts/ocr-node/onboard.sh from your machine (docs/operations.md,
# "On-prem OCR nodes"):
#
#   bash proxmox-create-vm.sh --vmid 120 --name usnm-ocr-node1 \
#     --ssh-key 'ssh-ed25519 AAAA... you@example'
#
# Optional: --cores 4 --memory-mib 8192 --disk-gib 64 --storage local-lvm
#           --bridge vmbr0 --snippets-storage local
#
# Uses AlmaLinux's GenericCloud image (checksum verified) and cloud-init:
# UEFI with Secure Boot (Microsoft keys pre-enrolled; AlmaLinux's shim is
# signed) and a vTPM, CPU type host (AVX2 for OCR), qemu guest agent, an
# admin user "claude" with key-only login and passwordless sudo, DHCP,
# start at boot.
set -euo pipefail

VMID="" NAME="" SSH_KEY=""
CORES=4 MEMORY_MIB=8192 DISK_GIB=64 STORAGE=local-lvm BRIDGE=vmbr0 SNIPPETS=local
usage() { sed -n '2,18s/^# \{0,1\}//p' "$0" >&2; exit 2; }
while [ $# -gt 0 ]; do
  case "$1" in
    --vmid) VMID=$2; shift 2 ;;
    --name) NAME=$2; shift 2 ;;
    --ssh-key) SSH_KEY=$2; shift 2 ;;
    --cores) CORES=$2; shift 2 ;;
    --memory-mib) MEMORY_MIB=$2; shift 2 ;;
    --disk-gib) DISK_GIB=$2; shift 2 ;;
    --storage) STORAGE=$2; shift 2 ;;
    --bridge) BRIDGE=$2; shift 2 ;;
    --snippets-storage) SNIPPETS=$2; shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$VMID" ] && [ -n "$NAME" ] && [ -n "$SSH_KEY" ] || usage
# The name becomes a file name, the hostname and part of the cloud-init
# document, so it must be a plain DNS label (as Proxmox and onboard.sh want).
[[ $NAME =~ ^[A-Za-z0-9]([A-Za-z0-9-]{0,52}[A-Za-z0-9])?$ ]] ||
  { echo "--name: 1 to 54 letters, digits or hyphens, not starting or ending with a hyphen" >&2; exit 1; }
[[ $VMID =~ ^[1-9][0-9]{2,8}$ ]] || { echo "--vmid: a number from 100" >&2; exit 1; }
[[ $SSH_KEY != *$'\n'* ]] || { echo "--ssh-key: one line" >&2; exit 1; }
[ "$(id -u)" = 0 ] || { echo "run as root on the Proxmox host" >&2; exit 1; }
command -v qm >/dev/null || { echo "qm not found: not a Proxmox host?" >&2; exit 1; }
qm status "$VMID" >/dev/null 2>&1 && { echo "VM $VMID already exists" >&2; exit 1; }
pvesm status --storage "$STORAGE" >/dev/null || { echo "no storage $STORAGE" >&2; exit 1; }
case "$SSH_KEY" in ssh-ed25519\ *|ssh-rsa\ *|ecdsa-sha2-*) ;; *) echo "--ssh-key must be a public key line" >&2; exit 1 ;; esac

# 1. Image, verified against AlmaLinux's published SHA-256 list.
BASE=https://repo.almalinux.org/almalinux/9/cloud/x86_64/images
IMG=AlmaLinux-9-GenericCloud-latest.x86_64.qcow2
CACHE=/var/lib/vz/template/cache
mkdir -p "$CACHE"
curl -fsSL -o "$CACHE/$IMG.CHECKSUM" "$BASE/CHECKSUM"
want=$(grep -F "$IMG" "$CACHE/$IMG.CHECKSUM" | grep -oE '[0-9a-f]{64}' | head -1)
[ -n "$want" ] || { echo "no checksum for $IMG" >&2; exit 1; }
if [ ! -f "$CACHE/$IMG" ] || [ "$(sha256sum "$CACHE/$IMG" | cut -d' ' -f1)" != "$want" ]; then
  echo "downloading $IMG"
  curl -fL -o "$CACHE/$IMG.part" "$BASE/$IMG"
  mv "$CACHE/$IMG.part" "$CACHE/$IMG"
fi
[ "$(sha256sum "$CACHE/$IMG" | cut -d' ' -f1)" = "$want" ] || { echo "checksum mismatch" >&2; exit 1; }
echo "image verified: $want"

# 2. cloud-init user data as a snippet (the storage must allow snippets).
SNIPDIR=$(pvesm path "$SNIPPETS:snippets/x" 2>/dev/null | sed 's#/x$##') || true
if ! pvesm status --storage "$SNIPPETS" --content snippets 2>/dev/null | grep -q "^$SNIPPETS "; then
  content=$(pvesh get "/storage/$SNIPPETS" --output-format json | sed -n 's/.*"content":"\([^"]*\)".*/\1/p')
  echo "enabling snippets on storage $SNIPPETS (was: $content)"
  pvesm set "$SNIPPETS" --content "$content,snippets"
  SNIPDIR=$(pvesm path "$SNIPPETS:snippets/x" | sed 's#/x$##')
fi
mkdir -p "$SNIPDIR"
USERDATA="$NAME-user.yaml"
cat > "$SNIPDIR/$USERDATA" <<EOF
#cloud-config
hostname: $NAME
fqdn: $NAME
preserve_hostname: false
users:
  - name: claude
    gecos: Claude Code automation
    groups: [wheel]
    shell: /bin/bash
    lock_passwd: true
    sudo: ["ALL=(ALL) NOPASSWD:ALL"]
    ssh_authorized_keys:
      - $SSH_KEY
ssh_pwauth: false
disable_root: true
package_update: true
packages: [qemu-guest-agent]
runcmd:
  # cloud-init can leave the hostname as localhost on this image; set it explicitly.
  - [hostnamectl, set-hostname, $NAME]
  - [systemctl, enable, --now, qemu-guest-agent]
EOF

# 3. VM: UEFI + Secure Boot + vTPM, host CPU, virtio disk and NIC, serial
# console, guest agent.
qm create "$VMID" --name "$NAME" --ostype l26 --machine q35 --bios ovmf \
  --efidisk0 "$STORAGE:1,efitype=4m,pre-enrolled-keys=1" \
  --tpmstate0 "$STORAGE:1,version=v2.0" \
  --cpu host --cores "$CORES" --sockets 1 --memory "$MEMORY_MIB" --balloon 0 \
  --net0 "virtio,bridge=$BRIDGE" --scsihw virtio-scsi-single \
  --serial0 socket --vga serial0 --agent enabled=1 --onboot 1 \
  --description "usnewsmap on-prem OCR node (#296)"
qm set "$VMID" --scsi0 "$STORAGE:0,import-from=$CACHE/$IMG,discard=on,ssd=1,iothread=1"
qm resize "$VMID" scsi0 "${DISK_GIB}G"
qm set "$VMID" --ide2 "$STORAGE:cloudinit" --boot order=scsi0 \
  --ipconfig0 ip=dhcp --cicustom "user=$SNIPPETS:snippets/$USERDATA"
qm start "$VMID"

# 4. Wait for the guest agent and print the address.
echo "waiting for the guest agent (first boot installs it, a few minutes)"
for _ in $(seq 1 60); do
  if ip=$(qm guest cmd "$VMID" network-get-interfaces 2>/dev/null |
    grep -oE '"ip-address" *: *"[0-9.]+"' | grep -vE '"127\.' | grep -oE '[0-9.]{7,}' | head -1) && [ -n "$ip" ]; then
    echo "VM $VMID ($NAME) is up at $ip"
    echo "test: ssh claude@$ip 'sudo -n true && echo ok'"
    exit 0
  fi
  sleep 10
done
echo "VM started, but the guest agent didn't report an address in 10 minutes; check the console: qm terminal $VMID" >&2
exit 1
