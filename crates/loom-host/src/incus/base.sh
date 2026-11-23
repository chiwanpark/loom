#!/bin/bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

os_value() {
	sed -n "s/^$1=//p" /etc/os-release | tr -d '"'
}
ID=$(os_value ID)
VERSION_CODENAME=$(os_value VERSION_CODENAME)

case "$ID" in
debian | ubuntu) ;;
*)
	echo "the base image must be Debian or Ubuntu, not $ID" >&2
	exit 1
	;;
esac

for _ in $(seq 1 60); do
	if getent ahosts deb.debian.org >/dev/null 2>&1 || [ -n "${HTTP_PROXY:-${http_proxy:-}}" ]; then
		break
	fi
	sleep 1
done

apt-get update -q
apt-get install -y -q --no-install-recommends \
	ca-certificates curl git gh gnupg less openssh-client procps sudo
update-ca-certificates

install -m 0755 -d /etc/apt/keyrings
curl -fsSL "https://download.docker.com/linux/$ID/gpg" -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/$ID $VERSION_CODENAME stable" \
	>/etc/apt/sources.list.d/docker.list
apt-get update -q
apt-get install -y -q docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

existing=$(getent passwd 1000 | cut -d: -f1 || true)
if [ -n "$existing" ] && [ "$existing" != dev ]; then
	usermod -l dev -d /home/dev -m "$existing"
	groupmod -n dev "$(id -gn dev)" || true
fi
if ! id dev >/dev/null 2>&1; then
	useradd -m -u 1000 -U -s /bin/bash dev
fi
usermod -aG sudo,docker dev
echo 'dev ALL=(ALL) NOPASSWD:ALL' >/etc/sudoers.d/dev
chmod 0440 /etc/sudoers.d/dev
install -d -o dev -g dev -m 0755 /workspaces /home/dev/.config
install -d -o dev -g dev -m 0700 /home/dev/.config/loom

if [ -s /etc/loom/environment ]; then
	cat /etc/loom/environment >>/etc/environment
	if grep -qiE '^(https?|no)_proxy=' /etc/loom/environment; then
		mkdir -p /etc/systemd/system/docker.service.d
		{
			echo '[Service]'
			grep -iE '^(https?|no)_proxy=' /etc/loom/environment | sed 's/^/Environment=/'
		} >/etc/systemd/system/docker.service.d/loom-proxy.conf
	fi
fi

systemctl enable docker.service
